//! Loopback-only HTTP API. Database and indexing work runs on blocking workers.
#[cfg(feature = "test-support")]
use crate::planning::FocusedView;
#[cfg(feature = "test-support")]
use crate::store::{EvidenceFence, EvidenceFencePolicy};
use crate::{
    acp::Acp,
    auth::valid_token,
    dependencies::{Catalog, CatalogOptions},
    indexer::IndexOptions,
    jev,
    live_jev::LiveJev,
    model::*,
    planning::{self, QuestionPacket, QuestionPreview, QuestionRequest, SelectionEnvelope},
    store::Store,
};
use anyhow::Context;
use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
#[cfg(feature = "test-support")]
use axum::{
    extract::DefaultBodyLimit,
    routing::{post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::{
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexJob {
    pub id: String,
    pub state: String,
    pub progress: IndexProgress,
    pub revision: Option<IndexPin>,
    pub error: Option<Value>,
    pub submitted_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}
struct Jobs {
    current: Option<String>,
    jobs: BTreeMap<String, IndexJob>,
    cancel: CancelFlag,
}
const MAX_PACKETS: usize = 8;
const MAX_PACKET_BYTES: usize = 1024 * 1024;
const MAX_CACHE_BYTES: usize = 8 * MAX_PACKET_BYTES;
#[derive(Default, Clone)]
struct PacketCache {
    packets: VecDeque<(Arc<QuestionPacket>, usize)>,
    bytes: usize,
}
impl PacketCache {
    fn remember(&mut self, packet: Arc<QuestionPacket>, bytes: usize) {
        if let Some(index) = self
            .packets
            .iter()
            .position(|(p, _)| p.packet_id == packet.packet_id)
        {
            self.bytes -= self.packets.remove(index).unwrap().1;
        }
        while self.packets.len() >= MAX_PACKETS || self.bytes + bytes > MAX_CACHE_BYTES {
            self.bytes -= self.packets.pop_front().unwrap().1;
        }
        self.bytes += bytes;
        self.packets.push_back((packet, bytes));
    }
    // The final fence runs after admission while this mutex is held. A failed
    // response cannot evict an earlier packet or replace its same-ID entry.
    fn remember_fenced(
        &mut self,
        packet: Arc<QuestionPacket>,
        bytes: usize,
        finish: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let prior = self.clone();
        self.remember(packet, bytes);
        if let Err(error) = finish() {
            *self = prior;
            return Err(error);
        }
        Ok(())
    }
}
struct DependencyIndex {
    generation: u64,
    cancel: Arc<AtomicBool>,
    stopped: bool,
    worker_running: bool,
    state: &'static str,
    catalog: Option<Arc<Catalog>>,
    warnings: Vec<String>,
}
impl DependencyIndex {
    /// Replace the pending generation, but admit at most one worker driver.
    fn request_build(&mut self) -> bool {
        if self.stopped {
            return false;
        }
        self.cancel.store(true, Ordering::Release);
        self.generation += 1;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.state = "loading";
        self.catalog = None;
        self.warnings.clear();
        !std::mem::replace(&mut self.worker_running, true)
    }
}
#[derive(Debug)]
struct GcPriorityYield;
impl std::fmt::Display for GcPriorityYield {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("maintenance deferred for publication")
    }
}
impl std::error::Error for GcPriorityYield {}

#[derive(Default)]
struct MaintenanceTelemetry {
    preemptions: u64,
    busy_attempts: u64,
    successful_units: u64,
    deferred_since: Option<Instant>,
    max_deferred_ms: u128,
    max_deferred_age_s: u64,
}
/// One bounded notice per minute, even when a worker fails at every 20 ms tick.
#[derive(Default)]
struct MaintenanceErrorLimiter {
    last_notice: Option<Instant>,
}
impl MaintenanceErrorLimiter {
    fn permit(&mut self, now: Instant) -> bool {
        if self
            .last_notice
            .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(60))
        {
            return false;
        }
        self.last_notice = Some(now);
        true
    }
}

/// Never render arbitrary anyhow context, SQLite messages, source, or paths.
/// SQLite extended result codes and OS errno are numeric, bounded categories.
fn maintenance_error_class(error: &anyhow::Error) -> (&'static str, i32) {
    for cause in error.chain() {
        if let Some(sqlite) = cause.downcast_ref::<rusqlite::Error>() {
            return match sqlite {
                rusqlite::Error::SqliteFailure(code, _) => ("sqlite", code.extended_code),
                _ => ("sqlite", 0),
            };
        }
    }
    if error.is::<crate::store::SqliteContention>() {
        return ("sqlite_contention", 0);
    }
    if let Some(io) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
    {
        return ("io", io.raw_os_error().unwrap_or(0));
    }
    ("other", 0)
}
fn maintenance_error_notice(category: &'static str, code: i32) -> String {
    format!("maintenance tick failed: category={category} code={code}\n")
}

impl MaintenanceTelemetry {
    fn observe_due_age(&mut self, age: Option<u64>) {
        if let Some(age) = age {
            self.max_deferred_age_s = self.max_deferred_age_s.max(age.saturating_sub(900));
        }
    }
    fn deferred(&mut self, now: Instant) -> (u128, u128) {
        self.preemptions = self.preemptions.saturating_add(1);
        let age = now
            .duration_since(*self.deferred_since.get_or_insert(now))
            .as_millis();
        self.max_deferred_ms = self.max_deferred_ms.max(age);
        (age, self.max_deferred_ms)
    }
    fn progressed(&mut self, now: Instant) -> (u128, u128) {
        self.successful_units = self.successful_units.saturating_add(1);
        let age = self
            .deferred_since
            .take()
            .map_or(0, |start| now.duration_since(start).as_millis());
        self.max_deferred_ms = self.max_deferred_ms.max(age);
        (age, self.max_deferred_ms)
    }
}

pub struct DaemonState {
    store: Store,
    serving_session: Mutex<Option<Arc<crate::store::topology::WriterSession>>>,
    options: IndexOptions,
    browser: crate::file_tree::SourceDir,
    rust_sources: Vec<crate::rust_sources::Root>,
    dependency_options: Option<CatalogOptions>,
    dependencies: Mutex<DependencyIndex>,
    #[cfg(all(test, feature = "test-support"))]
    dependency_capture_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(all(test, feature = "test-support"))]
    outer_fence_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(all(test, feature = "test-support"))]
    preview_finish_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(feature = "test-support")]
    token: String,
    #[cfg(feature = "test-support")]
    hosts: Vec<String>,
    #[cfg(feature = "test-support")]
    origins: Vec<String>,
    jobs: Mutex<Jobs>,
    queue_tick_started: AtomicBool,
    checkout_runtime_stopped: AtomicBool,
    #[cfg(feature = "test-support")]
    checkout_h_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    maintenance_tick_started: AtomicBool,
    maintenance_telemetry: Mutex<MaintenanceTelemetry>,
    maintenance_error_limiter: Mutex<MaintenanceErrorLimiter>,
    #[cfg(test)]
    #[cfg(test)]
    test_pending_read_failures: AtomicUsize,
    #[cfg(test)]
    test_activation_head_busy_once: AtomicBool,
    pending_requests: Mutex<Vec<String>>,
    job_progress: Mutex<BTreeMap<String, IndexProgress>>,
    native_stream: Mutex<()>,
    maintenance_stream: Mutex<()>,
    watcher_work: Mutex<
        Option<(
            std::sync::Weak<crate::store::topology::WriterSession>,
            crate::index_coordinator::WatcherWork,
        )>,
    >,
    causal_witness: Mutex<Option<Arc<crate::daemon::causal_witness::CausalWitness>>>,
    causal_runtime: Mutex<Option<std::sync::Weak<crate::daemon::registry::CheckoutRuntime>>>,
    causal_lineage_ordinal: Arc<AtomicU64>,
    causal_h_reported: Mutex<Option<(uuid::Uuid, uuid::Uuid, u64)>>,
    recovery_retry_after: Mutex<Option<Instant>>,
    idle_h_retry: AtomicBool,
    retention_last_run: Mutex<Instant>,
    gc_last_check: Mutex<Instant>,
    #[cfg(test)]
    test_queue_before_stream: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_after_stream: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_after_pending_snapshot: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_after_h_drain: crate::store::TestOneShotHook,
    packets: Mutex<PacketCache>,
    provider: Option<Arc<LiveJev>>,
    acp: Option<Arc<Acp>>,
}
/// Diagnostic logging must never kill the durable queue worker if the
/// launching parent closed its stderr pipe after the server banner.
fn best_effort_queue_stderr(mut output: impl std::io::Write, args: std::fmt::Arguments<'_>) {
    let _ = output.write_fmt(args);
}

pub fn new(
    store: Store,
    index_options: IndexOptions,
    token: String,
    address: SocketAddr,
) -> anyhow::Result<Arc<DaemonState>> {
    new_with_jev(store, index_options, token, address, None)
}
pub fn new_with_jev(
    store: Store,
    index_options: IndexOptions,
    token: String,
    address: SocketAddr,
    provider: Option<Arc<LiveJev>>,
) -> anyhow::Result<Arc<DaemonState>> {
    new_with_providers(store, index_options, token, address, provider, None)
}
pub fn new_with_providers(
    store: Store,
    index_options: IndexOptions,
    token: String,
    address: SocketAddr,
    jev: Option<Arc<LiveJev>>,
    acp: Option<Arc<Acp>>,
) -> anyhow::Result<Arc<DaemonState>> {
    let browse_root = index_options.workspace_root.clone();
    new_with_browser_root(store, index_options, token, address, jev, acp, browse_root)
}
pub fn new_with_browser_root(
    store: Store,
    index_options: IndexOptions,
    token: String,
    address: SocketAddr,
    jev: Option<Arc<LiveJev>>,
    acp: Option<Arc<Acp>>,
    browse_root: std::path::PathBuf,
) -> anyhow::Result<Arc<DaemonState>> {
    new_with_source_roots(
        store,
        index_options,
        token,
        address,
        jev,
        acp,
        browse_root,
        vec![],
    )
}
#[allow(clippy::too_many_arguments)]
pub fn new_with_source_roots(
    store: Store,
    index_options: IndexOptions,
    token: String,
    address: SocketAddr,
    jev: Option<Arc<LiveJev>>,
    acp: Option<Arc<Acp>>,
    browse_root: std::path::PathBuf,
    source_roots: Vec<(String, std::path::PathBuf)>,
) -> anyhow::Result<Arc<DaemonState>> {
    new_with_dependency_options(
        store,
        index_options,
        token,
        address,
        jev,
        acp,
        browse_root,
        source_roots,
        None,
    )
}
/// Catalog roots are trusted daemon configuration, never request parameters.
#[allow(clippy::too_many_arguments)]
pub fn new_with_dependency_options(
    store: Store,
    index_options: IndexOptions,
    token: String,
    address: SocketAddr,
    jev: Option<Arc<LiveJev>>,
    acp: Option<Arc<Acp>>,
    browse_root: std::path::PathBuf,
    source_roots: Vec<(String, std::path::PathBuf)>,
    catalog_options: Option<CatalogOptions>,
) -> anyhow::Result<Arc<DaemonState>> {
    anyhow::ensure!(
        address.ip().is_loopback() && address.port() != 0,
        "daemon requires a bound loopback address"
    );
    anyhow::ensure!(valid_token(&token), "invalid bearer token");
    #[cfg(feature = "test-support")]
    let hosts = vec![address.to_string(), format!("localhost:{}", address.port())];
    #[cfg(feature = "test-support")]
    let origins = hosts.iter().map(|h| format!("http://{h}")).collect();
    Ok(Arc::new(DaemonState {
        store,
        serving_session: Mutex::new(None),
        options: index_options,
        browser: crate::file_tree::SourceDir::open(&browse_root)?,
        rust_sources: crate::rust_sources::open_roots(source_roots)?,
        dependencies: Mutex::new(DependencyIndex {
            generation: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            stopped: false,
            worker_running: false,
            state: if catalog_options.is_some() {
                "loading"
            } else {
                "disabled"
            },
            catalog: None,
            warnings: Vec::new(),
        }),
        #[cfg(all(test, feature = "test-support"))]
        dependency_capture_hook: Mutex::new(None),
        #[cfg(all(test, feature = "test-support"))]
        outer_fence_hook: Mutex::new(None),
        #[cfg(all(test, feature = "test-support"))]
        preview_finish_hook: Mutex::new(None),
        dependency_options: catalog_options,
        #[cfg(feature = "test-support")]
        token,
        #[cfg(feature = "test-support")]
        hosts,
        #[cfg(feature = "test-support")]
        origins,
        provider: jev,
        acp,
        packets: Mutex::new(PacketCache::default()),
        queue_tick_started: AtomicBool::new(false),
        checkout_runtime_stopped: AtomicBool::new(false),
        #[cfg(feature = "test-support")]
        checkout_h_hook: Mutex::new(None),
        maintenance_tick_started: AtomicBool::new(false),
        maintenance_telemetry: Mutex::new(MaintenanceTelemetry::default()),
        maintenance_error_limiter: Mutex::new(MaintenanceErrorLimiter::default()),
        #[cfg(test)]
        #[cfg(test)]
        test_pending_read_failures: AtomicUsize::new(0),
        #[cfg(test)]
        test_activation_head_busy_once: AtomicBool::new(false),
        pending_requests: Mutex::new(Vec::new()),
        job_progress: Mutex::new(BTreeMap::new()),
        native_stream: Mutex::new(()),
        maintenance_stream: Mutex::new(()),
        watcher_work: Mutex::new(None),
        causal_witness: Mutex::new(None),
        causal_runtime: Mutex::new(None),
        causal_lineage_ordinal: Arc::new(AtomicU64::new(0)),
        causal_h_reported: Mutex::new(None),
        recovery_retry_after: Mutex::new(None),
        idle_h_retry: AtomicBool::new(false),
        retention_last_run: Mutex::new(Instant::now()),
        gc_last_check: Mutex::new(Instant::now() - Duration::from_secs(3600)),
        #[cfg(test)]
        test_queue_before_stream: crate::store::TestOneShotHook::default(),
        #[cfg(test)]
        test_queue_after_stream: crate::store::TestOneShotHook::default(),
        #[cfg(test)]
        test_queue_after_pending_snapshot: crate::store::TestOneShotHook::default(),
        #[cfg(test)]
        test_queue_after_h_drain: crate::store::TestOneShotHook::default(),
        jobs: Mutex::new(Jobs {
            current: None,
            jobs: BTreeMap::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        }),
    }))
}
impl DaemonState {
    /// Register a test-only, read-only witness after this checkout activates.
    /// The reverse edge is weak: release may drop the runtime independently.
    pub fn set_causal_witness_runtime(
        &self,
        reporter: Arc<crate::daemon::causal_witness::CausalWitness>,
        runtime: std::sync::Weak<crate::daemon::registry::CheckoutRuntime>,
    ) {
        *self.causal_witness.lock().unwrap() = Some(reporter);
        *self.causal_runtime.lock().unwrap() = Some(runtime);
    }

    /// Called only after queue_tick returned and released its native/work locks.
    fn report_causal_h_ready(self: &Arc<Self>) {
        let runtime = self.causal_runtime.lock().unwrap().clone();
        if let Some(runtime) = runtime.and_then(|runtime| runtime.upgrade()) {
            runtime.report_causal_h_ready(self);
        }
    }

    pub fn causal_lineage_for(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
    ) -> Option<crate::daemon::causal_witness::Lineage> {
        let serving = self.serving_session.lock().unwrap();
        if !serving
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, session))
        {
            return None;
        }
        let work = self.watcher_work.lock().unwrap();
        let (owner, scheduler) = work.as_ref()?;
        owner
            .upgrade()
            .filter(|owner| Arc::ptr_eq(owner, session))?;
        scheduler.causal_lineage()
    }

    pub fn causal_h_already_reported(
        &self,
        lineage: crate::daemon::causal_witness::Lineage,
    ) -> bool {
        self.causal_h_reported.lock().unwrap().as_ref()
            == Some(&(
                lineage.owner_incarnation,
                lineage.watch_epoch,
                lineage.ordinal,
            ))
    }

    pub fn emit_causal_h_ready_once(
        &self,
        lineage: crate::daemon::causal_witness::Lineage,
        pin: IndexPin,
    ) {
        let Some(reporter) = self.causal_witness.lock().unwrap().clone() else {
            return;
        };
        let key = (
            lineage.owner_incarnation,
            lineage.watch_epoch,
            lineage.ordinal,
        );
        let mut reported = self.causal_h_reported.lock().unwrap();
        if reported.as_ref() != Some(&key) {
            reporter.h_ready(lineage, pin);
            *reported = Some(key);
        }
    }

    pub fn retain_serving_session(
        self: &Arc<Self>,
        session: Arc<crate::store::topology::WriterSession>,
    ) {
        // Re-retention must not race an in-flight tick that sampled an old,
        // invalid holder and would replace this verified session afterward.
        {
            let _stream = self.native_stream.lock().unwrap();
            self.replace_serving_session(Some(session));
        }
        self.start_queue_tick();
    }
    /// Stop the native stream before idle release; no queue worker may keep
    /// the old writer or watcher alive after this returns.
    pub fn release_checkout_runtime(&self) {
        self.checkout_runtime_stopped.store(true, Ordering::Release);
        let _stream = self.native_stream.lock().unwrap();
        let _maintenance = self.maintenance_stream.lock().unwrap();
        self.replace_serving_session(None);
    }

    /// A daemon-local writer session is usable only after its H commits.
    /// A stale session cannot admit new claims or selected responses.
    pub fn checkout_serving_owner(&self) -> Option<Arc<crate::store::topology::WriterSession>> {
        let owner = self.serving_session.lock().unwrap().clone()?;
        owner.verify().ok()?;
        Some(owner)
    }

    pub fn checkout_root_loss_retired(&self) -> bool {
        self.store.root_path_replaced().is_ok_and(|lost| lost)
            && self.serving_session.lock().unwrap().is_none()
    }

    /// Deterministically pause local activation before mandatory H.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_checkout_h_hook_for_tests(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.checkout_h_hook.lock().unwrap() = Some(hook);
    }

    /// A pending watcher hint (including the periodic inventory deadline) keeps
    /// the selected checkout in the catching-up state until its tick acknowledges it.
    pub fn checkout_watch_pending(&self) -> bool {
        let pending = {
            let work = self.watcher_work.lock().unwrap();
            work.as_ref().map(|(_, scheduler)| {
                let options = self
                    .store
                    .recorded_index_options()
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| self.options.clone());
                scheduler.accepted_watch_intent(&options)
            })
        };
        pending.unwrap_or_else(|| self.serving_session.lock().unwrap().is_some())
    }

    /// Offline snapshot fixtures opt out of the daemon's queue and maintenance ticks.
    /// See #111 for the separate read-during-reconciliation product fix.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn retain_serving_session_without_tick_for_tests(
        self: &Arc<Self>,
        session: Arc<crate::store::topology::WriterSession>,
    ) {
        self.replace_serving_session(Some(session));
    }
    /// A failed initial H has no serving capability and no watcher yet. The
    /// empty-queue retry must be armed *before* starting the tick; otherwise an
    /// alive daemon binds HTTP but cannot discover a repaired checkout without
    /// an explicit request. Back off from the already-failed startup attempt.
    pub fn retry_failed_serving_startup(self: &Arc<Self>) {
        self.idle_h_retry.store(true, Ordering::Release);
        *self.recovery_retry_after.lock().unwrap() =
            Some(Instant::now() + Duration::from_millis(250));
        self.start_queue_tick();
    }
    fn replace_serving_session(&self, next: Option<Arc<crate::store::topology::WriterSession>>) {
        let mut current = self.serving_session.lock().unwrap();
        let changed = match (&*current, &next) {
            (Some(old), Some(next)) => !Arc::ptr_eq(old, next),
            (None, None) => false,
            _ => true,
        };
        if changed {
            *self.watcher_work.lock().unwrap() = None;
        }
        *current = next;
    }
    /// The local writer checks the durable queue at idle and retries typed contention.
    fn start_queue_tick(self: &Arc<Self>) {
        self.start_maintenance_tick();
        // A synchronous fixture may retain an owner without starting a daemon runtime.
        // Do not consume the start flag until a Tokio executor can own the tick.
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        if self.queue_tick_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(20));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(state) = weak.upgrade() else {
                    break;
                };
                let worker = state.clone();
                let result = tokio::task::spawn_blocking(move || worker.queue_tick()).await;
                // The witness may read phase only after queue_tick released the
                // native stream, watcher mutex and any SQLite statement.
                if result.as_ref().is_ok_and(|outcome| outcome.is_ok()) {
                    state.report_causal_h_ready();
                }
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => best_effort_queue_stderr(
                        std::io::stderr(),
                        format_args!("queue tick failed: {error:#}\n"),
                    ),
                    Err(error) => best_effort_queue_stderr(
                        std::io::stderr(),
                        format_args!("queue tick worker failed: {error:#}\n"),
                    ),
                }
            }
        });
    }
    /// One independent, verified-owner, low-priority lane. It never holds the
    /// serial native stream or a watcher mutex across a Store writer unit.
    fn start_maintenance_tick(self: &Arc<Self>) {
        if tokio::runtime::Handle::try_current().is_err()
            || self.maintenance_tick_started.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(20));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(state) = weak.upgrade() else {
                    break;
                };
                let worker = state.clone();
                let failure =
                    match tokio::task::spawn_blocking(move || worker.maintenance_tick()).await {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(maintenance_error_class(&error)),
                        Err(_) => Some(("worker_join", 0)),
                    };
                if let Some((category, code)) = failure {
                    let permitted = state
                        .maintenance_error_limiter
                        .lock()
                        .unwrap()
                        .permit(Instant::now());
                    if permitted {
                        let notice = maintenance_error_notice(category, code);
                        best_effort_queue_stderr(std::io::stderr(), format_args!("{notice}"));
                    }
                }
            }
        });
    }

    fn maintenance_priority_reason(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
        options: &IndexOptions,
    ) -> Option<&'static str> {
        if self.native_stream.try_lock().is_err() {
            return Some("native_stream");
        }
        if self.store.root_path_replaced().unwrap_or(true) || session.verify().is_err() {
            return Some("root_or_writer");
        }
        if self
            .pending_requests
            .try_lock()
            .map_or(true, |pending| !pending.is_empty())
        {
            return Some("local_fifo");
        }
        if self.store.has_recorded_completion(session).unwrap_or(true) {
            return Some("terminal_ack");
        }
        if self.serving_session.try_lock().map_or(true, |current| {
            !current
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, session))
        }) {
            return Some("session_replaced");
        }
        match self.watcher_work.try_lock() {
            Ok(work) => match work.as_ref() {
                Some((owner, scheduler))
                    if owner
                        .upgrade()
                        .is_some_and(|owner| Arc::ptr_eq(&owner, session)) =>
                {
                    if scheduler.accepted_watch_intent(options) {
                        Some("accepted_watcher")
                    } else {
                        None
                    }
                }
                _ => Some("watcher_replaced"),
            },
            Err(_) => Some("watcher_busy"),
        }
    }

    fn maintenance_priority(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
        options: &IndexOptions,
    ) -> bool {
        self.maintenance_priority_reason(session, options).is_none()
    }

    /// Targeted indexed debt probe, only for explicitly enabled diagnostics.
    /// A BUSY/unknown read never blocks or changes foreground work.
    fn maintenance_oldest_age(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
    ) -> (String, Option<u64>) {
        if !crate::index_coordinator::diagnostics_enabled() {
            return ("disabled".into(), None);
        }
        let Ok(leader) = session.writer_guard() else {
            return ("unknown".into(), None);
        };
        match self.store.maintenance_oldest_due_age_secs(leader) {
            Ok(Some(age)) => (age.to_string(), Some(age)),
            Ok(None) => ("none_or_clock_held".into(), None),
            Err(_) => ("unknown".into(), None),
        }
    }

    fn maintenance_deferred_reason(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
        options: &IndexOptions,
    ) -> &'static str {
        if let Some(reason) = self.maintenance_priority_reason(session, options) {
            return reason;
        }
        match self.store.open_maintenance_queue_probe() {
            Ok(probe) => match probe.check() {
                crate::store::MaintenanceQueueState::Clear => "writer_or_gate_uncertain",
                crate::store::MaintenanceQueueState::Pending => "external_fifo",
                crate::store::MaintenanceQueueState::Unknown => "queue_probe_unknown",
            },
            Err(_) => "queue_probe_unknown",
        }
    }

    fn maintenance_deferred(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
        reason: &str,
    ) {
        let (oldest, measured_age) = self.maintenance_oldest_age(session);
        let mut stats = self.maintenance_telemetry.lock().unwrap();
        stats.busy_attempts = self.store.maintenance_sqlite_busy_attempts();
        stats.observe_due_age(measured_age);
        let (age, max_age) = stats.deferred(Instant::now());
        let details = format!(
            "reason={reason} preemptions={} busy_attempts={} successful_units={} deferred_ms={age} max_deferred_ms={max_age} max_deferred_age_s={} oldest_due_age_s={oldest}",
            stats.preemptions,
            stats.busy_attempts,
            stats.successful_units,
            stats.max_deferred_age_s
        );
        drop(stats);
        crate::index_coordinator::diagnostic_marker(
            &session.incarnation().to_string(),
            "deferred",
            &details,
        );
    }

    fn gc_tick(
        &self,
        session: &Arc<crate::store::topology::WriterSession>,
        options: &IndexOptions,
    ) -> anyhow::Result<()> {
        if self.gc_last_check.lock().unwrap().elapsed() < Duration::from_secs(3600)
            || !self.maintenance_priority(session, options)
        {
            return Ok(());
        }
        let probe = match self.store.open_maintenance_queue_probe() {
            Ok(probe) => probe,
            Err(_) => return Ok(()),
        };
        if probe.check() != crate::store::MaintenanceQueueState::Clear {
            return Ok(());
        }
        let Some(permit) = self.store.maintenance_try_enter() else {
            return Ok(());
        };
        drop(permit); // Never hold the gate across the whole directory scan.
        let Ok(leader) = session.writer_guard() else {
            return Ok(());
        };
        let mut priority = |stage| -> anyhow::Result<()> {
            if stage == crate::store::topology::GcStage::AfterFirstDbUnlink
                || stage == crate::store::topology::GcStage::AfterParentSync
            {
                // Once unlink begins, finish the candidate's guarded sequence.
                return Ok(());
            }
            let Some(unit) = self.store.maintenance_try_enter() else {
                return Err(anyhow::Error::new(GcPriorityYield));
            };
            let clear = self.maintenance_priority(session, options)
                && probe.check() == crate::store::MaintenanceQueueState::Clear;
            drop(unit);
            if !clear {
                return Err(anyhow::Error::new(GcPriorityYield));
            }
            Ok(())
        };
        let started = Instant::now();
        crate::index_coordinator::diagnostic_marker(
            &session.incarnation().to_string(),
            "start",
            "kind=gc",
        );
        let result = self.store.automatic_gc_cooperative(leader, &mut priority);
        let outcome = if result.is_ok() {
            "completed"
        } else if result
            .as_ref()
            .is_err_and(|error| error.is::<GcPriorityYield>())
        {
            "deferred"
        } else {
            "error"
        };
        crate::index_coordinator::diagnostic_marker(
            &session.incarnation().to_string(),
            "end",
            &format!(
                "kind=gc outcome={outcome} duration_us={}",
                started.elapsed().as_micros()
            ),
        );
        // A daily attempt stamp may already have committed. Any remaining
        // candidate waits until the next authorized daily attempt after yield.
        *self.gc_last_check.lock().unwrap() = Instant::now();
        match result {
            Ok(_) => Ok(()),
            Err(error) if error.is::<GcPriorityYield>() => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn maintenance_tick(&self) -> anyhow::Result<()> {
        if self.checkout_runtime_stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let _lane = self.maintenance_stream.lock().unwrap();
        if self.checkout_runtime_stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        use crate::store::MaintenanceOutcome;
        if self.retention_last_run.lock().unwrap().elapsed() < Duration::from_secs(60) {
            return Ok(());
        }
        let session = self
            .serving_session
            .try_lock()
            .ok()
            .and_then(|held| held.clone());
        let Some(session) = session else {
            return Ok(());
        };
        // Read selected options only outside a maintenance writer. A changed
        // watcher/options binding is intent until native_stream replaces it.
        let options = match self.store.recorded_index_options() {
            Ok(Some(options)) => options,
            Ok(None) => self.options.clone(),
            Err(_) => return Ok(()),
        };
        if let Some(reason) = self.maintenance_priority_reason(&session, &options) {
            self.maintenance_deferred(&session, reason);
            *self.retention_last_run.lock().unwrap() =
                Instant::now() - Duration::from_secs(60) + Duration::from_millis(250);
            return Ok(());
        }
        let started = Instant::now();
        crate::index_coordinator::diagnostic_marker(
            &session.incarnation().to_string(),
            "start",
            &format!(
                "kind=retention oldest_due_age_s={}",
                self.maintenance_oldest_age(&session).0
            ),
        );
        let result =
            crate::index_coordinator::cooperative_maintenance_unit(&self.store, &session, || {
                self.maintenance_priority(&session, &options)
            });
        let outcome = match &result {
            Ok(crate::store::MaintenanceOutcome::Progress) => "progress",
            Ok(crate::store::MaintenanceOutcome::Deferred) => "deferred",
            Ok(crate::store::MaintenanceOutcome::Idle) => "idle",
            Err(_) => "error",
        };
        crate::index_coordinator::diagnostic_marker(
            &session.incarnation().to_string(),
            "end",
            &format!(
                "kind=retention outcome={outcome} duration_us={}",
                started.elapsed().as_micros()
            ),
        );
        match result {
            Ok(MaintenanceOutcome::Progress) => {
                let (oldest, measured_age) = self.maintenance_oldest_age(&session);
                let mut stats = self.maintenance_telemetry.lock().unwrap();
                stats.busy_attempts = self.store.maintenance_sqlite_busy_attempts();
                if let Some(age) = measured_age {
                    stats.max_deferred_age_s =
                        stats.max_deferred_age_s.max(age.saturating_sub(900));
                }
                let (deferred, max_deferred) = stats.progressed(Instant::now());
                let details = format!(
                    "successful_units={} preemptions={} busy_attempts={} deferred_ms={deferred} max_deferred_ms={max_deferred} max_deferred_age_s={} oldest_due_age_s={oldest}",
                    stats.successful_units,
                    stats.preemptions,
                    stats.busy_attempts,
                    stats.max_deferred_age_s
                );
                drop(stats);
                crate::index_coordinator::diagnostic_marker(
                    &session.incarnation().to_string(),
                    "progress",
                    &details,
                );
                // Keep making one small unit at the next idle tick, after
                // re-arbitrating every accepted watcher and external FIFO.
            }
            Ok(MaintenanceOutcome::Deferred) => {
                let reason = self.maintenance_deferred_reason(&session, &options);
                // Deferred also covers a newly accepted queue row or an unknown
                // probe. Do not invent an exact SQLite BUSY count from it.
                self.maintenance_deferred(&session, reason);
                *self.retention_last_run.lock().unwrap() =
                    Instant::now() - Duration::from_secs(60) + Duration::from_millis(250);
            }
            Ok(MaintenanceOutcome::Idle) => {
                *self.retention_last_run.lock().unwrap() = Instant::now();
            }
            Err(error) => {
                *self.retention_last_run.lock().unwrap() = Instant::now();
                return Err(error);
            }
        }
        self.gc_tick(&session, &options)?;
        Ok(())
    }

    /// Exercise the independent low-priority maintenance lane at its deadline
    /// in integration tests, without waiting for wall-clock time.
    #[doc(hidden)]
    pub fn force_retention_idle_tick_for_tests(self: &Arc<Self>) -> anyhow::Result<()> {
        // A standalone fixture can retain an owner without running the native
        // stream. Verify/ack its initial full watcher inventory before claiming
        // a genuine idle maintenance opportunity.
        {
            let _stream = self.native_stream.lock().unwrap();
            let session = self.serving_session.lock().unwrap().clone();
            if let Some(session) = session {
                let options = self
                    .store
                    .recorded_index_options()?
                    .unwrap_or_else(|| self.options.clone());
                let mut work = self.watcher_work.lock().unwrap();
                if work.is_none() {
                    let mut scheduler = crate::index_coordinator::WatcherWork::new(
                        &self.store,
                        &session,
                        &options,
                    )?;
                    // A late OS event from before this forced fixture tick must
                    // not defer pin expiration as accepted watcher work (#131).
                    scheduler.disable_native_watcher_for_tests();
                    scheduler.reconcile_due(
                        &self.store,
                        &session,
                        &options,
                        &Arc::new(AtomicBool::new(false)),
                        true,
                    )?;
                    *work = Some((Arc::downgrade(&session), scheduler));
                }
            }
        }
        *self.retention_last_run.lock().unwrap() = Instant::now() - Duration::from_secs(60);
        self.maintenance_tick()
    }

    /// Drive the serial root-loss transition in a standalone fixture without
    /// conflating it with the independent low-priority maintenance lane.
    #[doc(hidden)]
    pub fn force_root_transition_tick_for_tests(self: &Arc<Self>) -> anyhow::Result<()> {
        self.queue_tick()
    }

    /// Inspect the three mutually exclusive roles at a root-loss tick boundary.
    #[doc(hidden)]
    pub fn root_loss_retirement_for_tests(&self) -> (bool, bool, bool) {
        (
            self.serving_session.lock().unwrap().is_some(),
            self.watcher_work.lock().unwrap().is_some(),
            false,
        )
    }

    fn queue_tick(self: &Arc<Self>) -> anyhow::Result<()> {
        if self.checkout_runtime_stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        #[cfg(test)]
        self.test_queue_before_stream.run();
        let _stream = self.native_stream.lock().unwrap();
        if self.checkout_runtime_stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        #[cfg(test)]
        self.test_queue_after_stream.run();
        // A lost captured root has no claim or publication authority. Resolve
        // its accepted queue rows before releasing local scheduler resources.
        if self.store.root_path_replaced()? {
            self.store.fail_changed_root_requests()?;
            self.replace_serving_session(None);
            return Ok(());
        }
        // A previously serving writer cannot claim against a damaged index.
        // Retire its local work before the protected exceptional H path runs.
        if self.store.is_recreate_pending() {
            self.replace_serving_session(None);
        }
        if self
            .recovery_retry_after
            .lock()
            .unwrap()
            .is_some_and(|retry| Instant::now() < retry)
        {
            return Ok(());
        }
        let mut pending = self.pending_requests.lock().unwrap();
        pending.retain(|id| {
            #[cfg(test)]
            if self
                .test_pending_read_failures
                .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    (count > 0).then(|| count - 1)
                })
                .is_ok()
            {
                return true;
            }
            match self.store.request_by_id(id) {
                Ok(Some(row)) => row.finished_at.is_none(),
                Ok(None) | Err(_) => true,
            }
        });
        let pending_local = !pending.is_empty();
        drop(pending);
        #[cfg(test)]
        self.test_queue_after_pending_snapshot.run();
        let retry_h = self.idle_h_retry.load(Ordering::Acquire);
        let durable_pending = if !pending_local && !retry_h {
            self.store.earliest_unfinished_request()?.is_some()
        } else {
            false
        };
        if self.serving_session.lock().unwrap().is_none() {
            if !pending_local
                && !durable_pending
                && !retry_h
                && !self.store.is_root_replaced()
                && !self.store.is_recreate_pending()
            {
                return Ok(());
            }
            #[cfg(test)]
            let head = if self
                .test_activation_head_busy_once
                .swap(false, Ordering::AcqRel)
            {
                Err(crate::store::topology::StorageBusy.into())
            } else {
                self.store.earliest_unfinished_request()
            };
            #[cfg(not(test))]
            let head = self.store.earliest_unfinished_request();
            let head_options = match head {
                Ok(head) => head
                    .map(|head| head.options(std::path::Path::new(self.store.workspace_root())))
                    .transpose()?,
                Err(error) => {
                    if crate::store::transient_storage_contention(&error) {
                        *self.recovery_retry_after.lock().unwrap() =
                            Some(Instant::now() + Duration::from_millis(250));
                        return Ok(());
                    }
                    return Err(error);
                }
            };
            // An index requiring exceptional recreation has no trusted
            // recorded options. Without a verified FIFO head, daemon defaults
            // cannot establish the missing request's provenance.
            if self.store.is_recreate_pending()
                && !self.store.is_root_replaced()
                && head_options.is_none()
            {
                *self.recovery_retry_after.lock().unwrap() =
                    Some(Instant::now() + Duration::from_millis(500));
                anyhow::bail!("recovery_required: no accepted index options for exceptional H");
            }
            let recorded = if self.store.is_recreate_pending() {
                None
            } else {
                self.store.recorded_index_options()?
            };
            let selected_options = recorded
                .or(head_options)
                .unwrap_or_else(|| self.options.clone());
            #[cfg(feature = "test-support")]
            if let Some(hook) = self.checkout_h_hook.lock().unwrap().take() {
                hook();
            }
            match crate::index_coordinator::establish_serving_session(
                &self.store,
                Some(&selected_options),
                &Arc::new(AtomicBool::new(false)),
            ) {
                Ok(session) => {
                    self.replace_serving_session(Some(session));
                    self.idle_h_retry.store(false, Ordering::Release);
                    *self.recovery_retry_after.lock().unwrap() = None;
                }
                Err(error) => {
                    let busy = crate::store::transient_storage_contention(&error);
                    *self.recovery_retry_after.lock().unwrap() =
                        Some(Instant::now() + Duration::from_millis(if busy { 250 } else { 500 }));
                    if busy {
                        return Ok(());
                    }
                    return Err(error);
                }
            }
        }
        let retained = self.serving_session.lock().unwrap().clone();
        if let Some(ref session) = retained
            && session.verify().is_ok()
        {
            let processed = match crate::index_coordinator::drain_one_request_observed(
                &self.store,
                session,
                |id, p| {
                    self.job_progress.lock().unwrap().insert(id.to_owned(), p);
                },
            ) {
                Ok(processed) => processed,
                Err(error) => {
                    // A cached terminal result must be resolved by this same
                    // incarnation. Otherwise release the stale writer so a
                    // successor can reclaim an unfinished running head.
                    if !self.store.has_recorded_completion(session)?
                        || !crate::index_coordinator::retryable_completion_error(&error)
                    {
                        self.replace_serving_session(None);
                    }
                    return Err(error);
                }
            };
            if processed > 0 {
                *self.packets.lock().unwrap() = PacketCache::default();
                self.start_dependency_index();
            }
            #[cfg(test)]
            self.test_queue_after_h_drain.run();
            // A verified drain normally consumes every FIFO head. If one is
            // still queued, the guarded publish-BUSY path deferred it. Avoid
            // recapturing the workspace every 20 ms under sustained readers.
            if processed == 0
                && self
                    .store
                    .earliest_unfinished_request()
                    .is_ok_and(|head| head.is_some_and(|request| request.state == "queued"))
            {
                *self.recovery_retry_after.lock().unwrap() =
                    Some(Instant::now() + Duration::from_millis(250));
            }
            if (processed > 0 || self.store.earliest_unfinished_request()?.is_none())
                && !self.store.has_recorded_completion(session)?
            {
                // The idle watcher follows the selected head's persisted inputs.
                // Daemon defaults cannot silently supersede a FIFO head's options.
                let selected_options = self
                    .store
                    .recorded_index_options()?
                    .unwrap_or_else(|| self.options.clone());
                {
                    let mut work = self.watcher_work.lock().unwrap();
                    if !work.as_ref().is_some_and(|(owner, _)| {
                        owner
                            .upgrade()
                            .is_some_and(|owner| Arc::ptr_eq(&owner, session))
                    }) {
                        let mut scheduler = crate::index_coordinator::WatcherWork::new(
                            &self.store,
                            session,
                            &selected_options,
                        )?;
                        if let Some(reporter) = self.causal_witness.lock().unwrap().clone() {
                            scheduler.attach_causal_witness(
                                reporter,
                                session.incarnation(),
                                self.causal_lineage_ordinal.clone(),
                            );
                        }
                        *work = Some((Arc::downgrade(session), scheduler));
                    }
                    if let Some((_, scheduler)) = work.as_mut() {
                        let accounted = scheduler.reconcile_due(
                            &self.store,
                            session,
                            &selected_options,
                            &Arc::new(AtomicBool::new(false)),
                            false,
                        )?;
                        // ACK only a verified, acknowledged full inventory.
                        // Neither a failed FIFO probe nor a newer accepted hint
                        // can be called settled. The reporter only try_sends here.
                        if accounted
                            && let Some(reporter) = self.causal_witness.lock().unwrap().clone()
                            && !scheduler.accepted_watch_intent(&selected_options)
                            && matches!(self.store.earliest_unfinished_request(), Ok(None))
                            && let (Some(lineage), Some(generation), Some(certified_pin)) = (
                                scheduler.causal_lineage(),
                                scheduler.accounted_watch_generation(),
                                scheduler.accounted_watch_pin(),
                            )
                            && self.store.verify_writer_session(session).is_ok()
                            && self
                                .store
                                .index_baseline()
                                .is_ok_and(|pin| pin == certified_pin)
                            && self.store.verify_writer_session(session).is_ok()
                        {
                            reporter.watch_ack(lineage, generation, certified_pin, true);
                        }
                    }
                }
            }
            return Ok(());
        }
        Ok(())
    }
    pub fn retained_serving_session(
        &self,
    ) -> anyhow::Result<Arc<crate::store::topology::WriterSession>> {
        self.serving_session
            .lock()
            .unwrap()
            .clone()
            .context("index_not_ready: no verified daemon serving session")
    }

    /// Start a replacement generation without doing filesystem or database work on the caller.
    /// A previous generation can finish, but can never publish over its replacement.
    pub fn start_dependency_index(self: &Arc<Self>) {
        let Some(options) = self.dependency_options.clone() else {
            return;
        };
        if !self.dependencies.lock().unwrap().request_build() {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            loop {
                let (generation, cancel) = {
                    let mut index = state.dependencies.lock().unwrap();
                    if index.stopped {
                        index.worker_running = false;
                        return;
                    }
                    (index.generation, index.cancel.clone())
                };
                let worker = state.clone();
                let worker_cancel = cancel.clone();
                let options = options.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let result = (|| {
                        let revision = worker.store.status()?.revision;
                        Catalog::build(
                            &worker.options.workspace_root,
                            revision,
                            &options,
                            &worker_cancel,
                        )
                    })();
                    worker.publish_dependency_index(generation, &worker_cancel, result);
                })
                .await;
                if result.is_err() {
                    // Error publication does not access the database.
                    state.publish_dependency_index(
                        generation,
                        &cancel,
                        Err(anyhow::anyhow!("worker failed")),
                    );
                }
                let mut index = state.dependencies.lock().unwrap();
                if index.stopped || index.generation == generation {
                    index.worker_running = false;
                    return;
                }
                // Refresh bursts coalesce to the latest generation. Only this driver
                // can launch catalog work, and its previous blocking worker has exited.
            }
        });
    }
    fn publish_dependency_index(
        &self,
        generation: u64,
        cancel: &AtomicBool,
        result: anyhow::Result<Catalog>,
    ) {
        // Called on a blocking worker for successful builds. No parsing, source I/O,
        // or database work while holding the catalog lock.
        let result = result.and_then(|catalog| {
            anyhow::ensure!(
                self.store.status()?.revision == catalog.workspace_revision,
                "revision conflict"
            );
            Ok(catalog)
        });
        let mut index = self.dependencies.lock().unwrap();
        if index.stopped || index.generation != generation || cancel.load(Ordering::Acquire) {
            return;
        }
        match result {
            Ok(catalog) => {
                index.state = "ready";
                index.catalog = Some(Arc::new(catalog));
            }
            Err(_) => {
                index.state = "failed";
                index.catalog = None;
                index.warnings = vec![
                    "Dependency catalog build failed or workspace changed; refresh to retry".into(),
                ];
            }
        }
    }
    /// Presentation-only snapshot. The caller must use a matching cached workspace revision.
    pub(crate) fn catalog_snapshot(&self, revision: IndexPin) -> Option<Arc<Catalog>> {
        self.dependencies
            .lock()
            .unwrap()
            .catalog
            .as_ref()
            .filter(|catalog| catalog.workspace_revision == revision)
            .cloned()
    }
    pub fn cancel_active(&self) {
        {
            let mut dependencies = self.dependencies.lock().unwrap();
            dependencies.stopped = true;
            dependencies.cancel.store(true, Ordering::Release);
            dependencies.generation += 1;
            dependencies.catalog = None;
            if self.dependency_options.is_some() {
                dependencies.state = "failed";
                dependencies.warnings =
                    vec!["Dependency indexing cancelled during shutdown".into()];
            }
        }
        if let Some(acp) = &self.acp {
            acp.cancel_all();
        }
        self.jobs
            .lock()
            .unwrap()
            .cancel
            .store(true, Ordering::Release);
    }
}
fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error":{"code":code,"message":message}})),
    )
        .into_response()
}
#[derive(Debug)]
struct ApiError(StatusCode, &'static str, &'static str);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        error(self.0, self.1, self.2)
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        // IOERR is broken SQLite locking/storage, not ordinary BUSY. Even an
        // outer storage_busy context must never mask an IOERR_RDLOCK as 409.
        if e.chain().any(|cause| cause.downcast_ref::<rusqlite::Error>().is_some_and(|error| {
            matches!(error, rusqlite::Error::SqliteFailure(info, _) if info.code == rusqlite::ErrorCode::SystemIoFailure)
        })) {
            return Self(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", "Operation failed");
        }
        if e.chain().any(|cause| cause.downcast_ref::<rusqlite::Error>().is_some_and(|error| {
            matches!(error, rusqlite::Error::SqliteFailure(info, _) if matches!(info.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
        })) {
            return Self(StatusCode::CONFLICT, "storage_busy", "Storage is busy");
        }
        if let Some(directory) = e.downcast_ref::<BrowseDirectoryError>() {
            return browse_directory_error(&directory.0);
        }
        if let Some(invalid) = e.downcast_ref::<crate::navigation::InvalidRequest>() {
            Self(
                StatusCode::BAD_REQUEST,
                "invalid_navigation_request",
                invalid.0,
            )
        } else if let Some(invalid) = e.downcast_ref::<crate::class_diagram::InvalidRequest>() {
            Self(StatusCode::BAD_REQUEST, "invalid_class_request", invalid.0)
        } else if e
            .chain()
            .any(|cause| cause.downcast_ref::<crate::store::PinExpired>().is_some())
        {
            Self(StatusCode::CONFLICT, "pin_expired", "The index pin expired")
        } else if e
            .chain()
            .any(|cause| cause.to_string().starts_with("revision conflict"))
        {
            Self(
                StatusCode::CONFLICT,
                "revision_conflict",
                "The index revision changed",
            )
        } else if e.to_string() == "packet_missing" {
            missing()
        } else if e.to_string() == "invalid_question_selection" {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_question_selection",
                "Invalid question or selection",
            )
        } else if e.chain().any(|cause| {
            matches!(
                cause.to_string().as_str(),
                "saved view target replacement is not allowed"
                    | "saved annotation target replacement is not allowed"
                    | "native declaration target missing"
            )
        }) {
            invalid()
        } else {
            // Context must not hide an exact allowlisted refusal from the
            // public API. An IOERR_RDLOCK is NOT busy/locked and still maps
            // to 500; never mask broken SQLite locking as retryable work.
            for cause in e.chain() {
                let text = cause.to_string();
                for (prefix, status, code) in [
                    ("root_changed", StatusCode::CONFLICT, "root_changed"),
                    (
                        "root_key_collision",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "root_key_collision",
                    ),
                    (
                        "workspace_id_changed",
                        StatusCode::CONFLICT,
                        "workspace_id_changed",
                    ),
                    ("storage_busy", StatusCode::CONFLICT, "storage_busy"),
                    (
                        "index_not_ready",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "index_not_ready",
                    ),
                    (
                        "incompatible_index",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "incompatible_index",
                    ),
                    (
                        "incompatible_record",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "incompatible_record",
                    ),
                    (
                        "incomplete_record",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "incomplete_record",
                    ),
                    (
                        "recovery_required",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "recovery_required",
                    ),
                    (
                        "unsafe_index",
                        StatusCode::SERVICE_UNAVAILABLE,
                        "unsafe_index",
                    ),
                ] {
                    if text.starts_with(prefix) {
                        return Self(status, code, "Storage is unavailable");
                    }
                }
            }
            Self(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Operation failed",
            )
        }
    }
}
fn invalid() -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "Invalid request",
    )
}
fn missing() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not_found", "Not found")
}
async fn db<T: Send + 'static>(
    s: Arc<DaemonState>,
    f: impl FnOnce(&Store) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(move || f(&s.store))
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Operation failed",
            )
        })?
        .map_err(Into::into)
}
#[cfg(feature = "test-support")]
async fn guard(State(s): State<Arc<DaemonState>>, req: Request, next: Next) -> Response {
    guard_common(&s.token, &s.hosts, &s.origins, req, next).await
}

async fn guard_common(
    token: &str,
    hosts: &[String],
    origins: &[String],
    mut req: Request,
    next: Next,
) -> Response {
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin = headers.get(header::ORIGIN);
    let mut response = if headers.get_all(header::HOST).iter().count() != 1
        || !host.is_some_and(|h| hosts.iter().any(|v| v == h))
    {
        error(StatusCode::FORBIDDEN, "invalid_host", "Host is not allowed")
    } else if headers.get_all(header::ORIGIN).iter().count() > 1
        || origin.is_some_and(|h| {
            !h.to_str()
                .ok()
                .is_some_and(|v| origins.iter().any(|o| o == v))
        })
    {
        error(
            StatusCode::FORBIDDEN,
            "invalid_origin",
            "Origin is not allowed",
        )
    } else if (req.uri().path() == "/api" || req.uri().path().starts_with("/api/"))
        && (headers.get_all(header::AUTHORIZATION).iter().count() != 1
            || !headers
                .get(header::AUTHORIZATION)
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.strip_prefix("Bearer "))
                .is_some_and(|t| bool::from(t.as_bytes().ct_eq(token.as_bytes()))))
    {
        error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Bearer authentication required",
        )
    } else {
        let body = std::mem::replace(req.body_mut(), Body::empty());
        match to_bytes(body, 1024 * 1024).await {
            Ok(bytes) => {
                *req.body_mut() = Body::from(bytes);
                next.run(req).await
            }
            Err(_) => error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "Request body exceeds limit",
            ),
        }
    };
    if response.status().is_client_error()
        && !response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|v| v == "application/json")
    {
        response = error(response.status(), "invalid_request", "Invalid request");
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY,"default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}
#[cfg(feature = "test-support")]
pub fn router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    include_str!("../web/index.html"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/app.js"),
                )
            }),
        )
        .route(
            "/shell.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/shell.js"),
                )
            }),
        )
        .route(
            "/fonts/jetbrains-mono-latin.woff2",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "font/woff2")],
                    &include_bytes!("../web/fonts/jetbrains-mono-latin.woff2")[..],
                )
            }),
        )
        .route(
            "/fonts/space-grotesk-latin.woff2",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "font/woff2")],
                    &include_bytes!("../web/fonts/space-grotesk-latin.woff2")[..],
                )
            }),
        )
        .route(
            "/fonts/JetBrainsMono-OFL.txt",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    include_str!("../web/fonts/JetBrainsMono-OFL.txt"),
                )
            }),
        )
        .route(
            "/fonts/SpaceGrotesk-OFL.txt",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    include_str!("../web/fonts/SpaceGrotesk-OFL.txt"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/style.css"),
                )
            }),
        )
        .route(
            "/healthz",
            get(|| async { Json(json!({"ok":true,"version":env!("CARGO_PKG_VERSION")})) }),
        )
        .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
        .route("/api/status", get(status))
        .route("/api/jev/status", get(jev_status))
        .route("/api/acp/status", get(acp_status))
        .route(
            "/api/questions/{packet_id}/acp-answer",
            post(question_answer),
        )
        .route("/api/questions/{packet_id}/jev-run", post(question_run))
        .route("/api/index", post(start_index))
        .route("/api/jobs/current", get(current_job))
        .route("/api/jobs/{id}", get(job))
        .route("/api/jobs/{id}/cancel", post(cancel_job))
        .route("/api/dependencies", get(dependency_status))
        .route("/api/dependencies/refresh", post(dependency_refresh))
        .route("/api/dependencies/symbols", get(dependency_symbols))
        .route("/api/dependencies/source", get(dependency_source))
        .route("/api/rust-sources", get(rust_source_roots))
        .route("/api/rust-sources/tree", get(rust_source_tree))
        .route("/api/rust-sources/file", get(rust_source_file))
        .route("/api/tree", get(tree))
        .route("/api/files", get(files))
        .route("/api/methods", get(methods))
        .route("/api/sequence", post(sequence))
        .route("/api/classes", get(classes))
        .route("/api/class-diagram", post(class_diagram))
        .route("/api/navigation", post(navigation))
        .route(
            "/classes.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/classes.js"),
                )
            }),
        )
        .route(
            "/classes.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/classes.css"),
                )
            }),
        )
        .route(
            "/navigation.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/navigation.js"),
                )
            }),
        )
        .route(
            "/navigation.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/navigation.css"),
                )
            }),
        )
        .route(
            "/sequence.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/sequence.js"),
                )
            }),
        )
        .route("/api/symbols", get(symbols))
        .route("/api/symbol", get(symbol))
        .route("/api/source", get(source))
        .route("/api/query", post(query))
        .route("/api/questions/preview", post(question_preview))
        .route(
            "/api/questions/{packet_id}/jev-request",
            get(question_export),
        )
        .route(
            "/api/questions/{packet_id}/jev-response",
            post(question_import),
        )
        .route(
            "/api/questions/{packet_id}/selection",
            post(question_selection),
        )
        .route("/api/views", get(views))
        .route(
            "/api/views/{id}",
            get(view).put(save_view).delete(delete_view),
        )
        .route("/api/annotations", get(annotations))
        .route(
            "/api/annotations/{id}",
            put(save_annotation).delete(delete_annotation),
        )
        .fallback(|| async { missing().into_response() })
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            evidence_response_guard,
        ))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}
// Only endpoints that include a native snapshot need this outer response fence.
// Durable records and external-source-only routes keep their independent policy.
#[cfg(feature = "test-support")]
fn native_response_route(path: &str) -> bool {
    matches!(
        path,
        "/api/status"
            | "/api/tree"
            | "/api/files"
            | "/api/methods"
            | "/api/sequence"
            | "/api/classes"
            | "/api/class-diagram"
            | "/api/navigation"
            | "/api/symbols"
            | "/api/symbol"
            | "/api/source"
            | "/api/query"
            | "/api/dependencies"
            | "/api/dependencies/symbols"
            | "/api/dependencies/source"
            | "/api/rust-sources/tree"
    )
}
#[cfg(feature = "test-support")]
async fn evidence_response_guard(
    State(state): State<Arc<DaemonState>>,
    request: Request,
    next: Next,
) -> Response {
    if !native_response_route(request.uri().path()) {
        return next.run(request).await;
    }
    // The snapshot is short: retain its protected writer observation through
    // serialization. An ordinary same-incarnation publish does not reject an
    // already-materialized coherent unpinned response.
    let admitted = db(state.clone(), |store| {
        let snapshot = store.evidence_response()?;
        Ok(snapshot.into_fence(EvidenceFencePolicy::T03))
    })
    .await;
    let response = next.run(request).await;
    if !response.status().is_success() {
        // Preserve validation/materialization error precedence, but still check T03.
        if let Ok(fence) = admitted {
            let _ = fence.finish(());
        }
        return response;
    }
    let fence = match admitted {
        Ok(fence) => fence,
        Err(error) => return error.into_response(),
    };
    #[cfg(test)]
    if let Some(hook) = state.outer_fence_hook.lock().unwrap().as_ref() {
        hook();
    }
    match fence.finish(response) {
        Ok(response) => response,
        Err(error) => ApiError::from(error).into_response(),
    }
}
fn dependency_stale() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "stale_catalog",
        "Dependency catalog or source changed; refresh the catalog",
    )
}
#[cfg(feature = "test-support")]
async fn dependency_work<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "dependency_error",
            "Dependency worker failed",
        )
    })?
}
impl DaemonState {
    #[cfg(feature = "test-support")]
    fn requested_catalog(&self, id: &str) -> Result<Arc<Catalog>, ApiError> {
        let revision = self.store.status()?.revision;
        self.catalog_snapshot(revision)
            .filter(|catalog| catalog.id == id)
            .ok_or_else(dependency_stale)
    }
    #[cfg(feature = "test-support")]
    fn check_catalog(&self, catalog: &Arc<Catalog>) -> Result<(), ApiError> {
        let current = self.requested_catalog(&catalog.id)?;
        if !Arc::ptr_eq(&current, catalog) {
            return Err(dependency_stale());
        }
        Ok(())
    }
}
#[cfg(feature = "test-support")]
async fn dependency_status(State(s): State<Arc<DaemonState>>) -> Result<Response, ApiError> {
    dependency_work(move || dependency_status_response(&s, || {}).map_err(ApiError::from)).await
}
#[cfg(feature = "test-support")]
fn dependency_status_response(
    s: &DaemonState,
    after_capture: impl FnOnce(),
) -> anyhow::Result<Response> {
    let response = s.store.evidence_response()?;
    let revision = response.status()?.revision;
    after_capture();
    #[cfg(test)]
    if let Some(hook) = s.dependency_capture_hook.lock().unwrap().as_ref() {
        hook();
    }
    let (state, catalog, mut warnings) = {
        let index = s.dependencies.lock().unwrap();
        (index.state, index.catalog.clone(), index.warnings.clone())
    };
    let payload = if let Some(catalog) = catalog {
        if catalog.workspace_revision == revision {
            json!({"state":state,"workspaceRevision":revision,
                "catalogId":catalog.id,"packages":catalog.packages,
                "symbolCount":catalog.symbols.len(),"warnings":catalog.warnings})
        } else {
            warnings.push("Workspace changed; refresh the dependency catalog".into());
            json!({"state":"failed","workspaceRevision":revision,
                "catalogId":null,"packages":[],"symbolCount":0,"warnings":warnings})
        }
    } else {
        json!({"state":state,"workspaceRevision":revision,
            "catalogId":null,"packages":[],"symbolCount":0,"warnings":warnings})
    };
    // IntoResponse serializes the owned JSON before the final T03 check. No
    // evidence body can escape on a failed fence, including the failed-catalog branch.
    let result = Json(payload).into_response();
    response.finish(result)
}
#[cfg(feature = "test-support")]
async fn dependency_refresh(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body: Value = serde_json::from_slice(&body).map_err(|_| invalid())?;
    if !body.as_object().is_some_and(|object| object.is_empty()) {
        return Err(invalid());
    }
    if s.dependency_options.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "dependencies_disabled",
            "Dependency catalog is disabled",
        ));
    }
    if s.dependencies.lock().unwrap().stopped {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "Daemon is shutting down",
        ));
    }
    s.start_dependency_index();
    Ok((StatusCode::ACCEPTED, Json(json!({"state":"loading"}))))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DependencySymbolsQuery {
    catalog_id: String,
    package_id: Option<String>,
    #[serde(default)]
    q: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "dependency_limit")]
    limit: usize,
}
fn dependency_limit() -> usize {
    100
}
#[cfg(feature = "test-support")]
async fn dependency_symbols(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<DependencySymbolsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    if q.catalog_id.is_empty()
        || q.catalog_id.len() > 8192
        || q.q.len() > 8192
        || q.package_id.as_ref().is_some_and(|id| id.len() > 8192)
        || !(1..=200).contains(&q.limit)
        || q.offset > 50_000
    {
        return Err(invalid());
    }
    dependency_work(move || {
        let catalog = s.requested_catalog(&q.catalog_id)?;
        if q.package_id
            .as_ref()
            .is_some_and(|id| !catalog.packages.iter().any(|p| &p.id == id))
        {
            return Err(missing());
        }
        let search = q.q.to_lowercase();
        let mut matches = catalog
            .symbols
            .iter()
            .filter(|symbol| {
                q.package_id
                    .as_ref()
                    .is_none_or(|id| &symbol.package_id == id)
                    && (search.is_empty()
                        || symbol.qualified_name.to_lowercase().contains(&search)
                        || symbol.name.to_lowercase().contains(&search))
            })
            .skip(q.offset);
        let items: Vec<_> = matches.by_ref().take(q.limit).collect();
        let next_offset = matches.next().map(|_| q.offset + items.len());
        let response = Json(
            json!({"catalogId":catalog.id,"workspaceRevision":catalog.workspace_revision,
            "items":items,"nextOffset":next_offset}),
        );
        s.check_catalog(&catalog)?;
        Ok(response)
    })
    .await
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DependencySourceQuery {
    catalog_id: String,
    source_ref: String,
}
#[cfg(feature = "test-support")]
async fn dependency_source(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<DependencySourceQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    if q.catalog_id.is_empty()
        || q.catalog_id.len() > 8192
        || q.source_ref.is_empty()
        || q.source_ref.len() > 8192
    {
        return Err(invalid());
    }
    dependency_work(move || {
        let catalog = s.requested_catalog(&q.catalog_id)?;
        let source = catalog.sources.get(&q.source_ref).ok_or_else(missing)?;
        // This descriptor is retained from discovery. No user-selected paths or reopened roots.
        let text = source.directory.read_file(&source.path).map_err(rust_source_error)?;
        let hash = hex::encode(Sha256::digest(text.as_bytes()));
        if hash != source.hash { return Err(dependency_stale()); }
        let package = catalog.packages.iter().find(|package| package.id == source.package_id).ok_or_else(missing)?;
        let definitions: Vec<_> = catalog.symbols.iter().filter(|symbol| symbol.source_ref == q.source_ref)
            .map(|symbol| json!({"id":symbol.id,"name":symbol.name,"kind":symbol.kind,
                "parent":symbol.parent,"path":symbol.path,"range":symbol.range})).collect();
        let file = SourceFile { path: source.path.clone(), hash: hash.clone(), language: "rust".into(), text };
        let response = Json(json!({"id":q.source_ref,"rootId":package.id,"rootLabel":package.name,
            "path":source.path,"hash":hash,"file":file,"definitions":definitions,
            "warnings":["Definitional candidates only; not confirmed callees. Separate from the workspace graph and source-sharing scope."]}));
        s.check_catalog(&catalog)?;
        Ok(response)
    }).await
}

#[cfg(feature = "test-support")]
async fn rust_source_roots(State(s): State<Arc<DaemonState>>) -> Json<Value> {
    Json(
        json!({"roots": s.rust_sources.iter().map(|r| json!({"id": r.label, "label": r.label, "path": r.directory.root.to_string_lossy()})).collect::<Vec<_>>()}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RustTreeQuery {
    root: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "file_limit")]
    limit: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RustFileQuery {
    root: String,
    path: String,
}
fn rust_source_error(e: std::io::Error) -> ApiError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => missing(),
        ErrorKind::FileTooLarge => ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "source_too_large",
            "Source exceeds 2 MiB limit",
        ),
        ErrorKind::PermissionDenied => ApiError(
            StatusCode::FORBIDDEN,
            "source_forbidden",
            "Source access is not allowed",
        ),
        ErrorKind::InvalidInput | ErrorKind::InvalidData | ErrorKind::NotADirectory => {
            browse_invalid()
        }
        _ if e.raw_os_error() == Some(libc::ELOOP) => ApiError(
            StatusCode::FORBIDDEN,
            "symlink_forbidden",
            "Symlink traversal is not allowed",
        ),
        _ => ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "source_error",
            "Source could not be read",
        ),
    }
}
#[cfg(feature = "test-support")]
async fn rust_source_tree(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<RustTreeQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::file_tree::Page>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    if !crate::file_tree::valid_path(&q.path)
        || !(1..=200).contains(&q.limit)
        || q.offset > crate::file_tree::SCAN_LIMIT
    {
        return Err(browse_invalid());
    }
    let revision = db(s.clone(), |s| s.status()).await?.revision;
    tokio::task::spawn_blocking(move || {
        let root = s
            .rust_sources
            .iter()
            .find(|r| r.label == q.root)
            .ok_or_else(missing)?;
        let (items, next_offset, truncated) = root
            .directory
            .list(&q.path, q.offset, q.limit)
            .map_err(rust_source_error)?;
        Ok(Json(crate::file_tree::Page {
            root: root.directory.root.to_string_lossy().into_owned(),
            indexed_workspace: String::new(),
            path: q.path,
            revision,
            items,
            next_offset,
            truncated,
        }))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "source_error",
            "Source worker failed",
        )
    })?
}
#[cfg(feature = "test-support")]
async fn rust_source_file(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<RustFileQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::rust_sources::Snapshot>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    if q.path.is_empty() || !crate::file_tree::valid_path(&q.path) || !q.path.ends_with(".rs") {
        return Err(browse_invalid());
    }
    tokio::task::spawn_blocking(move || {
        let root = s
            .rust_sources
            .iter()
            .find(|r| r.label == q.root)
            .ok_or_else(missing)?;
        root.snapshot(&q.path).map(Json).map_err(rust_source_error)
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "source_error",
            "Source worker failed",
        )
    })?
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TreeQuery {
    #[serde(default)]
    path: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "file_limit")]
    limit: usize,
}
#[derive(Debug)]
struct BrowseDirectoryError(std::io::Error);
impl std::fmt::Display for BrowseDirectoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("browse directory unavailable")
    }
}
impl std::error::Error for BrowseDirectoryError {}

fn browse_directory_error(e: &std::io::Error) -> ApiError {
    match e.kind() {
        std::io::ErrorKind::NotFound => ApiError(
            StatusCode::NOT_FOUND,
            "directory_missing",
            "Directory no longer exists; refresh its parent",
        ),
        std::io::ErrorKind::PermissionDenied => ApiError(
            StatusCode::FORBIDDEN,
            "directory_forbidden",
            "Permission denied while listing directory",
        ),
        std::io::ErrorKind::NotADirectory | std::io::ErrorKind::InvalidInput => ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_directory",
            "Path must be a real directory; symlink traversal is not allowed",
        ),
        _ if e.raw_os_error() == Some(libc::ELOOP) => ApiError(
            StatusCode::FORBIDDEN,
            "symlink_forbidden",
            "Symlink traversal is not allowed",
        ),
        _ => ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "directory_error",
            "Directory could not be read; check permissions and refresh",
        ),
    }
}

#[cfg(feature = "test-support")]
async fn tree(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<TreeQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::file_tree::Page>, ApiError> {
    let Query(q) = query.map_err(|_| browse_invalid())?;
    if !crate::file_tree::valid_path(&q.path)
        || !(1..=200).contains(&q.limit)
        || q.offset > crate::file_tree::SCAN_LIMIT
    {
        return Err(browse_invalid());
    }
    tokio::task::spawn_blocking(move || {
        s.store.verify_root()?;
        let (mut items, next_offset, truncated) = s
            .browser
            .list(&q.path, q.offset, q.limit)
            .map_err(|e| browse_directory_error(&e))?;
        let (revision, indexed_workspace) = s.store.tree_metadata(&s.browser.root, &mut items)?;
        for item in items
            .iter_mut()
            .filter(|e| e.kind == "file" && e.indexed_path.is_none())
        {
            let absolute = s.browser.root.join(&item.path);
            let reason = if !absolute.starts_with(&indexed_workspace) {
                "Outside indexed workspace"
            } else {
                match absolute
                    .extension()
                    .and_then(|extension| extension.to_str())
                {
                    Some("ts" | "tsx") => "TypeScript indexing not supported yet",
                    Some("js" | "mjs" | "cjs" | "rs" | "java" | "py") => {
                        "Not indexed yet (may be excluded or size-limited)"
                    }
                    _ => "Unsupported source type",
                }
            };
            item.unindexed_reason = Some(reason.into());
        }
        s.store.verify_root()?;
        Ok(Json(crate::file_tree::Page {
            root: s.browser.root.to_string_lossy().into_owned(),
            indexed_workspace,
            path: q.path,
            revision,
            items,
            next_offset,
            truncated,
        }))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "directory_error",
            "Directory worker failed",
        )
    })?
}
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PinQuery {
    index_generation: Option<String>,
    index_revision: Option<String>,
}
impl PinQuery {
    fn pin(&self) -> Result<Option<IndexPin>, ApiError> {
        match (&self.index_generation, &self.index_revision) {
            (None, None) => Ok(None),
            (Some(generation), Some(revision)) => {
                let revision = revision.parse::<u64>().map_err(|_| invalid())?;
                let value = json!({"indexGeneration":generation,"indexRevision":revision});
                Ok(Some(serde_json::from_value(value).map_err(|_| invalid())?))
            }
            _ => Err(invalid()),
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesQuery {
    #[serde(flatten)]
    pin: PinQuery,
    #[serde(default)]
    offset: usize,
    #[serde(default = "file_limit")]
    limit: usize,
}
fn file_limit() -> usize {
    200
}
fn browse_invalid() -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "Invalid browse or sequence request",
    )
}
#[cfg(feature = "test-support")]
async fn files(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<FilesQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = query.map_err(|_| browse_invalid())?;
    if !(1..=200).contains(&q.limit) || q.offset > i64::MAX as usize {
        return Err(browse_invalid());
    }
    let pin = q.pin.pin()?;
    Ok(Json(
        db(s, move |s| s.files_at(pin, q.offset, q.limit)).await?,
    ))
}
#[cfg(feature = "test-support")]
async fn methods(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<SourceQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = query.map_err(|_| browse_invalid())?;
    if q.path.is_empty()
        || q.path.len() > 8192
        || q.path.contains(['\0', '\\', ':'])
        || q.path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(browse_invalid());
    }
    let pin = q.pin.pin()?;
    Ok(Json(
        db(s, move |s| s.methods_at(&q.path, pin))
            .await?
            .ok_or_else(missing)?,
    ))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SequenceRequest {
    seed: String,
    expected_revision: IndexPin,
    #[serde(default)]
    show_all: bool,
}
#[cfg(feature = "test-support")]
async fn sequence(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<Json<crate::behavior::SequenceView>, ApiError> {
    let q: SequenceRequest = serde_json::from_slice(&body).map_err(|_| browse_invalid())?;
    if q.seed.is_empty() || q.seed.len() > 8192 || q.seed.contains('\0') {
        return Err(browse_invalid());
    }
    let result = tokio::task::spawn_blocking(move || {
        let mut view = s
            .store
            .sequence_at(&q.seed, q.expected_revision, q.show_all)?;
        if let Some(view) = view.as_mut()
            && let Some(catalog) = s.catalog_snapshot(view.revision)
            && let Some((_, file)) = s.store.source_at(&view.seed.path, Some(view.revision))?
        {
            crate::dependency_links::annotate(view, &file, &catalog);
        }
        Ok::<_, anyhow::Error>(view)
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
    .map_err(|e| {
        if e.chain()
            .any(|cause| cause.downcast_ref::<crate::store::PinExpired>().is_some())
            || e.to_string().starts_with("revision conflict")
            || e.to_string().starts_with("index_not_ready")
            || e.to_string().starts_with("incompatible_index")
        {
            e.into()
        } else {
            browse_invalid()
        }
    })?;
    Ok(Json(result.ok_or_else(missing)?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClassesQuery {
    path: Option<String>,
    #[serde(default)]
    q: String,
    #[serde(flatten)]
    pin: PinQuery,
    #[serde(default)]
    offset: usize,
    #[serde(default = "class_limit")]
    limit: usize,
}
fn class_limit() -> usize {
    100
}
#[cfg(feature = "test-support")]
async fn classes(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<ClassesQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::class_diagram::ClassPage>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    let pin = q.pin.pin()?;
    Ok(Json(
        db(s, move |s| {
            s.classes_at(q.path.as_deref(), &q.q, pin, q.offset, q.limit)
        })
        .await?,
    ))
}
#[cfg(feature = "test-support")]
async fn navigation(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<Json<crate::navigation::NavigationResult>, ApiError> {
    if body.len() > 32 * 1024 {
        return Err(invalid());
    }
    let request: crate::navigation::NavigationRequest =
        serde_json::from_slice(&body).map_err(|_| invalid())?;
    request.validate()?;
    Ok(Json(db(s, move |s| s.navigation_at(&request)).await?))
}

#[cfg(feature = "test-support")]
async fn class_diagram(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<Json<crate::class_diagram::ClassDiagram>, ApiError> {
    let request: crate::class_diagram::ClassDiagramRequest =
        serde_json::from_slice(&body).map_err(|_| invalid())?;
    request.validate().map_err(ApiError::from)?;
    Ok(Json(db(s, move |s| s.class_diagram_at(&request)).await?))
}

#[cfg(feature = "test-support")]
async fn status(
    State(s): State<Arc<DaemonState>>,
    uri: axum::http::Uri,
) -> Result<Json<IndexStatus>, ApiError> {
    if uri.query().is_some() {
        return Err(invalid());
    }
    Ok(Json(db(s, |s| s.status()).await?))
}
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IndexRequest {
    expected_revision: Option<IndexPin>,
}
fn now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}
impl From<crate::store::requests::Request> for IndexJob {
    fn from(row: crate::store::requests::Request) -> Self {
        let error = row
            .error_code
            .map(|code| json!({"code":code,"message":"Index job failed"}));
        Self {
            id: row.id,
            state: row.state,
            progress: IndexProgress::default(),
            revision: row.revision,
            error,
            submitted_at: row.submitted_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
        }
    }
}
async fn start_index(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<(StatusCode, Json<IndexJob>), ApiError> {
    let request: IndexRequest = if body.is_empty() {
        IndexRequest::default()
    } else {
        let value: Value = serde_json::from_slice(&body).map_err(|_| invalid())?;
        if value.get("expectedRevision").is_some_and(Value::is_null) {
            return Err(invalid());
        }
        serde_json::from_value(value).map_err(|_| invalid())?
    };
    if s.store.is_recreate_pending() && request.expected_revision.is_some() {
        return Err(ApiError::from(anyhow::anyhow!(
            "revision conflict: exceptional recovery has no decodable prior pin"
        )));
    }
    let options = s.options.clone();
    let row = db(s.clone(), move |store| {
        store.enqueue_request(&options, request.expected_revision)
    })
    .await?;
    let job = IndexJob::from(row);
    // A later durable admission supersedes the legacy exceptional display slot.
    s.jobs.lock().unwrap().current = None;
    s.pending_requests.lock().unwrap().push(job.id.clone());
    // A newly durable browser ACK starts its own attempt immediately, even if
    // an earlier exceptional storage probe was backed off.
    *s.recovery_retry_after.lock().unwrap() = None;
    s.start_queue_tick();
    Ok((StatusCode::ACCEPTED, Json(job)))
}
// Exceptional recovery has no decodable prior pin. Reserve the only job and detach
// any old daemon owner atomically, then drop that owner before the worker tries EX.
// An unrelated protected reader remains in control of BUSY; no retry is implicit.
#[allow(dead_code)]
fn start_exceptional_index(s: Arc<DaemonState>) -> Result<(StatusCode, Json<IndexJob>), ApiError> {
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let job = IndexJob {
        id: uuid::Uuid::new_v4().to_string(),
        state: "running".into(),
        progress: IndexProgress::default(),
        revision: None,
        error: None,
        submitted_at: now(),
        started_at: Some(now()),
        finished_at: None,
    };
    let old_owner = {
        let mut jobs = s.jobs.lock().unwrap();
        if jobs
            .current
            .as_ref()
            .and_then(|id| jobs.jobs.get(id))
            .is_some_and(|j| j.finished_at.is_none())
        {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "job_active",
                "An index job is already active",
            ));
        }
        if !s.store.is_recreate_pending() {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "index_not_ready",
                "Exceptional recovery disposition changed; retry explicit indexing",
            ));
        }
        // All admission paths touching both locks use jobs -> serving_session.
        let old_owner = s.serving_session.lock().unwrap().take();
        jobs.current = Some(job.id.clone());
        jobs.cancel = cancel.clone();
        jobs.jobs.insert(job.id.clone(), job.clone());
        old_owner
    };
    drop(old_owner);
    let id = job.id.clone();
    tokio::spawn(async move {
        let worker = s.clone();
        let worker_id = id.clone();
        let worker_cancel = cancel.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::index_coordinator::reconcile_workspace(
                &worker.store,
                &worker.options,
                &worker_cancel,
                |p| {
                    if let Some(j) = worker.jobs.lock().unwrap().jobs.get_mut(&worker_id) {
                        j.progress = p;
                    }
                },
            )
        })
        .await;
        let outcome = match result {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("index worker failed")),
        };
        finish_exceptional_index_job(&s, &id, outcome, &cancel, |_| {});
    });
    Ok((StatusCode::ACCEPTED, Json(job)))
}

// A successful worker keeps its writer guard inside the returned Arc even across
// spawn_blocking handoff. Validation and any test barrier run while that Arc lives.
// Install it before completing the job; never publish a success without an owner.
#[allow(dead_code)]
fn finish_exceptional_index_job(
    s: &Arc<DaemonState>,
    id: &str,
    result: anyhow::Result<(IndexPin, Arc<crate::store::topology::WriterSession>)>,
    cancel: &CancelFlag,
    before_install: impl FnOnce(&Arc<crate::store::topology::WriterSession>),
) {
    let result = result.and_then(|(pin, session)| {
        s.store.verify_writer_session(&session)?;
        anyhow::ensure!(
            s.store.status()?.revision == pin,
            "revision conflict: exceptional recovery pin changed before owner installation"
        );
        before_install(&session);
        s.store.verify_writer_session(&session)?;
        Ok((pin, session))
    });
    let mut abandoned = None;
    let mut jobs = s.jobs.lock().unwrap();
    let mut serving = s.serving_session.lock().unwrap();
    let job = jobs
        .jobs
        .get_mut(id)
        .expect("started exceptional index job");
    let published = match result {
        Ok((_pin, session)) if serving.is_some() => {
            abandoned = Some(session);
            job.state = "failed".into();
            job.error = Some(json!({"code":"index_failed","message":"Index job failed"}));
            false
        }
        Ok((pin, session)) => {
            // A late cancellation cannot undo durable activation. Retain the
            // verified owner and report the committed revision as completed.
            *serving = Some(session);
            job.revision = Some(pin);
            job.state = "done".into();
            true
        }
        Err(error) if cancel.load(Ordering::Acquire) => {
            let _ = error;
            job.state = "failed".into();
            job.error = Some(json!({"code":"index_failed","message":"Index job failed"}));
            false
        }
        Err(error) => {
            job.state = "failed".into();
            let code = if error.to_string().starts_with("revision conflict") {
                "revision_conflict"
            } else if error.to_string().starts_with("storage_busy") {
                "storage_busy"
            } else {
                "index_failed"
            };
            job.error = Some(json!({"code":code,"message":"Index job failed"}));
            false
        }
    };
    job.finished_at = Some(now());
    drop(serving);
    drop(jobs);
    drop(abandoned);
    if published {
        *s.packets.lock().unwrap() = PacketCache::default();
        s.start_dependency_index();
        s.start_queue_tick();
    }
}

// A failed publication cannot release cached packet ownership. Only a committed
// revision transition clears packets; old barriers still block their public reads.
#[allow(dead_code)]
fn finish_index_job(
    s: &Arc<DaemonState>,
    id: &str,
    result: anyhow::Result<IndexPin>,
    cancel: &CancelFlag,
) {
    let mut jobs = s.jobs.lock().unwrap();
    let job = jobs.jobs.get_mut(id).expect("started index job");
    match result {
        Ok(revision) => {
            job.state = "completed".into();
            job.revision = Some(revision);
        }
        Err(error) if cancel.load(Ordering::Acquire) => {
            let _ = error;
            job.state = "cancelled".into();
        }
        Err(error) => {
            job.state = "failed".into();
            let code = if error.to_string().starts_with("revision conflict") {
                "revision_conflict"
            } else {
                "index_failed"
            };
            job.error = Some(json!({"code":code,"message":"Index job failed"}));
        }
    }
    job.finished_at = Some(now());
    let completed = job.state == "completed";
    drop(jobs);
    if completed {
        *s.packets.lock().unwrap() = PacketCache::default();
        s.start_dependency_index();
    }
}
// The exceptional index-only recovery worker predates requests.db. Until the
// root/recovery slice moves it into the queue, its current ID remains readable
// through the authenticated routes; ordinary jobs always use durable rows.
async fn current_job(
    State(s): State<Arc<DaemonState>>,
) -> Result<Json<Option<IndexJob>>, ApiError> {
    let legacy = {
        let jobs = s.jobs.lock().unwrap();
        jobs.current
            .as_ref()
            .and_then(|id| jobs.jobs.get(id))
            .cloned()
    };
    if legacy.is_some() {
        db(s, |store| store.verify_root()).await?;
        return Ok(Json(legacy));
    }
    let row = db(s.clone(), |store| store.current_request()).await?;
    Ok(Json(row.map(|row| {
        let mut job = IndexJob::from(row);
        if job.finished_at.is_none()
            && let Some(progress) = s.job_progress.lock().unwrap().get(&job.id)
        {
            job.progress = progress.clone();
        }
        job
    })))
}
async fn job(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Result<Json<IndexJob>, ApiError> {
    let legacy = s.jobs.lock().unwrap().jobs.get(&id).cloned();
    if let Some(legacy) = legacy {
        db(s, |store| store.verify_root()).await?;
        return Ok(Json(legacy));
    }
    let row = db(s.clone(), move |store| store.request_by_id(&id))
        .await?
        .ok_or_else(missing)?;
    let mut job = IndexJob::from(row);
    if job.finished_at.is_none()
        && let Some(progress) = s.job_progress.lock().unwrap().get(&job.id)
    {
        job.progress = progress.clone();
    }
    Ok(Json(job))
}
async fn cancel_job(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Result<Json<IndexJob>, ApiError> {
    let legacy = s.jobs.lock().unwrap().jobs.get(&id).cloned();
    if let Some(legacy) = legacy {
        db(s, |store| store.verify_root()).await?;
        if legacy.finished_at.is_some() {
            return Ok(Json(legacy));
        }
        return Err(ApiError(
            StatusCode::CONFLICT,
            "request_not_cancellable",
            "Accepted requests cannot be cancelled",
        ));
    }
    let row = db(s.clone(), move |store| store.request_by_id(&id))
        .await?
        .ok_or_else(missing)?;
    if row.finished_at.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "request_not_cancellable",
            "Accepted requests cannot be cancelled",
        ));
    }
    // Terminal responses are immutable durable rows. Do not mix in late
    // process-local progress when cancel is read after the initial GET.
    Ok(Json(IndexJob::from(row)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolsQuery {
    #[serde(default)]
    q: String,
    #[serde(default = "limit")]
    limit: usize,
}
fn limit() -> usize {
    50
}
#[cfg(feature = "test-support")]
async fn symbols(
    State(s): State<Arc<DaemonState>>,
    Query(q): Query<SymbolsQuery>,
) -> Result<Json<Value>, ApiError> {
    if q.q.len() > 8192 || !(1..=150).contains(&q.limit) {
        return Err(invalid());
    }
    let (revision, items) = db(s, move |s| s.symbols_at(&q.q, q.limit)).await?;
    Ok(Json(json!({"revision":revision,"items":items})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SymbolQuery {
    id: String,
    #[serde(flatten)]
    pin: PinQuery,
}
#[cfg(feature = "test-support")]
async fn symbol(
    State(s): State<Arc<DaemonState>>,
    Query(q): Query<SymbolQuery>,
) -> Result<Json<Value>, ApiError> {
    if q.id.is_empty() || q.id.len() > 8192 || q.id.contains('\0') {
        return Err(invalid());
    }
    let pin = q.pin.pin()?;
    let (revision, symbol) = db(s, move |s| s.symbol_at(&q.id, pin))
        .await?
        .ok_or_else(missing)?;
    Ok(Json(json!({"revision":revision,"symbol":symbol})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceQuery {
    path: String,
    #[serde(flatten)]
    pin: PinQuery,
}
#[cfg(feature = "test-support")]
async fn source(
    State(s): State<Arc<DaemonState>>,
    Query(q): Query<SourceQuery>,
) -> Result<Json<Value>, ApiError> {
    if q.path.is_empty()
        || q.path.len() > 8192
        || q.path.contains(['\0', '\\', ':'])
        || q.path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(invalid());
    }
    let pin = q.pin.pin()?;
    let (revision, file) = db(s, move |s| s.source_at(&q.path, pin))
        .await?
        .ok_or_else(missing)?;
    Ok(Json(json!({"revision":revision,"file":file})))
}
#[cfg(feature = "test-support")]
async fn query(
    State(s): State<Arc<DaemonState>>,
    pin: Result<Query<PinQuery>, axum::extract::rejection::QueryRejection>,
    body: Result<Json<ViewQuery>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<ViewResult>, ApiError> {
    let Query(pin) = pin.map_err(|_| invalid())?;
    let expected = pin.pin()?;
    let Json(q) = body.map_err(|_| invalid())?;
    q.validate().map_err(|_| invalid())?;
    Ok(Json(
        db(s, move |s| s.query_view_at(&q, expected.as_ref()))
            .await?
            .ok_or_else(missing)?,
    ))
}
#[cfg(feature = "test-support")]
async fn views(
    State(s): State<Arc<DaemonState>>,
    pin: Result<Query<PinQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Vec<SavedViewState>>, ApiError> {
    let Query(pin) = pin.map_err(|_| invalid())?;
    let expected = pin.pin()?;
    Ok(Json(db(s, move |s| s.saved_views_at(expected)).await?))
}
#[cfg(feature = "test-support")]
async fn view(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
    pin: Result<Query<PinQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<SavedViewState>, ApiError> {
    validate_record_id(&id).map_err(|_| invalid())?;
    let Query(pin) = pin.map_err(|_| invalid())?;
    let expected = pin.pin()?;
    Ok(Json(
        db(s, move |s| s.saved_view_at(&id, expected))
            .await?
            .ok_or_else(missing)?,
    ))
}
#[cfg(feature = "test-support")]
async fn save_view(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
    pin: Result<Query<PinQuery>, axum::extract::rejection::QueryRejection>,
    body: Result<Json<SavedViewRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<SavedViewState>, ApiError> {
    let Query(pin) = pin.map_err(|_| invalid())?;
    let pin = pin.pin()?.ok_or_else(invalid)?;
    let Json(v) = body.map_err(|_| invalid())?;
    if v.id != id {
        return Err(invalid());
    }
    v.validate().map_err(|_| invalid())?;
    Ok(Json(db(s, move |s| s.save_view_at(pin, &v)).await?))
}
#[cfg(feature = "test-support")]
async fn delete_view(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    validate_record_id(&id).map_err(|_| invalid())?;
    db(s, move |s| s.delete_view(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}
#[cfg(feature = "test-support")]
async fn annotations(
    State(s): State<Arc<DaemonState>>,
    pin: Result<Query<PinQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Vec<AnnotationState>>, ApiError> {
    let Query(pin) = pin.map_err(|_| invalid())?;
    let expected = pin.pin()?;
    Ok(Json(
        db(s, move |s| s.saved_annotations_at(expected)).await?,
    ))
}
#[cfg(feature = "test-support")]
async fn save_annotation(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
    pin: Result<Query<PinQuery>, axum::extract::rejection::QueryRejection>,
    body: Result<Json<AnnotationRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<AnnotationState>, ApiError> {
    let Query(pin) = pin.map_err(|_| invalid())?;
    let pin = pin.pin()?.ok_or_else(invalid)?;
    let Json(a) = body.map_err(|_| invalid())?;
    if a.id != id {
        return Err(invalid());
    }
    a.validate().map_err(|_| invalid())?;
    Ok(Json(db(s, move |s| s.save_annotation_at(pin, &a)).await?))
}
#[cfg(feature = "test-support")]
async fn delete_annotation(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    validate_record_id(&id).map_err(|_| invalid())?;
    db(s, move |s| s.delete_annotation(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// These endpoints only transform locally indexed evidence. They never contact a provider.
fn question_error(e: anyhow::Error) -> ApiError {
    let message = e.to_string();
    if e.chain()
        .any(|cause| cause.downcast_ref::<crate::store::PinExpired>().is_some())
        || message.starts_with("revision conflict")
        || [
            "root_changed",
            "root_key_collision",
            "workspace_id_changed",
            "index_not_ready",
            "storage_busy",
            "incompatible_index",
            "incompatible_record",
            "incomplete_record",
            "recovery_required",
            "unsafe_index",
        ]
        .iter()
        .any(|prefix| message.starts_with(prefix))
        || e.chain().any(|cause| {
            cause
                .downcast_ref::<rusqlite::Error>()
                .is_some_and(|error| {
                    matches!(error, rusqlite::Error::SqliteFailure(info, _) if matches!(info.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
                })
        })
    {
        e.into()
    } else if message == "question seed not found" {
        missing()
    } else if message.contains("176000") {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "evidence_too_large",
            "Jev request exceeds 176000 bytes. Narrow the evidence scope; complete source files cannot be truncated.",
        )
    } else if message.contains("1 MiB") {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "evidence_too_large",
            "Complete question packet exceeds 1 MiB. Narrow the evidence scope; sources cannot be truncated.",
        )
    } else {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_question_selection",
            "Invalid question or selection. Use this packet's exact candidate IDs and provide one valid decision per candidate.",
        )
    }
}
#[cfg(feature = "test-support")]
async fn question_work<T: Send + 'static>(
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Operation failed",
            )
        })?
        .map_err(question_error)
}
#[cfg(feature = "test-support")]
async fn question_preview(
    State(s): State<Arc<DaemonState>>,
    body: Result<Json<QuestionRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(request) = body.map_err(|_| invalid())?;
    request.validate().map_err(|_| invalid())?;
    question_work(move || {
        let response = s.store.evidence_response()?;
        response.validate_pin(request.expected_revision)?;
        let packet = planning::prepare_in(&response, request)?;
        let selection = planning::preview(&packet)?;
        let view = planning::assemble(&packet, &selection, "localPreview")?;
        let bytes = serde_json::to_vec(&packet)?.len();
        anyhow::ensure!(
            bytes <= MAX_PACKET_BYTES,
            "complete question packet exceeds 1 MiB"
        );
        let cached = Arc::new(packet.clone());
        let result = Json(QuestionPreview {
            packet,
            selection,
            view,
        })
        .into_response();
        // Admission to the cache is part of response assembly, before the final fence.
        s.packets
            .lock()
            .unwrap()
            .remember_fenced(cached, bytes, || {
                #[cfg(test)]
                if let Some(hook) = s.preview_finish_hook.lock().unwrap().as_ref() {
                    hook();
                }
                response.finish(())
            })?;
        Ok(result)
    })
    .await
}
#[cfg(feature = "test-support")]
async fn cached_packet(
    s: Arc<DaemonState>,
    id: String,
) -> Result<(Arc<QuestionPacket>, EvidenceFence), ApiError> {
    // Gate readiness before touching cached evidence, including stale packets.
    let response = s.store.evidence_response()?;
    let revision = response.status()?.revision;
    let packet = {
        let cache = s.packets.lock().unwrap();
        cache
            .packets
            .iter()
            .find(|(p, _)| p.packet_id == id)
            .map(|(p, _)| p.clone())
            .ok_or_else(missing)?
    };
    if revision != packet.revision {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "revision_conflict",
            "The index revision changed",
        ));
    }
    // A packet hash authenticates only the in-memory copy. Recheck its exact
    // selected graph and source witnesses under one pinned SQLite snapshot
    // before exporting, displaying, or sending cached evidence to a provider.
    let witness = packet.clone();
    response.validate_selected_view(&witness.context, &witness.source_files)?;
    let fence = response.into_fence(EvidenceFencePolicy::ExactPin(packet.revision));
    Ok((packet, fence))
}
// Always run the post-materialization fence, even when assembly fails. Preserve
// the original materialization error, as Store::with_evidence does.
#[cfg(feature = "test-support")]
fn finish_packet_result(
    fence: &EvidenceFence,
    result: Result<Response, ApiError>,
) -> Result<Response, ApiError> {
    let checked = fence.finish(()).map_err(ApiError::from);
    match result {
        Err(error) => Err(error),
        Ok(response) => {
            checked?;
            Ok(response)
        }
    }
}
#[cfg(feature = "test-support")]
async fn question_export(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let (packet, fence) = cached_packet(s, id).await?;
    let result = question_work(move || jev::request_for(&packet))
        .await
        .map(|value| Json(value).into_response());
    finish_packet_result(&fence, result)
}
#[derive(Serialize)]
#[cfg(feature = "test-support")]
struct QuestionSelection {
    selection: SelectionEnvelope,
    view: FocusedView,
}
#[cfg(feature = "test-support")]
async fn question_import(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
    Json(response): Json<Value>,
) -> Result<Response, ApiError> {
    let (packet, fence) = cached_packet(s, id).await?;
    let result = question_work(move || {
        let selection = jev::parse_response(&packet, &response)?;
        let warnings = jev::response_warnings(&response);
        let mut view = planning::assemble(&packet, &selection, "importedJev")?;
        view.warnings.extend(warnings);
        Ok(QuestionSelection { selection, view })
    })
    .await
    .map(|value| Json(value).into_response());
    finish_packet_result(&fence, result)
}
#[cfg(feature = "test-support")]
async fn question_selection(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
    Json(selection): Json<SelectionEnvelope>,
) -> Result<Response, ApiError> {
    let (packet, fence) = cached_packet(s, id).await?;
    let result = question_work(move || {
        let view = planning::assemble(&packet, &selection, "manual")?;
        Ok(QuestionSelection { selection, view })
    })
    .await
    .map(|value| Json(value).into_response());
    finish_packet_result(&fence, result)
}

// Status only reads the local ledger. No endpoint other than jev-run contacts Jev.
#[cfg(feature = "test-support")]
async fn acp_status(State(s): State<Arc<DaemonState>>) -> Result<Json<Value>, ApiError> {
    let Some(provider) = s.acp.clone() else {
        return Ok(Json(json!({"enabled":false,"status":null})));
    };
    let status = tokio::task::spawn_blocking(move || provider.status())
        .await
        .map_err(|_| acp_error(anyhow::anyhow!("status unavailable")))?
        .map_err(acp_error)?;
    Ok(Json(json!({"enabled":true,"status":status})))
}
fn acp_error(e: anyhow::Error) -> ApiError {
    match e.to_string().as_str() {
        "ACP allowance exhausted" => ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "acp_exhausted",
            "ACP attempt allowance exhausted",
        ),
        "ACP invalid answer" => ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "acp_invalid_answer",
            "ACP answer failed citation validation. Any reservation is retained; no automatic retry was made.",
        ),
        "ACP invalid request" => ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "acp_invalid_request",
            "Evidence exceeds the ACP request limits. Prepare a smaller packet. No provider call was made.",
        ),
        "ACP authentication required" => ApiError(
            StatusCode::BAD_GATEWAY,
            "acp_auth_required",
            "ACP could not verify Claude subscription access. Check the Claude Code subscription sign-in on this daemon host. API-key fallback is disabled. The attempt reservation is retained; no automatic retry was made.",
        ),
        "ACP model unavailable" => ApiError(
            StatusCode::BAD_GATEWAY,
            "acp_model_unavailable",
            "Sonnet is unavailable to this Claude subscription session. Check account access to Sonnet before retrying. No alternate model was used. The attempt reservation is retained; no automatic retry was made.",
        ),
        "ACP model mismatch" => ApiError(
            StatusCode::BAD_GATEWAY,
            "acp_model_mismatch",
            "ACP could not confirm Sonnet was selected. Check the configured runner and Claude adapter before retrying. No answer was accepted. The attempt reservation is retained; no automatic retry was made.",
        ),
        _ => ApiError(
            StatusCode::BAD_GATEWAY,
            "acp_failed",
            "ACP attempt failed. Any reservation is retained; no automatic retry was made.",
        ),
    }
}
#[cfg(feature = "test-support")]
async fn question_answer(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    // Accept only an empty object: never take client prompts, sources or provider overrides.
    let request: Value = serde_json::from_slice(&body).map_err(|_| invalid())?;
    if !request.as_object().is_some_and(|object| object.is_empty()) {
        return Err(invalid());
    }
    db(s.clone(), |store| store.status().map(|_| ())).await?;
    let provider = s.acp.clone().ok_or(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "acp_disabled",
        "Live ACP is disabled",
    ))?;
    let (packet, admission) = cached_packet(s.clone(), id).await?;
    // The selected packet has been attested. Never retain its DELETE-journal
    // reader during an external provider call: H may need to COMMIT meanwhile.
    admission.finish(()).map_err(ApiError::from)?;
    drop(admission);
    let response = provider.run(&packet).await;
    // Recheck on provider success AND failure. A fresh selected snapshot must
    // still match the exact pin, source bytes, and graph witnesses.
    let selected = s.store.evidence_response()?;
    if selected.status()?.revision != packet.revision {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "revision_conflict",
            "The index revision changed",
        ));
    }
    selected.validate_selected_view(&packet.context, &packet.source_files)?;
    let fence = selected.into_fence(EvidenceFencePolicy::ExactPin(packet.revision));
    let result = response.map_err(acp_error).map(|response| {
        Json(
            json!({"packetId":packet.packet_id,"revision":packet.revision,
            "source":"liveAcp","attemptId":response.attempt_id,"answer":response.answer,
            "latencyMs":response.latency_ms,"estimatedUsd":response.estimated_usd}),
        )
        .into_response()
    });
    finish_packet_result(&fence, result)
}

#[cfg(feature = "test-support")]
async fn jev_status(State(s): State<Arc<DaemonState>>) -> Result<Json<Value>, ApiError> {
    let Some(provider) = s.provider.clone() else {
        return Ok(Json(json!({"enabled":false,"budget":null})));
    };
    let budget = db(s, move |_| provider.budget()).await?;
    Ok(Json(json!({"enabled":true,"budget":budget})))
}
fn live_jev_error(e: anyhow::Error) -> ApiError {
    let message = e.to_string();
    if message.starts_with("Jev budget exhausted") {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "jev_budget_exhausted",
            "Jev budget exhausted",
        )
    } else if message.starts_with("invalid Jev request") {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_jev_request",
            "Jev request is invalid or exceeds the request limit. Reduce evidence depth or choose a smaller root, then prepare a new preview. Complete source files cannot be truncated. No provider call was made.",
        )
    } else if message.starts_with("Jev requires at least one candidate") {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "jev_no_candidates",
            "This packet has no candidate calls. Choose a root with outgoing calls and prepare a new preview. No provider call was made.",
        )
    } else if message == "Jev attempt failed: invalid_selection" {
        ApiError(
            StatusCode::BAD_GATEWAY,
            "jev_invalid_response",
            "The provider response failed validation and was saved for inspection. The attempt reservation is retained; no automatic retry was made.",
        )
    } else if message == "Jev attempt failed: context_exceeded" {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "jev_context_exceeded",
            "The provider context limit was exceeded. Reduce evidence depth or choose a smaller root, then prepare a new preview. Complete source files cannot be truncated. The attempt reservation is retained; no automatic retry was made.",
        )
    } else {
        ApiError(
            StatusCode::BAD_GATEWAY,
            "jev_failed",
            "Jev attempt failed. Any reservation is retained; no automatic retry was made.",
        )
    }
}

#[cfg(feature = "test-support")]
async fn question_run(
    State(s): State<Arc<DaemonState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    // No caller-supplied source, credentials, URL, or budget overrides.
    if !body.is_empty() && body.as_ref() != b"{}" {
        return Err(invalid());
    }
    let provider = s.provider.clone().ok_or(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "jev_disabled",
        "Live Jev is disabled",
    ))?;
    let (packet, admission) = cached_packet(s.clone(), id).await?;
    // Never retain a DELETE-journal reader across an external provider call.
    // The packet was fully attested on the selected snapshot before this call.
    admission.finish(()).map_err(ApiError::from)?;
    drop(admission);
    let response = provider.run(&packet).await;
    // Recheck even failed attempts. A fresh selected snapshot both admits the
    // current head and reattests the packet's source and graph witnesses.
    let selected = s.store.evidence_response()?;
    if selected.status()?.revision != packet.revision {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "revision_conflict",
            "The index revision changed",
        ));
    }
    selected.validate_selected_view(&packet.context, &packet.source_files)?;
    let fence = selected.into_fence(EvidenceFencePolicy::ExactPin(packet.revision));
    let result = match response.map_err(live_jev_error) {
        Err(error) => Err(error),
        Ok(response) => {
            let selection = response.selection.clone();
            question_work(move || planning::assemble(&packet, &selection, "liveJev"))
                .await
                .map(|mut view| {
                    view.warnings.extend(response.warnings.clone());
                    Json(json!({"selection":response.selection,"view":view,
                        "attemptId":response.attempt_id,"latencyMs":response.latency_ms,
                        "estimatedUsd":response.estimated_usd,"usage":response.usage,
                        "warnings":response.warnings}))
                    .into_response()
                })
        }
    };
    finish_packet_result(&fence, result)
}

#[cfg(all(test, feature = "test-support"))]
mod live_tests {
    use super::*;
    use crate::indexer::index_workspace_bundle;
    use tower::ServiceExt;

    #[test]
    fn outer_http_fence_covers_native_composition_not_durable_payloads() {
        for path in [
            "/api/status",
            "/api/tree",
            "/api/files",
            "/api/methods",
            "/api/sequence",
            "/api/classes",
            "/api/class-diagram",
            "/api/navigation",
            "/api/symbols",
            "/api/symbol",
            "/api/source",
            "/api/query",
            "/api/dependencies",
            "/api/dependencies/symbols",
            "/api/dependencies/source",
            "/api/rust-sources/tree",
        ] {
            assert!(native_response_route(path), "{path}");
        }
        for path in [
            "/api/views",
            "/api/views/saved",
            "/api/annotations",
            "/api/rust-sources/file",
            "/api/healthz",
            "/api/jobs/current",
        ] {
            assert!(!native_response_route(path), "{path}");
        }
    }
    #[test]
    fn question_error_keeps_typed_refusals_and_selection_errors() {
        for (message, code) in [
            ("root_changed: fixture", "root_changed"),
            ("recovery_required: fixture", "recovery_required"),
            ("unsafe_index: fixture", "unsafe_index"),
            ("index_not_ready: fixture", "index_not_ready"),
        ] {
            assert_eq!(question_error(anyhow::anyhow!("{message}")).1, code);
        }
        assert_eq!(
            question_error(anyhow::anyhow!("revision conflict")).1,
            "revision_conflict"
        );
        assert_eq!(
            question_error(anyhow::anyhow!("invalid selection")).1,
            "invalid_question_selection"
        );
        assert_eq!(
            question_error(anyhow::anyhow!("complete question packet exceeds 1 MiB")).1,
            "evidence_too_large"
        );
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );
        assert_eq!(question_error(busy.into()).1, "storage_busy");
    }

    #[test]
    fn api_error_walks_context_but_never_masks_sqlite_ioerr_rdlock() {
        let busy =
            anyhow::anyhow!("storage_busy: SQLite lock contention").context("durable enqueue");
        let mapped = ApiError::from(busy);
        assert_eq!((mapped.0, mapped.1), (StatusCode::CONFLICT, "storage_busy"));
        let root = anyhow::anyhow!("root_changed: pathname replaced").context("durable enqueue");
        let mapped = ApiError::from(root);
        assert_eq!((mapped.0, mapped.1), (StatusCode::CONFLICT, "root_changed"));
        let ioerr = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR_RDLOCK),
            None,
        );
        let mapped = ApiError::from(
            anyhow::Error::new(ioerr).context("storage_busy: wrapped database I/O failure"),
        );
        assert_eq!(
            (mapped.0, mapped.1),
            (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        );
    }

    #[test]
    fn preview_fence_failure_restores_evicted_and_same_id_packets() {
        let revision = IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 1,
        };
        let packet = QuestionPacket {
            packet_id: "original".into(),
            revision,
            request: QuestionRequest {
                seed: "seed".into(),
                question: "what?".into(),
                expected_revision: revision,
                evidence_depth: 0,
                max_visible: 1,
                allow_deeper_display: false,
                focus_terms: vec![],
            },
            context: ViewResult {
                revision,
                query: ViewQuery {
                    seed: "seed".into(),
                    depth: 0,
                    max_nodes: 1,
                    max_calls: 1,
                    include_callbacks: false,
                    exclude_paths: vec![],
                },
                nodes: vec![],
                calls: vec![],
                regions: vec![],
                truncated: false,
                omitted_nodes: 0,
                warnings: vec![],
            },
            source_files: vec![],
            warnings: vec![],
        };
        let mut cache = PacketCache::default();
        for i in 0..MAX_PACKETS {
            let mut item = packet.clone();
            item.packet_id = format!("packet-{i}");
            cache.remember(Arc::new(item), MAX_PACKET_BYTES);
        }
        let original: Vec<_> = cache
            .packets
            .iter()
            .map(|(p, bytes)| (p.clone(), *bytes))
            .collect();
        let original_bytes = cache.bytes;
        let mut incoming = packet.clone();
        incoming.packet_id = "new".into();
        let failed = cache.remember_fenced(Arc::new(incoming), MAX_PACKET_BYTES, || {
            anyhow::bail!("index_not_ready: incarnation lost")
        });
        assert!(
            failed
                .unwrap_err()
                .to_string()
                .starts_with("index_not_ready")
        );
        assert_eq!(cache.bytes, original_bytes);
        assert_eq!(cache.packets.len(), MAX_PACKETS);
        for ((actual, size), (before, expected)) in cache.packets.iter().zip(&original) {
            assert!(Arc::ptr_eq(actual, before));
            assert_eq!(size, expected);
        }
        let mut replacement = packet;
        replacement.packet_id = "packet-3".into();
        assert!(
            cache
                .remember_fenced(Arc::new(replacement), MAX_PACKET_BYTES + 1, || {
                    anyhow::bail!("root_changed: root replaced")
                })
                .is_err()
        );
        assert_eq!(cache.bytes, original_bytes);
        assert_eq!(cache.packets.len(), MAX_PACKETS);
        for ((actual, size), (before, expected)) in cache.packets.iter().zip(&original) {
            assert!(Arc::ptr_eq(actual, before));
            assert_eq!(size, expected);
        }
    }

    #[tokio::test]
    async fn preview_root_change_after_serialization_discards_packet() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("a.js"), "function go() { measured(); }\n").unwrap();
        let options = IndexOptions::new(workspace.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let store = Store::open_for_tests(&dir.path().join("state"), &workspace).unwrap();
        let (graph, _, _) =
            index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
        let seed = graph
            .nodes
            .iter()
            .find(|node| node.name == "go")
            .unwrap()
            .id
            .clone();
        let (pin, session) =
            crate::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {})
                .unwrap();
        let state = new(
            store,
            options,
            "0123456789abcdef".repeat(4),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        state.retain_serving_session(session);
        let root = workspace.clone();
        *state.preview_finish_hook.lock().unwrap() = Some(Arc::new(move || {
            std::fs::rename(&root, root.with_file_name("replaced-workspace")).unwrap();
            std::fs::create_dir(&root).unwrap();
        }));
        let app = router(state.clone());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/questions/preview")
            .header("host", "127.0.0.1:7331")
            .header(
                "authorization",
                "Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"seed":seed,"question":"what does go do?","expectedRevision":pin})
                    .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "root_changed");
        assert!(body.get("packet").is_none());
        assert!(state.packets.lock().unwrap().packets.is_empty());
    }

    #[test]
    fn acp_controlled_errors_are_exact_and_sanitized() {
        for (message, code) in [
            ("ACP authentication required", "acp_auth_required"),
            ("ACP model unavailable", "acp_model_unavailable"),
            ("ACP model mismatch", "acp_model_mismatch"),
        ] {
            let known = acp_error(anyhow::anyhow!("{message}"));
            assert_eq!(known.0, StatusCode::BAD_GATEWAY);
            assert_eq!(known.1, code);
            assert!(known.2.contains("no automatic retry"));
            let untrusted = acp_error(anyhow::anyhow!("{message} private output"));
            assert_eq!(untrusted.1, "acp_failed");
            assert!(!untrusted.2.contains("private output"));
        }
    }

    #[tokio::test]
    async fn live_response_is_rejected_if_snapshot_changes_during_call() {
        mock_run(true, "success").await;
    }

    #[tokio::test]
    async fn pinned_packet_conflicts_even_when_provider_fails() {
        mock_run(true, "context_exceeded").await;
    }

    #[tokio::test]
    async fn live_response_is_labeled_and_accounted() {
        mock_run(false, "success").await;
    }

    #[tokio::test]
    async fn corrupt_selected_cached_source_prevents_live_provider_call() {
        mock_run(false, "selected_corruption").await;
    }

    #[tokio::test]
    async fn context_exceeded_is_actionable_and_retains_reservation() {
        mock_run(false, "context_exceeded").await;
    }

    #[test]
    fn live_error_classification_is_allowlisted_and_redacted() {
        for (message, status, code) in [
            (
                "invalid Jev request: private source",
                422,
                "invalid_jev_request",
            ),
            (
                "Jev requires at least one candidate",
                422,
                "jev_no_candidates",
            ),
            (
                "Jev attempt failed: context_exceeded",
                422,
                "jev_context_exceeded",
            ),
            (
                "Jev attempt failed: context_exceeded private source",
                502,
                "jev_failed",
            ),
            (
                "Jev budget exhausted: private source",
                429,
                "jev_budget_exhausted",
            ),
            (
                "Jev attempt failed: invalid_selection",
                502,
                "jev_invalid_response",
            ),
            (
                "Jev attempt failed: invalid_selection private source",
                502,
                "jev_failed",
            ),
            ("private source", 502, "jev_failed"),
        ] {
            let error = live_jev_error(anyhow::anyhow!(message));
            assert_eq!(error.0.as_u16(), status);
            assert_eq!(error.1, code);
            assert!(!error.2.contains("private source"));
        }
    }

    #[tokio::test]
    async fn rounded_live_response_warns_and_invalid_response_is_actionable() {
        mock_run(false, "rounded").await;
        mock_run(false, "invalid_selection").await;
    }

    async fn mock_run(change_snapshot: bool, scenario: &'static str) {
        // Loopback mock only: no environment credentials or external endpoint.
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let notify = entered.clone();
        let gate = release.clone();
        let mock = Router::new().route("/", post(move |Json(request): Json<Value>| {
            let notify = notify.clone();
            let gate = gate.clone();
            async move {
                notify.notify_one();
                gate.notified().await;
                if scenario == "context_exceeded" {
                    return (StatusCode::BAD_REQUEST, Json(json!({"detail":{"error_type":"max_tokens_exceeded"}}))).into_response();
                }
                let answers: serde_json::Map<String, Value> = request["questions"].as_object().unwrap()
                    .keys().map(|key| (key.clone(), json!({"type":"choice","choice":"essential","confidence":1.0,
                    "probabilities":{"essential":if scenario == "rounded" {0.69} else if scenario == "invalid_selection" {0.6} else {1.0},"supporting":if scenario == "success" {0.0} else {0.2},"incidental":if scenario == "success" {0.0} else {0.1},"uncertain":0.0}}))).collect();
                Json(json!({"model":"jev-1.13.0","answers":answers})).into_response()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(
            workspace.join("a.js"),
            "function seed() { console.log('synthetic'); }",
        )
        .unwrap();
        let options = IndexOptions::new(workspace.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let store = Store::open_for_tests(&dir.path().join("state"), &workspace).unwrap();
        store.settle_foreign_root_requests().unwrap();
        crate::store::topology::assert_topology_fixture(&store, &dir.path().join("state"));
        let (graph, native, capture) =
            index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
        let seed = graph
            .nodes
            .iter()
            .find(|node| node.name == "seed")
            .unwrap()
            .id
            .clone();
        let session = store.writer_session().unwrap();
        store
            .publish_native(
                &graph,
                &capture,
                &native,
                session.writer_guard().unwrap(),
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        let provider = Arc::new(
            LiveJev::open(
                &dir.path().join("budget"),
                "synthetic".into(),
                10,
                &workspace,
            )
            .unwrap()
            .with_test_endpoint(endpoint),
        );
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let state = new_with_jev(
            store.clone(),
            options,
            token.into(),
            "127.0.0.1:7331".parse().unwrap(),
            Some(provider.clone()),
        )
        .unwrap();
        // The provider fixture drives its own H explicitly; do not race an
        // unrelated timer watcher for the selected source change.
        state.retain_serving_session_without_tick_for_tests(session.clone());
        let app = router(state.clone());
        let request = |path: &str, body: Value| {
            axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let preview = app
            .clone()
            .oneshot(request(
                "/api/questions/preview",
                json!({"seed":seed,"question":"logging", "expectedRevision":store.status().unwrap().revision}),
            ))
            .await
            .unwrap();
        assert_eq!(preview.status(), StatusCode::OK);
        let bytes = to_bytes(preview.into_body(), 1024 * 1024).await.unwrap();
        let preview: Value = serde_json::from_slice(&bytes).unwrap();
        let path = format!(
            "/api/questions/{}/jev-run",
            preview["packet"]["packetId"].as_str().unwrap()
        );
        if scenario == "selected_corruption" {
            let index_root = dir.path().join("state/cache/indexes");
            let db_path = std::fs::read_dir(index_root)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.is_dir())
                .unwrap()
                .join("index.db");
            let db = rusqlite::Connection::open(db_path).unwrap();
            let mut bytes: Vec<u8> = db
                .query_row(
                    "SELECT v.source_bytes FROM document_versions v JOIN revision_documents m ON m.document_version_id=v.id JOIN native_revisions r ON r.id=m.revision_id JOIN index_metadata current ON current.index_revision=r.published_index_revision WHERE m.path='a.js'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            bytes[0] ^= 1;
            assert_eq!(
                db.execute(
                    "UPDATE document_versions SET source_bytes=?1 WHERE id=(SELECT m.document_version_id FROM revision_documents m JOIN native_revisions r ON r.id=m.revision_id JOIN index_metadata current ON current.index_revision=r.published_index_revision WHERE m.path='a.js')",
                    [bytes],
                )
                .unwrap(),
                1
            );
            // The background leader watcher may discover the corrupted selected
            // bytes before this read; both a still-pinned status and a typed
            // fail-closed index are valid. Neither may call the live provider.
            match store.status() {
                Ok(status) => assert_eq!(
                    status.revision,
                    serde_json::from_value(preview["packet"]["revision"].clone()).unwrap()
                ),
                Err(error) => assert!(
                    format!("{error:#}").contains("incompatible_index"),
                    "unexpected selected corruption classification: {error:#}"
                ),
            }
            let attempts = provider.budget().unwrap().attempts;
            let response = app.oneshot(request(&path, json!({}))).await.unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            assert_eq!(body["error"]["code"], "incompatible_index");
            assert_eq!(provider.budget().unwrap().attempts, attempts);
            server.abort();
            return;
        }
        let run = tokio::spawn(app.oneshot(request(&path, json!({}))));
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        if change_snapshot {
            // The provider has admitted the old packet and is paused. Change
            // captured source so the same daemon-local writer must publish H,
            // rather than taking the unchanged-head fast path.
            std::fs::write(
                workspace.join("a.js"),
                "function seed() { console.log('changed while provider waits'); }",
            )
            .unwrap();
            let publisher = store.clone();
            let owner = session.clone();
            let selected = IndexOptions::new(workspace.clone());
            let publishing = tokio::task::spawn_blocking(move || {
                crate::index_coordinator::IndexJobCoordinator::prepare_with_session(
                    &publisher, None, owner,
                )?
                .run_serving(&selected, &cancel, |_| {})
            });
            // H commits while the provider is blocked. A leaked SQLite read
            // snapshot would prevent this publication within the timeout.
            let prior = store.status().unwrap().revision;
            let committed = tokio::time::timeout(std::time::Duration::from_secs(5), publishing)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(committed.index_generation, prior.index_generation);
            assert!(committed.index_revision > prior.index_revision);
        }
        release.notify_one();
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if change_snapshot {
            assert_eq!(response.status(), StatusCode::CONFLICT);
            let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            assert!(!String::from_utf8_lossy(&body).contains("liveJev"));
        } else if scenario == "invalid_selection" {
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"]["code"], "jev_invalid_response");
            let message = body["error"]["message"].as_str().unwrap();
            assert!(message.contains("saved for inspection"));
            assert!(message.contains("no automatic retry"));
            assert!(!message.contains("probabilities"));
        } else if scenario == "context_exceeded" {
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"]["code"], "jev_context_exceeded");
            let message = body["error"]["message"].as_str().unwrap();
            assert!(message.contains("reservation is retained"));
            assert!(message.contains("cannot be truncated"));
            assert!(!message.contains("private source"));
        } else {
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["view"]["selectionSource"], "liveJev");
            assert_eq!(
                body["view"]["revision"],
                json!(store.status().unwrap().revision)
            );
            assert!(body["view"]["calls"].as_array().unwrap().len() <= 5);
            assert!(body["attemptId"].as_str().is_some());
            if scenario == "rounded" {
                let warnings = body["warnings"].as_array().unwrap();
                assert!(!warnings.is_empty());
                for warning in warnings {
                    assert!(
                        body["view"]["warnings"]
                            .as_array()
                            .unwrap()
                            .contains(warning)
                    );
                }
            } else {
                assert_eq!(body["warnings"], json!([]));
            }
        }
        assert_eq!(provider.budget().unwrap().reserved_cents, 10);
        assert_eq!(provider.budget().unwrap().attempts, 1);
        server.abort();
    }
}

#[cfg(test)]
mod serving_holder_tests {
    use super::*;
    use std::fs;

    #[test]
    fn takeover_head_manifest_options_survive_ack_and_idle_watcher() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        let defaults = IndexOptions::new(root.clone());
        let (_, old) = crate::index_coordinator::reconcile_workspace(
            &store,
            &defaults,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        drop(old);
        let mut head = defaults.clone();
        head.manifest_path = Some(root.join("optional-manifest.json"));
        let queued = store.enqueue_request(&head, None).unwrap();
        let state = new(
            store.clone(),
            defaults,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        state.queue_tick().unwrap();
        let done = store.request_by_id(&queued.id).unwrap().unwrap();
        assert_eq!(done.state, "done");
        let pin = done.revision.unwrap();
        assert_eq!(store.status().unwrap().revision, pin);
        assert_eq!(
            store
                .recorded_index_options()
                .unwrap()
                .unwrap()
                .manifest_path,
            head.manifest_path
        );
        state.queue_tick().unwrap();
        assert_eq!(
            store.status().unwrap().revision,
            pin,
            "idle watcher must not silently switch to daemon defaults"
        );
        assert_eq!(
            store
                .recorded_index_options()
                .unwrap()
                .unwrap()
                .manifest_path,
            head.manifest_path
        );
    }

    #[test]
    fn takeover_retains_reconciled_owner_before_done_becomes_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        let options = IndexOptions::new(root);
        let state = new(
            store.clone(),
            options.clone(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        assert!(state.retained_serving_session().is_err());
        let accepted = store.enqueue_request(&options, None).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(0);
        state.test_queue_after_h_drain.set(move || {
            // The terminal row is committed, but queue_tick has not returned.
            done_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        let worker = state.clone();
        let tick = std::thread::spawn(move || worker.queue_tick());
        done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("takeover did not commit the accepted head");
        let observed_row = store.request_by_id(&accepted.id);
        let observed_status = store.status();
        let observed_leader = state
            .retained_serving_session()
            .is_ok_and(|s| s.verify().is_ok());
        // Release and join even if an observed value is wrong. The old ordering
        // fails below without stranding the worker on this test's channel.
        let release_result = release_tx.send(());
        let tick_result = tick.join();
        release_result.unwrap();
        tick_result.unwrap().unwrap();
        let row = observed_row.unwrap().unwrap();
        assert_eq!(row.state, "done");
        assert_eq!(observed_status.unwrap().revision, row.revision.unwrap());
        assert!(
            observed_leader,
            "a committed done row must not precede retention of its reconciled owner"
        );
    }

    #[tokio::test]
    async fn daemon_local_writer_validates_selected_head_before_draining_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        let options = IndexOptions::new(root);
        let (old_pin, prior) = crate::index_coordinator::reconcile_workspace(
            &store,
            &options,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        drop(prior);
        let state = new(
            store.clone(),
            options.clone(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        let accepted = store.enqueue_request(&options, None).unwrap();
        state
            .pending_requests
            .lock()
            .unwrap()
            .push(accepted.id.clone());
        state.queue_tick().unwrap();
        let session = state.retained_serving_session().unwrap();
        store.verify_writer_session(&session).unwrap();
        let completed = store.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(completed.state, "done");
        // The selected H must publish before the accepted FIFO request.
        assert_eq!(
            completed.revision.unwrap().index_revision,
            old_pin.index_revision + 2
        );
    }

    #[tokio::test]
    async fn failed_pending_read_preserves_id_until_durable_completion_is_observed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        let options = IndexOptions::new(root);
        let state = new(
            store.clone(),
            options.clone(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        let request = store.enqueue_request(&options, None).unwrap();
        state
            .pending_requests
            .lock()
            .unwrap()
            .push(request.id.clone());
        state.test_pending_read_failures.store(1, Ordering::Release);
        state.queue_tick().unwrap();
        assert_eq!(
            *state.pending_requests.lock().unwrap(),
            vec![request.id.clone()]
        );
        let completed = store.request_by_id(&request.id).unwrap().unwrap();
        assert_eq!(completed.state, "done");
        state.queue_tick().unwrap();
        assert!(state.pending_requests.lock().unwrap().is_empty());
        let done = store.request_by_id(&request.id).unwrap().unwrap();
        assert_eq!(done.revision, completed.revision);
        assert_eq!(
            (done.id, done.seq, done.state.as_str()),
            (request.id, request.seq, "done")
        );
        store
            .verify_writer_session(&state.retained_serving_session().unwrap())
            .unwrap();
    }

    #[test]
    fn closed_stderr_pipe_cannot_stop_queue_diagnostics() {
        struct ClosedPipe;
        impl std::io::Write for ClosedPipe {
            fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        best_effort_queue_stderr(
            ClosedPipe,
            format_args!("queue tick failed: storage_busy\n"),
        );
        best_effort_queue_stderr(
            ClosedPipe,
            format_args!("queue takeover failed for accepted ID\n"),
        );
        // Reaching here proves a broken diagnostic pipe cannot panic the
        // timer thread or prevent its next accepted-work tick.
    }

    #[tokio::test]
    async fn verified_writer_busy_requeue_gets_bounded_backoff_before_same_ack_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        let options = IndexOptions::new(root.clone());
        let (old_pin, owner) = crate::index_coordinator::reconcile_workspace(
            &store,
            &options,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let mut options = options;
        options.max_file_bytes = 1024;
        let state = new(
            store.clone(),
            options.clone(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        state.retain_serving_session_without_tick_for_tests(owner);
        let accepted = store.enqueue_request(&options, None).unwrap();
        store.fail_next_live_publish_commit_busy();
        let retry_started = Instant::now();
        state.queue_tick().unwrap();
        let retry_finished = Instant::now();
        let scheduled_retry = *state.recovery_retry_after.lock().unwrap();
        assert!(
            scheduled_retry.is_some_and(|when| {
                when >= retry_started && when <= retry_finished + Duration::from_millis(250)
            }),
            "BUSY requeue must schedule its bounded 250ms retry"
        );
        let deferred = store.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(
            (deferred.seq, deferred.state.as_str()),
            (accepted.seq, "queued")
        );
        assert!(deferred.finished_at.is_none() && deferred.error_code.is_none());
        assert_eq!(store.index_baseline().unwrap(), old_pin);
        // This is the daemon-owned selected read path, not a second Store or
        // SQLite opener. A deferred typed BUSY must not hide the prior head.
        let selected = store.evidence_response().unwrap();
        let (served_pin, source) = selected.source_at("a.js", Some(old_pin)).unwrap().unwrap();
        assert_eq!(served_pin, old_pin);
        assert_eq!(source.text, "function a() {}\n");
        selected.finish(()).unwrap();
        drop(selected);
        *state.recovery_retry_after.lock().unwrap() =
            Some(Instant::now() + Duration::from_secs(10));
        state.queue_tick().unwrap();
        assert_eq!(
            store.request_by_id(&accepted.id).unwrap().unwrap().state,
            "queued"
        );
        *state.recovery_retry_after.lock().unwrap() = None;
        state.queue_tick().unwrap();
        let done = store.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(done.state, "done");
        assert_eq!(done.seq, accepted.seq);
        assert_eq!(
            done.revision.unwrap().index_revision,
            old_pin.index_revision + 1
        );
    }

    #[test]
    fn typed_fifo_head_read_busy_defers_h_and_retries_same_accepted_id() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        let defaults = IndexOptions::new(root.clone());
        let (old_pin, owner) = crate::index_coordinator::reconcile_workspace(
            &store,
            &defaults,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        drop(owner);
        let mut requested = defaults.clone();
        requested.max_file_bytes = 4096;
        let accepted = store.enqueue_request(&requested, None).unwrap();
        let state = new(
            store.clone(),
            defaults,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        // A local accepted ACK bypasses the earlier !pending_local probe.
        state
            .pending_requests
            .lock()
            .unwrap()
            .push(accepted.id.clone());
        state
            .test_activation_head_busy_once
            .store(true, Ordering::Release);
        let started = Instant::now();
        state.queue_tick().unwrap();
        let finished = Instant::now();
        assert!(
            state
                .recovery_retry_after
                .lock()
                .unwrap()
                .is_some_and(|at| { at >= started && at <= finished + Duration::from_millis(250) }),
            "typed FIFO head-read BUSY must schedule the 250ms retry"
        );
        assert!(
            state.serving_session.lock().unwrap().is_none(),
            "H must not start with unknown head"
        );
        assert_eq!(store.index_baseline().unwrap(), old_pin);
        let queued = store.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(
            (
                queued.id.as_str(),
                queued.seq,
                queued.state.as_str(),
                queued.options_json.as_str()
            ),
            (
                accepted.id.as_str(),
                accepted.seq,
                "queued",
                accepted.options_json.as_str()
            )
        );
        assert!(queued.finished_at.is_none() && queued.revision.is_none());
        // The immediate timer tick honors the backoff without claiming Q.
        *state.recovery_retry_after.lock().unwrap() =
            Some(Instant::now() + Duration::from_secs(10));
        state.queue_tick().unwrap();
        assert_eq!(
            store.request_by_id(&accepted.id).unwrap().unwrap().state,
            "queued"
        );
        *state.recovery_retry_after.lock().unwrap() = None;
        state.queue_tick().unwrap();
        let done = store.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(
            (
                done.id.as_str(),
                done.seq,
                done.state.as_str(),
                done.options_json.as_str()
            ),
            (
                accepted.id.as_str(),
                accepted.seq,
                "done",
                accepted.options_json.as_str()
            )
        );
        assert!(done.revision.unwrap().index_revision > old_pin.index_revision);
    }

    #[tokio::test]
    async fn transient_publish_commit_busy_cannot_fail_accepted_takeover_head() {
        use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let state_root = tmp.path().join("state");
        let original = Store::open_for_tests(&state_root, &root).unwrap();
        let options = IndexOptions::new(root.clone());
        let (old_pin, owner) = crate::index_coordinator::reconcile_workspace(
            &original,
            &options,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let roots =
            TopologyRoots::isolated_for_tests(state_root.join("cache"), state_root.join("data"));
        let follower_store = Store::open_for_tests(&state_root, &root).unwrap();
        let state = new(
            follower_store.clone(),
            options.clone(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        let ack = follower_store.enqueue_request(&options, None).unwrap();
        state.pending_requests.lock().unwrap().push(ack.id.clone());
        let encoded =
            serde_json::to_string(&crate::indexer::ReconcileOptions::from(&options)).unwrap();
        // This legacy fixture emits the plain-string pre-COMMIT BUSY. It is
        // not the direct typed mandatory-H retry hook.
        follower_store.fail_next_live_publish_commit_busy();
        drop(owner);
        // Plain-string BUSY can be retried inside the same local writer tick.
        // Typed BUSY has its own test below for a mandatory-H backoff.
        state.queue_tick().unwrap();
        let after_busy = follower_store.request_by_id(&ack.id).unwrap().unwrap();
        assert_eq!(
            (
                after_busy.id.as_str(),
                after_busy.seq,
                after_busy.options_json.as_str()
            ),
            (ack.id.as_str(), ack.seq, encoded.as_str())
        );
        assert_ne!(after_busy.state, "failed", "BUSY cannot fail accepted work");
        if after_busy.state == "queued" {
            assert!(after_busy.revision.is_none() && after_busy.finished_at.is_none());
            assert_eq!(
                follower_store.index_baseline().unwrap(),
                old_pin,
                "pre-COMMIT BUSY cannot publish part of H or Q1"
            );
            *state.recovery_retry_after.lock().unwrap() = None;
            state.queue_tick().unwrap();
        }
        let done = follower_store.request_by_id(&ack.id).unwrap().unwrap();
        assert_eq!(
            (
                done.id.as_str(),
                done.seq,
                done.options_json.as_str(),
                done.state.as_str()
            ),
            (ack.id.as_str(), ack.seq, encoded.as_str(), "done")
        );
        assert!(done.finished_at.is_some() && done.error_code.is_none());
        let done_pin = done.revision.unwrap();
        let h_pin = crate::model::IndexPin {
            index_generation: old_pin.index_generation,
            index_revision: old_pin.index_revision + 1,
        };
        assert_eq!(done_pin.index_generation, h_pin.index_generation);
        assert_eq!(done_pin.index_revision, h_pin.index_revision + 1);
        assert_eq!(follower_store.status().unwrap().revision, done_pin);
        assert!(state.retained_serving_session().unwrap().verify().is_ok());
        // Post-ready historical read authenticates the distinct H publication
        // before Q1. Drop the reader before opening native metadata.
        let read = follower_store.evidence_response().unwrap();
        read.validate_pin(old_pin).unwrap();
        read.validate_pin(h_pin).unwrap();
        read.validate_pin(done_pin).unwrap();
        assert_eq!(
            read.source_at("a.js", Some(h_pin)).unwrap().unwrap().1.text,
            "function a() {}\n"
        );
        assert_eq!(
            read.source_at("a.js", Some(done_pin))
                .unwrap()
                .unwrap()
                .1
                .text,
            "function a() {}\n"
        );
        read.finish(()).unwrap();
        drop(read);
        let db = rusqlite::Connection::open_with_flags(
            roots.index_db(&identity),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let key = format!("pin:v1:{}:{}", h_pin.index_generation, h_pin.index_revision);
        let h_json: String = db.query_row(
            "SELECT reconcile_options FROM native_revisions WHERE id=?1 AND published_index_revision=?2",
            rusqlite::params![key, h_pin.index_revision as i64],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(
            serde_json::from_str::<crate::indexer::ReconcileOptions>(&h_json).unwrap(),
            crate::indexer::ReconcileOptions::from(&options),
            "immutable H pin must retain persisted selected options"
        );
    }

    #[tokio::test]
    async fn selected_h_failure_must_not_fail_valid_q1_before_reconciliation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let state_root = tmp.path().join("state");
        let original = Store::open_for_tests(&state_root, &root).unwrap();
        let mut h = IndexOptions::new(root.clone());
        h.max_file_bytes = 128;
        let mut q1_options = h.clone();
        q1_options.max_file_bytes = 512;
        let (prior, old_owner) = crate::index_coordinator::reconcile_workspace(
            &original,
            &h,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let store = Store::open_for_tests(&state_root, &root).unwrap();
        let state = new(
            store.clone(),
            IndexOptions::new(root.clone()),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        let q1 = store.enqueue_request(&q1_options, None).unwrap();
        state.pending_requests.lock().unwrap().push(q1.id.clone());
        let encoded =
            serde_json::to_string(&crate::indexer::ReconcileOptions::from(&q1_options)).unwrap();
        assert_eq!(q1.options_json, encoded);
        // H cannot admit this source, but Q1's own larger limit could.
        let large = format!("function a() {{}}\n// {}\n", "x".repeat(180));
        assert!(large.len() > 128 && large.len() <= 512);
        fs::write(root.join("a.js"), large).unwrap();
        drop(old_owner);
        let error = state.queue_tick().unwrap_err();
        assert!(format!("{error:#}").contains("unsafe or oversized input"));
        let row = store.request_by_id(&q1.id).unwrap().unwrap();
        assert_eq!(
            (
                row.id.as_str(),
                row.seq,
                row.options_json.as_str(),
                row.state.as_str()
            ),
            (q1.id.as_str(), q1.seq, encoded.as_str(), "queued")
        );
        assert!(row.revision.is_none() && row.finished_at.is_none() && row.error_code.is_none());
        assert_eq!(store.index_baseline().unwrap(), prior);
        assert!(state.retained_serving_session().is_err());
        assert!(state.watcher_work.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn selected_h_precommit_busy_retries_h_before_fifo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let path = root.join("a.js");
        fs::write(&path, "function a() {}\n").unwrap();
        let state_root = tmp.path().join("state");
        let original = Store::open_for_tests(&state_root, &root).unwrap();
        let mut h = IndexOptions::new(root.clone());
        h.max_file_bytes = 128;
        let mut a = h.clone();
        a.max_file_bytes = 32;
        let mut b = h.clone();
        b.max_file_bytes = 512;
        let (prior, old_owner) = crate::index_coordinator::reconcile_workspace(
            &original,
            &h,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let store = Store::open_for_tests(&state_root, &root).unwrap();
        let state = new(
            store.clone(),
            IndexOptions::new(root.clone()),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        let q1 = store.enqueue_request(&a, None).unwrap();
        let q2 = store.enqueue_request(&b, None).unwrap();
        assert!(q1.seq < q2.seq && q1.id != q2.id);
        let encoded_a = serde_json::to_string(&crate::indexer::ReconcileOptions::from(&a)).unwrap();
        let encoded_b = serde_json::to_string(&crate::indexer::ReconcileOptions::from(&b)).unwrap();
        state
            .pending_requests
            .lock()
            .unwrap()
            .extend([q1.id.clone(), q2.id.clone()]);
        fs::write(&path, "function b() {}\n").unwrap();
        store.fail_next_live_publish_commit_typed_busy();
        drop(old_owner);
        state.queue_tick().unwrap(); // RetryMandatory, not an ACK or retained owner.
        for (accepted, options) in [(&q1, &encoded_a), (&q2, &encoded_b)] {
            let row = store.request_by_id(&accepted.id).unwrap().unwrap();
            assert_eq!(
                (
                    row.id.as_str(),
                    row.seq,
                    row.options_json.as_str(),
                    row.state.as_str()
                ),
                (
                    accepted.id.as_str(),
                    accepted.seq,
                    options.as_str(),
                    "queued"
                )
            );
            assert!(
                row.revision.is_none() && row.finished_at.is_none() && row.error_code.is_none()
            );
        }
        assert_eq!(store.index_baseline().unwrap(), prior);
        assert!(state.retained_serving_session().is_err());
        assert!(state.watcher_work.lock().unwrap().is_none());
        assert!(state.recovery_retry_after.lock().unwrap().is_some());
        // Only the advisory timer is advanced. Success requires a fresh H
        // publication and the same accepted FIFO rows, not elapsed time.
        *state.recovery_retry_after.lock().unwrap() = Some(Instant::now() - Duration::from_secs(1));
        state.queue_tick().unwrap();
        let first = store.request_by_id(&q1.id).unwrap().unwrap();
        let second = store.request_by_id(&q2.id).unwrap().unwrap();
        assert_eq!(
            (
                first.id.as_str(),
                first.seq,
                first.options_json.as_str(),
                first.state.as_str()
            ),
            (q1.id.as_str(), q1.seq, encoded_a.as_str(), "done")
        );
        assert_eq!(
            (
                second.id.as_str(),
                second.seq,
                second.options_json.as_str(),
                second.state.as_str()
            ),
            (q2.id.as_str(), q2.seq, encoded_b.as_str(), "queued")
        );
        assert!(first.finished_at.is_some() && first.error_code.is_none());
        let q1_pin = first.revision.unwrap();
        assert!(q1_pin.index_revision > prior.index_revision + 1);
        assert_eq!(store.status().unwrap().revision, q1_pin);
        assert!(state.retained_serving_session().unwrap().verify().is_ok());
        state.queue_tick().unwrap();
        let finished_q1 = store.request_by_id(&q1.id).unwrap().unwrap();
        let finished_q2 = store.request_by_id(&q2.id).unwrap().unwrap();
        assert_eq!(
            (finished_q1.state.as_str(), finished_q1.revision),
            ("done", Some(q1_pin))
        );
        assert_eq!(
            (
                finished_q2.id.as_str(),
                finished_q2.seq,
                finished_q2.options_json.as_str(),
                finished_q2.state.as_str()
            ),
            (q2.id.as_str(), q2.seq, encoded_b.as_str(), "done")
        );
        assert!(finished_q2.finished_at.is_some() && finished_q2.error_code.is_none());
        assert_eq!(
            finished_q2.revision.unwrap().index_revision,
            q1_pin.index_revision + 1
        );
        assert_eq!(
            store.status().unwrap().revision,
            finished_q2.revision.unwrap()
        );
    }

    #[tokio::test]
    async fn local_writer_uses_claimed_clients_options_not_daemon_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let state_root = tmp.path().join("state");
        let owner_store = Store::open_for_tests(&state_root, &root).unwrap();
        let own_options = IndexOptions::new(root.clone());
        let (_, owner) = crate::index_coordinator::reconcile_workspace(
            &owner_store,
            &own_options,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let follower_store = Store::open_for_tests(&state_root, &root).unwrap();
        let mut bad_daemon_options = own_options.clone();
        bad_daemon_options.scip_path = Some(root.clone()); // directory is an invalid optional input
        let state = new(
            follower_store.clone(),
            bad_daemon_options,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        let client = follower_store.enqueue_request(&own_options, None).unwrap();
        state
            .pending_requests
            .lock()
            .unwrap()
            .push(client.id.clone());
        drop(owner);
        state.queue_tick().unwrap();
        let done = follower_store.request_by_id(&client.id).unwrap().unwrap();
        assert_eq!(
            done.state, "done",
            "a client's valid inputs may not be failed by unrelated daemon defaults"
        );
        assert!(done.error_code.is_none() && done.revision.is_some());
        assert!(state.retained_serving_session().unwrap().verify().is_ok());
    }

    #[test]
    fn root_loss_settles_accepted_queue_and_retires_local_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function before() {}\n").unwrap();
        let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        let options = IndexOptions::new(root.clone());
        let state = new(
            store.clone(),
            options.clone(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        let accepted = store.enqueue_request(&options, None).unwrap();
        state.queue_tick().unwrap();
        assert_eq!(
            store.request_by_id(&accepted.id).unwrap().unwrap().state,
            "done"
        );
        let next = store.enqueue_request(&options, None).unwrap();
        fs::rename(&root, tmp.path().join("former-workspace")).unwrap();
        fs::create_dir(&root).unwrap();
        state.queue_tick().unwrap();
        // Normal reads reject a replaced captured root. Inspect only the durable
        // request row to prove settlement survived loss of that root.
        let db = rusqlite::Connection::open(store.request_db_path()).unwrap();
        let (status, error): (String, Option<String>) = db
            .query_row(
                "SELECT state, error_code FROM requests WHERE id=?1",
                [&next.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("root_changed"));
        assert_eq!(
            state.root_loss_retirement_for_tests(),
            (false, false, false)
        );
    }

    #[test]
    fn empty_fifo_busy_retries_selected_b_before_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let source = root.join("a.js");
        fs::write(&source, "function old() {}\n").unwrap();
        let state_root = tmp.path().join("state");
        let original = Store::open_for_tests(&state_root, &root).unwrap();
        let mut selected_b = IndexOptions::new(root.clone());
        selected_b.max_file_bytes = 4096;
        let (old_pin, owner) = crate::index_coordinator::reconcile_workspace(
            &original,
            &selected_b,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let follower_store = Store::open_for_tests(&state_root, &root).unwrap();
        let daemon_defaults_a = IndexOptions::new(root.clone());
        let state = new(
            follower_store.clone(),
            daemon_defaults_a,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        assert!(follower_store.current_request().unwrap().is_none());
        fs::write(&source, "function after_busy() {}\n").unwrap();
        // Typed pre-COMMIT BUSY defers mandatory H without exposing a
        // partially committed selected revision.
        follower_store.fail_next_live_publish_commit_typed_busy();
        drop(owner);
        // An explicit idle reconciliation intent admits a local writer even
        // without a queued request; a bare timer tick has no reason to start H.
        state.idle_h_retry.store(true, Ordering::Release);
        // Typed pre-COMMIT contention is deferred with a 250ms deadline;
        // inspect the scheduled bound before diagnostic reads can outlast it.
        let retry_started = Instant::now();
        state.queue_tick().unwrap();
        let retry_finished = Instant::now();
        let scheduled_retry = *state.recovery_retry_after.lock().unwrap();
        assert!(
            scheduled_retry.is_some_and(|t| {
                t >= retry_started && t <= retry_finished + Duration::from_millis(250)
            }),
            "failed empty-FIFO reconciliation must schedule its bounded 250ms retry"
        );
        assert_eq!(
            follower_store.index_baseline().unwrap(),
            old_pin,
            "pre-COMMIT BUSY must not publish a partial revision"
        );
        assert!(follower_store.current_request().unwrap().is_none());
        // The reads above may run beyond 250ms under CI load. Hold the same
        // retry branch open for the immediate-tick assertion without a clock race.
        *state.recovery_retry_after.lock().unwrap() =
            Some(Instant::now() + Duration::from_secs(10));
        state.queue_tick().unwrap();
        *state.recovery_retry_after.lock().unwrap() = None;
        state.queue_tick().unwrap();
        let selected = follower_store.status().unwrap().revision;
        assert_eq!(selected.index_generation, old_pin.index_generation);
        assert!(selected.index_revision > old_pin.index_revision);
        assert!(
            state
                .serving_session
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .verify()
                .is_ok()
        );
        assert_eq!(
            follower_store
                .recorded_index_options()
                .unwrap()
                .unwrap()
                .max_file_bytes,
            4096
        );
        let read = follower_store.evidence_response().unwrap();
        assert_eq!(
            read.source_at("a.js", Some(selected))
                .unwrap()
                .unwrap()
                .1
                .text,
            "function after_busy() {}\n"
        );
        read.finish(()).unwrap();
        assert!(follower_store.current_request().unwrap().is_none());
    }

    #[tokio::test]
    async fn failed_mandatory_h_takeover_backs_off_without_failing_fifo_head() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let source = root.join("a.js");
        fs::write(&source, "function a() {}\n").unwrap();
        let state_root = tmp.path().join("state");
        let owner_store = Store::open_for_tests(&state_root, &root).unwrap();
        let options = IndexOptions::new(root.clone());
        let (old_pin, owner) = crate::index_coordinator::reconcile_workspace(
            &owner_store,
            &options,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let follower_store = Store::open_for_tests(&state_root, &root).unwrap();
        let state = new(
            follower_store.clone(),
            options.clone(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        let accepted = follower_store.enqueue_request(&options, None).unwrap();
        state
            .pending_requests
            .lock()
            .unwrap()
            .push(accepted.id.clone());
        let encoded =
            serde_json::to_string(&crate::indexer::ReconcileOptions::from(&options)).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            fs::read(&source).is_err(),
            "fixture must fail real captured source reads"
        );
        drop(owner);
        let before_tick = Instant::now();
        let error = state
            .queue_tick()
            .expect_err("unreadable selected H must fail before Q1 claim");
        assert!(
            format!("{error:#}").contains("opening input:"),
            "mandatory H must fail its captured source read"
        );
        let deadline = state
            .recovery_retry_after
            .lock()
            .unwrap()
            .expect("H nonbusy failure must set an advisory retry");
        assert!(
            deadline > before_tick,
            "H retry must have a future advisory deadline"
        );
        assert!(deadline <= Instant::now() + Duration::from_millis(500));
        let after_first = follower_store.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(
            (
                after_first.id.as_str(),
                after_first.seq,
                after_first.options_json.as_str(),
                after_first.state.as_str()
            ),
            (
                accepted.id.as_str(),
                accepted.seq,
                encoded.as_str(),
                "queued"
            ),
            "mandatory H capture failure cannot be attributed to Q1"
        );
        assert!(
            after_first.revision.is_none()
                && after_first.finished_at.is_none()
                && after_first.error_code.is_none()
        );
        assert_eq!(follower_store.index_baseline().unwrap(), old_pin);
        assert!(state.retained_serving_session().is_err());
        assert!(state.watcher_work.lock().unwrap().is_none());
        // Pin the advisory deadline beyond these immediate calls so CPU load
        // cannot turn a bounded-rate assertion into elapsed-time flakiness.
        *state.recovery_retry_after.lock().unwrap() =
            Some(Instant::now() + Duration::from_secs(10));
        for _ in 0..3 {
            state.queue_tick().unwrap();
        }
        assert_eq!(
            follower_store
                .request_by_id(&accepted.id)
                .unwrap()
                .unwrap()
                .state,
            "queued"
        );
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
        // Repair H's source, then advance ONLY the advisory retry timer.
        *state.recovery_retry_after.lock().unwrap() = Some(Instant::now() - Duration::from_secs(1));
        state.queue_tick().unwrap();
        let done = follower_store.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(
            (
                done.id.as_str(),
                done.seq,
                done.options_json.as_str(),
                done.state.as_str()
            ),
            (accepted.id.as_str(), accepted.seq, encoded.as_str(), "done")
        );
        assert!(done.finished_at.is_some() && done.error_code.is_none());
        let q1_pin = done.revision.unwrap();
        let h_pin = crate::model::IndexPin {
            index_generation: old_pin.index_generation,
            index_revision: old_pin.index_revision + 1,
        };
        assert_eq!(q1_pin.index_generation, h_pin.index_generation);
        assert_eq!(q1_pin.index_revision, h_pin.index_revision + 1);
        let read = follower_store.evidence_response().unwrap();
        read.validate_pin(h_pin).unwrap();
        read.validate_pin(q1_pin).unwrap();
        assert_eq!(read.status().unwrap().revision, q1_pin);
        read.finish(()).unwrap();
        drop(read);
        assert!(state.retained_serving_session().unwrap().verify().is_ok());
    }
}

#[cfg(test)]
mod exceptional_recovery_tests {
    use super::*;
    use crate::store::topology::{TopologyRoots, WorkspaceIdentity};

    fn fixture() -> (
        tempfile::TempDir,
        Store,
        Arc<DaemonState>,
        TopologyRoots,
        WorkspaceIdentity,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            tmp.path().join("state/cache"),
            tmp.path().join("state/data"),
        );
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let old = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        // An accepted Q0 creates the durable queue witness before damage to
        // derived index.db. Recovery must retain and drain it ahead of later IDs.
        old.enqueue_request(&IndexOptions::new(root.clone()), None)
            .unwrap();
        drop(old);
        std::fs::write(roots.index_db(&identity), b"bad sqlite index header").unwrap();
        let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        assert!(store.is_recreate_pending());
        let state = new(
            store.clone(),
            IndexOptions::new(root),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        (tmp, store, state, roots, identity)
    }

    #[test]
    fn damaged_index_without_accepted_head_never_guesses_h_options() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let state_root = tmp.path().join("state");
        let roots =
            TopologyRoots::isolated_for_tests(state_root.join("cache"), state_root.join("data"));
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let old = Store::open_for_tests(&state_root, &root).unwrap();
        let options = IndexOptions::new(root.clone());
        let (_, owner) = crate::index_coordinator::reconcile_workspace(
            &old,
            &options,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        assert!(old.current_request().unwrap().is_none());
        drop(owner);
        drop(old);
        let index = roots.index_db(&identity);
        let damaged = b"bad sqlite index header";
        std::fs::write(&index, damaged).unwrap();
        let store = Store::open_for_tests(&state_root, &root).unwrap();
        assert!(store.is_recreate_pending());
        assert!(store.earliest_unfinished_request().unwrap().is_none());
        let state = new(
            store.clone(),
            options,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        let started = Instant::now();
        let error = state.queue_tick().unwrap_err();
        let finished = Instant::now();
        assert!(
            error.to_string().starts_with("recovery_required:"),
            "{error:#}"
        );
        assert!(
            state
                .recovery_retry_after
                .lock()
                .unwrap()
                .is_some_and(|at| { at >= started && at <= finished + Duration::from_millis(500) })
        );
        assert!(state.serving_session.lock().unwrap().is_none());
        assert!(store.current_request().unwrap().is_none());
        assert_eq!(std::fs::read(index).unwrap(), damaged);
    }

    #[test]
    fn damaged_index_reconciles_h_then_existing_q0_once() {
        let (_tmp, store, state, roots, identity) = fixture();
        let index = roots.index_db(&identity);
        let damaged = std::fs::read(&index).unwrap();
        let q0 = store.current_request().unwrap().unwrap();
        assert_eq!(q0.state, "queued");
        state.queue_tick().unwrap();
        assert!(!store.is_recreate_pending());
        store
            .verify_writer_session(&state.retained_serving_session().unwrap())
            .unwrap();
        let done = store.request_by_id(&q0.id).unwrap().unwrap();
        assert_eq!(done.state, "done");
        let pin = done.revision.unwrap();
        assert_eq!(pin.index_revision, 2, "H r1 must precede Q0 r2");
        assert_ne!(std::fs::read(&index).unwrap(), damaged);
        state.queue_tick().unwrap();
        assert_eq!(store.status().unwrap().revision, pin);
    }

    #[tokio::test]
    async fn damaged_index_drains_earlier_q0_before_browser_ack() {
        let (_tmp, store, state, _roots, _identity) = fixture();
        let q0 = store.current_request().unwrap().unwrap();
        let (code, Json(browser)) = start_index(State(state.clone()), Bytes::new())
            .await
            .unwrap();
        assert_eq!(code, StatusCode::ACCEPTED);
        let q1 = store.request_by_id(&browser.id).unwrap().unwrap();
        assert!(q0.seq < q1.seq);
        assert_eq!(q1.state, "queued");
        state.queue_tick().unwrap();
        let first = store.request_by_id(&q0.id).unwrap().unwrap();
        let pending = store.request_by_id(&q1.id).unwrap().unwrap();
        assert_eq!(
            (first.state.as_str(), pending.state.as_str()),
            ("done", "queued")
        );
        assert_eq!(first.revision.unwrap().index_revision, 2);
        state.queue_tick().unwrap();
        let second = store.request_by_id(&q1.id).unwrap().unwrap();
        assert_eq!(
            (second.id.as_str(), second.seq, second.state.as_str()),
            (q1.id.as_str(), q1.seq, "done")
        );
        assert_eq!(second.revision.unwrap().index_revision, 3);
        assert_eq!(
            first.revision.unwrap().index_generation,
            second.revision.unwrap().index_generation
        );
        assert_eq!(second.options_json, q1.options_json);
        store
            .verify_writer_session(&state.retained_serving_session().unwrap())
            .unwrap();
    }

    struct PausedRecoveryFixture {
        _tmp: tempfile::TempDir,
        store: Store,
        state: Arc<DaemonState>,
        roots: TopologyRoots,
        identity: WorkspaceIdentity,
        old_owner: Option<Arc<crate::store::topology::WriterSession>>,
        q2: crate::store::requests::Request,
        q1: crate::store::requests::Request,
        old_generation: uuid::Uuid,
        index_path: std::path::PathBuf,
    }

    fn paused_recovery_fixture() -> PausedRecoveryFixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            tmp.path().join("state/cache"),
            tmp.path().join("state/data"),
        );
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let index_path = roots.index_db(&identity);
        let old = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        let options = IndexOptions::new(root.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let (pin, old_owner) =
            crate::index_coordinator::reconcile_workspace(&old, &options, &cancel, |_| {}).unwrap();
        let q2 = old.enqueue_request(&options, None).unwrap();
        assert_eq!(old.claim_request(&old_owner).unwrap().unwrap().id, q2.id);
        let q1 = old.enqueue_request(&options, None).unwrap();
        assert!(q2.seq < q1.seq);
        std::fs::write(&index_path, b"bad sqlite index header").unwrap();
        let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        assert!(store.is_recreate_pending());
        let state = new(
            store.clone(),
            options,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        PausedRecoveryFixture {
            _tmp: tmp,
            store,
            state,
            roots,
            identity,
            old_owner: Some(old_owner),
            q2,
            q1,
            old_generation: pin.index_generation,
            index_path,
        }
    }

    #[tokio::test]
    async fn browser_q1_then_cli_q2_exceptional_fifo_survives_reader_contention() {
        let (_tmp, store, state, roots, identity) = fixture();
        let reader = roots.index_use_existing(&identity).unwrap();
        let q0 = store.current_request().unwrap().unwrap();
        let (status, Json(browser)) = start_index(State(state.clone()), Bytes::new())
            .await
            .unwrap();
        assert_eq!(status, StatusCode::ACCEPTED);
        let q1 = store.request_by_id(&browser.id).unwrap().unwrap();
        let cli = Store::open_for_tests(&_tmp.path().join("state"), &state.options.workspace_root)
            .unwrap();
        let q2 = cli.enqueue_request(&state.options, None).unwrap();
        assert!(q0.seq < q1.seq && q1.seq < q2.seq);
        assert_eq!(
            store.request_by_id(&q1.id).unwrap().unwrap().state,
            "queued"
        );
        assert_eq!(
            store.request_by_id(&q2.id).unwrap().unwrap().state,
            "queued"
        );
        drop(reader);
        state.queue_tick().unwrap();
        let first = store.request_by_id(&q0.id).unwrap().unwrap();
        assert_eq!(first.state, "done");
        assert_eq!(
            store.request_by_id(&q1.id).unwrap().unwrap().state,
            "queued"
        );
        assert_eq!(
            store.request_by_id(&q2.id).unwrap().unwrap().state,
            "queued"
        );
        state.queue_tick().unwrap();
        let browser_done = store.request_by_id(&q1.id).unwrap().unwrap();
        assert_eq!(browser_done.state, "done");
        assert_eq!(
            store.request_by_id(&q2.id).unwrap().unwrap().state,
            "queued"
        );
        state.queue_tick().unwrap();
        let cli_done = store.request_by_id(&q2.id).unwrap().unwrap();
        assert_eq!(
            (
                first.state.as_str(),
                browser_done.state.as_str(),
                cli_done.state.as_str()
            ),
            ("done", "done", "done")
        );
        assert_eq!(first.revision.unwrap().index_revision, 2);
        assert_eq!(browser_done.revision.unwrap().index_revision, 3);
        assert_eq!(cli_done.revision.unwrap().index_revision, 4);
        assert_eq!(
            first.revision.unwrap().index_generation,
            browser_done.revision.unwrap().index_generation
        );
        assert_eq!(
            browser_done.revision.unwrap().index_generation,
            cli_done.revision.unwrap().index_generation
        );
        assert_eq!(
            (
                browser_done.id.as_str(),
                browser_done.seq,
                browser_done.options_json.as_str()
            ),
            (q1.id.as_str(), q1.seq, q1.options_json.as_str())
        );
        assert_eq!(
            (
                cli_done.id.as_str(),
                cli_done.seq,
                cli_done.options_json.as_str()
            ),
            (q2.id.as_str(), q2.seq, q2.options_json.as_str())
        );
    }

    #[tokio::test]
    async fn cli_accepted_exceptional_request_without_local_pending_id_must_be_driven() {
        let (_tmp, store, state, _roots, _identity) = fixture();
        let q0 = store.current_request().unwrap().unwrap();
        let accepted = store.enqueue_request(&state.options, None).unwrap();
        assert!(
            state.pending_requests.lock().unwrap().is_empty(),
            "a separate CLI process cannot populate daemon-local pending_requests"
        );
        let original_queue = std::fs::read(store.request_db_path()).unwrap();
        state.queue_tick().unwrap();
        assert_eq!(store.request_by_id(&q0.id).unwrap().unwrap().state, "done");
        assert_eq!(
            store.request_by_id(&accepted.id).unwrap().unwrap().state,
            "queued"
        );
        state.queue_tick().unwrap();
        let row = store.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(
            (
                row.id.as_str(),
                row.seq,
                row.options_json.as_str(),
                row.state.as_str()
            ),
            (
                accepted.id.as_str(),
                accepted.seq,
                accepted.options_json.as_str(),
                "done"
            ),
            "next tick must claim durable CLI row without a daemon-local pending ID"
        );
        assert_eq!(row.revision.unwrap(), store.status().unwrap().revision);
        let prior = store.request_by_id(&q0.id).unwrap().unwrap();
        assert_eq!(prior.state, "done");
        assert_eq!(prior.revision.unwrap().index_revision, 2);
        assert_eq!(
            row.revision.unwrap().index_revision,
            3,
            "H r1 and earlier accepted Q0 r2 precede CLI r3"
        );
        assert!(state.retained_serving_session().unwrap().verify().is_ok());
        store
            .verify_writer_session(&state.retained_serving_session().unwrap())
            .unwrap();
        assert_ne!(
            std::fs::read(store.request_db_path()).unwrap(),
            original_queue,
            "accepted row needs a durable terminal transition"
        );
    }

    fn assert_reclaimed_fifo(
        f: &PausedRecoveryFixture,
        old_incarnation: uuid::Uuid,
        queue_inode: u64,
    ) {
        use std::os::unix::fs::MetadataExt;
        let q2 = f.store.request_by_id(&f.q2.id).unwrap().unwrap();
        let q1 = f.store.request_by_id(&f.q1.id).unwrap().unwrap();
        assert_eq!((q2.state.as_str(), q1.state.as_str()), ("done", "done"));
        assert_eq!(q2.revision.unwrap().index_revision, 2);
        assert_eq!(q1.revision.unwrap().index_revision, 3);
        assert_eq!(
            q2.revision.unwrap().index_generation,
            q1.revision.unwrap().index_generation
        );
        assert_ne!(q1.revision.unwrap().index_generation, f.old_generation);
        assert_eq!(f.store.status().unwrap().revision, q1.revision.unwrap());
        assert_eq!(
            std::fs::metadata(f.store.request_db_path()).unwrap().ino(),
            queue_inode,
            "index-only recreation must not replace requests.db"
        );
        let session = f.state.retained_serving_session().unwrap();
        f.store.verify_writer_session(&session).unwrap();
        assert_ne!(session.incarnation(), old_incarnation);
        f.store.verify_writer_session(&session).unwrap();
    }

    #[test]
    fn independent_shared_reader_process() {
        use std::io::{Read, Write};
        let Some(path) = std::env::var_os("TRELLIS_TEST_INDEX_USE_SH") else {
            return;
        };
        let guard = crate::store::topology::UseGuard::acquire_existing(
            std::path::Path::new(&path),
            false,
            false,
        )
        .unwrap();
        println!("SH_READY");
        std::io::stdout().flush().unwrap();
        let mut signal = [0];
        std::io::stdin().read_exact(&mut signal).unwrap();
        drop(guard);
    }

    #[tokio::test]
    async fn idle_tick_cannot_drain_request_admitted_after_empty_pending_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            tmp.path().join("state/cache"),
            tmp.path().join("state/data"),
        );
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let index_path = roots.index_db(&identity);
        let old = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        let options = IndexOptions::new(root.clone());
        let (prior, old_owner) = crate::index_coordinator::reconcile_workspace(
            &old,
            &options,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        std::fs::write(&index_path, b"bad sqlite index header").unwrap();
        let corrupt_bytes = std::fs::read(&index_path).unwrap();
        let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        assert!(store.is_recreate_pending());
        let state = new(
            store.clone(),
            options.clone(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        // Own the old serving SH without starting the periodic timer. Drive the
        // exact empty-snapshot race with one explicitly paused tick instead.
        *state.serving_session.lock().unwrap() = Some(old_owner);
        let reader = roots.index_use_existing(&identity).unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        state.test_queue_after_pending_snapshot.set(move || {
            let _ = entered_tx.send(());
            release_rx.recv().unwrap();
        });
        let worker = state.clone();
        let idle_tick = tokio::task::spawn_blocking(move || worker.queue_tick());
        tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
            .await
            .expect("idle tick did not snapshot empty pending list")
            .unwrap();
        let request = store.enqueue_request(&options, None).unwrap();
        state
            .pending_requests
            .lock()
            .unwrap()
            .push(request.id.clone());
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), idle_tick)
            .await
            .expect("idle tick did not leave the snapshot barrier")
            .unwrap()
            .unwrap();
        let admitted = store.request_by_id(&request.id).unwrap().unwrap();
        assert_eq!(
            admitted.state, "queued",
            "idle tick drained a post-snapshot recovery request"
        );
        assert!(admitted.finished_at.is_none());
        assert!(
            state.retained_serving_session().is_err(),
            "durable post-snapshot ACK prompted EX attempt and released old holder; independent reader still fences recreation"
        );
        let worker = state.clone();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::task::spawn_blocking(move || worker.queue_tick()),
        )
        .await
        .expect("recovery tick did not attempt protected EX")
        .unwrap()
        .unwrap();
        assert!(state.retained_serving_session().is_err());
        assert_eq!(std::fs::read(&index_path).unwrap(), corrupt_bytes);
        let blocked = store.request_by_id(&request.id).unwrap().unwrap();
        assert_eq!(blocked.state, "queued");
        assert!(blocked.finished_at.is_none());
        drop(reader);
        // Advance the advisory retry deadline without sleeping. The held SH
        // had fenced EX; the durable row remains queued for this next tick.
        *state.recovery_retry_after.lock().unwrap() = None;
        let worker = state.clone();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || worker.queue_tick()),
        )
        .await
        .expect("accepted request did not retry after foreign reader release")
        .unwrap()
        .unwrap();
        let done = store.request_by_id(&request.id).unwrap().unwrap();
        assert_eq!(done.state, "done");
        assert_eq!(done.revision.unwrap().index_revision, 2);
        assert_ne!(
            done.revision.unwrap().index_generation,
            prior.index_generation
        );
    }

    #[tokio::test]
    async fn timer_tick_with_independent_shared_reader_defers_without_changing_both_dbs() {
        use std::io::{BufRead, Write};
        use std::os::unix::fs::MetadataExt;
        use std::process::{Command, Stdio};
        let mut fixture = paused_recovery_fixture();
        let old_incarnation = fixture.old_owner.as_ref().unwrap().incarnation();
        let queue_path = fixture.store.request_db_path();
        let queue_inode = std::fs::metadata(&queue_path).unwrap().ino();
        let queue_bytes = std::fs::read(&queue_path).unwrap();
        let corrupt_index = std::fs::read(&fixture.index_path).unwrap();
        let lock_path = fixture.roots.index_use_lock(&fixture.identity);
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    let _ = self.0.kill();
                }
                let _ = self.0.wait();
            }
        }
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "http::exceptional_recovery_tests::independent_shared_reader_process",
                    "--nocapture",
                ])
                .env("TRELLIS_TEST_INDEX_USE_SH", &lock_path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdout = child.0.stdout.take().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let drain = tokio::task::spawn_blocking(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut ready = Some(ready_tx);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                if line.contains("SH_READY")
                    && let Some(sender) = ready.take()
                {
                    let _ = sender.send(());
                }
            }
            assert!(ready.is_none(), "reader exited before SH_READY");
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx)
            .await
            .expect("independent SH reader did not start")
            .unwrap();
        let (selected_tx, selected_rx) = tokio::sync::oneshot::channel();
        let (release_select_tx, release_select_rx) = std::sync::mpsc::channel();
        fixture.store.set_queue_select_hook(move || {
            let _ = selected_tx.send(());
            let _ = release_select_rx.recv();
        });
        fixture
            .state
            .pending_requests
            .lock()
            .unwrap()
            .push(fixture.q1.id.clone());
        fixture
            .state
            .retain_serving_session(fixture.old_owner.take().unwrap());
        tokio::time::timeout(std::time::Duration::from_secs(5), selected_rx)
            .await
            .expect("20-ms timer did not pause in protected SELECT")
            .unwrap();
        assert_eq!(std::fs::read(&queue_path).unwrap(), queue_bytes);
        assert_eq!(std::fs::read(&fixture.index_path).unwrap(), corrupt_index);
        release_select_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if fixture.state.retained_serving_session().is_err()
                    && fixture.state.native_stream.try_lock().is_ok()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("tick did not defer after external SH prevented EX");
        assert_eq!(
            std::fs::read(&queue_path).unwrap(),
            queue_bytes,
            "corruption-only EX BUSY must not change queued/running rows"
        );
        assert_eq!(std::fs::read(&fixture.index_path).unwrap(), corrupt_index);
        assert!(
            fixture
                .state
                .pending_requests
                .lock()
                .unwrap()
                .contains(&fixture.q1.id)
        );
        let db = rusqlite::Connection::open(&queue_path).unwrap();
        for (id, expected) in [(&fixture.q2.id, "running"), (&fixture.q1.id, "queued")] {
            let (state, finished): (String, Option<String>) = db
                .query_row(
                    "SELECT state,finished_at FROM requests WHERE id=?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(state, expected);
            assert!(finished.is_none());
        }
        drop(db);
        child.0.stdin.take().unwrap().write_all(b"x").unwrap();
        let child_status = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("independent SH reader did not exit after release");
        assert!(
            child_status.success(),
            "independent SH reader exited unsuccessfully"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), drain)
            .await
            .expect("child output drain did not finish")
            .unwrap();
        let mut last_seen = None;
        let timeout_result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match fixture.store.request_by_id(&fixture.q1.id) {
                    Ok(Some(row)) if row.finished_at.is_some() => break row,
                    Ok(Some(row)) => last_seen = Some((row.state, row.error_code)),
                    Err(error) if error.is::<crate::store::topology::StorageBusy>() => {}
                    other => panic!("accepted Q1 lookup failed during recovery: {other:?}"),
                }
                // Let the daemon's nonblocking EX acquire the use lock; a
                // tight SH SQLite poll can starve it under all-target load.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        let observed = timeout_result.unwrap_or_else(|_| {
            panic!(
                "same accepted Q1 ID did not retry after external SH release: last_row={last_seen:?}, pending_recovery={}",
                fixture.store.is_recreate_pending()
            )
        });
        assert!(
            observed.finished_at.is_some(),
            "same accepted Q1 ID did not retry after external SH release: row={}/{:?}, pending_recovery={}",
            observed.state,
            observed.error_code,
            fixture.store.is_recreate_pending()
        );
        assert_reclaimed_fifo(&fixture, old_incarnation, queue_inode);
    }

    #[tokio::test]
    async fn timer_tick_quiesces_protected_select_before_ex_and_fences_injected_tick() {
        use std::os::unix::fs::MetadataExt;
        let mut fixture = paused_recovery_fixture();
        let old_incarnation = fixture.old_owner.as_ref().unwrap().incarnation();
        let queue_path = fixture.store.request_db_path();
        let queue_inode = std::fs::metadata(&queue_path).unwrap().ino();
        let queue_bytes = std::fs::read(&queue_path).unwrap();
        let corrupt_index = std::fs::read(&fixture.index_path).unwrap();
        let (selected_tx, selected_rx) = tokio::sync::oneshot::channel();
        let (release_select_tx, release_select_rx) = std::sync::mpsc::channel();
        fixture.store.set_queue_select_hook(move || {
            let _ = selected_tx.send(());
            let _ = release_select_rx.recv();
        });
        let (exclusive_tx, exclusive_rx) = tokio::sync::oneshot::channel();
        let (release_ex_tx, release_ex_rx) = std::sync::mpsc::channel();
        fixture.store.set_exclusive_recovery_hook(move || {
            let _ = exclusive_tx.send(());
            let _ = release_ex_rx.recv();
        });
        fixture
            .state
            .pending_requests
            .lock()
            .unwrap()
            .push(fixture.q1.id.clone());
        fixture
            .state
            .retain_serving_session(fixture.old_owner.take().unwrap());
        tokio::time::timeout(std::time::Duration::from_secs(5), selected_rx)
            .await
            .expect("20-ms timer did not reach protected SELECT")
            .unwrap();
        // This is the genuine daemon timer spawn_blocking worker, paused while
        // its short-lived request connection and shared use guard are both open.
        assert!(fixture.state.native_stream.try_lock().is_err());
        assert_eq!(std::fs::read(&queue_path).unwrap(), queue_bytes);
        assert_eq!(std::fs::read(&fixture.index_path).unwrap(), corrupt_index);
        release_select_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), exclusive_rx)
            .await
            .expect("timer recovery did not acquire fresh EX")
            .unwrap();
        assert!(
            fixture.state.retained_serving_session().is_err(),
            "old Arc must be dropped before EX"
        );
        assert!(
            fixture.state.native_stream.try_lock().is_err(),
            "EX stays inside native stream"
        );
        assert_eq!(std::fs::read(&queue_path).unwrap(), queue_bytes);
        assert_eq!(std::fs::read(&fixture.index_path).unwrap(), corrupt_index);
        let (before_tx, before_rx) = tokio::sync::oneshot::channel();
        let (after_tx, mut after_rx) = tokio::sync::oneshot::channel();
        fixture.state.test_queue_before_stream.set(move || {
            let _ = before_tx.send(());
        });
        fixture.state.test_queue_after_stream.set(move || {
            let _ = after_tx.send(());
        });
        // The production 20-ms timer awaits its first worker; this SECOND call
        // is deliberately injected on the SAME daemon to test the stream gate.
        let state = fixture.state.clone();
        let injected = tokio::task::spawn_blocking(move || state.queue_tick());
        tokio::time::timeout(std::time::Duration::from_secs(5), before_rx)
            .await
            .expect("injected same-daemon tick did not reach stream")
            .unwrap();
        assert!(fixture.state.native_stream.try_lock().is_err());
        assert!(
            matches!(
                after_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "injected tick must not open a queue handle while EX is held"
        );
        assert_eq!(std::fs::read(&queue_path).unwrap(), queue_bytes);
        assert_eq!(std::fs::read(&fixture.index_path).unwrap(), corrupt_index);
        release_ex_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), after_rx)
            .await
            .expect("injected tick never passed stream after EX")
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), injected)
            .await
            .expect("injected same-daemon tick did not finish")
            .unwrap()
            .unwrap();
        let mut last_seen = None;
        let timeout_result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match fixture.store.request_by_id(&fixture.q1.id) {
                    Ok(Some(row)) if row.finished_at.is_some() => break row,
                    Ok(Some(row)) => last_seen = Some((row.state, row.error_code)),
                    Err(error) if error.is::<crate::store::topology::StorageBusy>() => {}
                    other => panic!("accepted Q1 lookup failed during EX recovery: {other:?}"),
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        let observed = timeout_result.unwrap_or_else(|_| {
            panic!(
                "same queued ID never finished after EX: last_row={last_seen:?}, pending_recovery={}",
                fixture.store.is_recreate_pending()
            )
        });
        assert!(
            observed.finished_at.is_some(),
            "same queued ID never finished after EX: row={}/{:?}, pending_recovery={}",
            observed.state,
            observed.error_code,
            fixture.store.is_recreate_pending()
        );
        assert_reclaimed_fifo(&fixture, old_incarnation, queue_inode);
    }

    #[tokio::test]
    #[cfg(feature = "test-support")]
    async fn concurrent_posts_queue_two_requests_across_exceptional_recovery() {
        use tower::ServiceExt;
        let (_tmp, store, state, _roots, _identity) = fixture();
        // Keep this fixture's daemon timer and maintenance lane inert until
        // both POSTs are durably admitted. Drive one FIFO head per explicit tick.
        assert!(!state.queue_tick_started.swap(true, Ordering::AcqRel));
        assert!(!state.maintenance_tick_started.swap(true, Ordering::AcqRel));
        let q0 = store.current_request().unwrap().unwrap();
        assert_eq!(q0.state, "queued");
        let expected_options =
            serde_json::to_string(&crate::indexer::ReconcileOptions::from(&state.options)).unwrap();
        assert_eq!(q0.options_json, expected_options);
        let app = router(state.clone());
        let mut ids = Vec::new();
        for _ in 0..2 {
            let request = axum::http::Request::builder()
                .method("POST")
                .uri("/api/index")
                .header("host", "127.0.0.1:7331")
                .header("origin", "http://127.0.0.1:7331")
                .header(
                    "authorization",
                    "Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let ack: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            assert_eq!(ack["state"], "queued");
            assert!(ack["startedAt"].is_null());
            ids.push(ack["id"].as_str().unwrap().to_owned());
        }
        assert_ne!(ids[0], ids[1]);
        assert!(
            state.jobs.lock().unwrap().jobs.is_empty(),
            "legacy job map must not own queued work"
        );
        async fn tick_one(state: Arc<DaemonState>, phase: &'static str) {
            let work = tokio::task::spawn_blocking(move || state.queue_tick());
            let result = tokio::time::timeout(std::time::Duration::from_secs(10), work)
                .await
                .unwrap_or_else(|_| panic!("{phase}: queue_tick did not finish within 10 seconds"))
                .unwrap_or_else(|error| panic!("{phase}: queue_tick worker: {error}"));
            result.unwrap_or_else(|error| panic!("{phase}: queue_tick failed: {error:#}"));
        }
        let first_queued = store.request_by_id(&ids[0]).unwrap().unwrap();
        let second_queued = store.request_by_id(&ids[1]).unwrap().unwrap();
        assert!(q0.seq < first_queued.seq && first_queued.seq < second_queued.seq);
        assert_eq!(first_queued.state, "queued");
        assert_eq!(second_queued.state, "queued");
        assert!(first_queued.started_at.is_none() && second_queued.started_at.is_none());
        for row in [&first_queued, &second_queued] {
            assert_eq!(row.options_json, expected_options);
            assert!(row.revision.is_none() && row.finished_at.is_none());
        }
        tick_one(state.clone(), "H+Q0").await;
        let q0_done = store.request_by_id(&q0.id).unwrap().unwrap();
        let first_queued_after_h = store.request_by_id(&ids[0]).unwrap().unwrap();
        let second_queued_after_h = store.request_by_id(&ids[1]).unwrap().unwrap();
        assert_eq!(
            (q0_done.id.as_str(), q0_done.seq, q0_done.state.as_str()),
            (q0.id.as_str(), q0.seq, "done")
        );
        let q0_pin = q0_done.revision.unwrap();
        assert_eq!(
            q0_pin.index_revision, 2,
            "H r1 must precede preexisting Q0 r2"
        );
        assert_eq!(
            store.status().unwrap().revision,
            q0_pin,
            "no partial publication after H+Q0"
        );
        for (before, after) in [
            (&first_queued, &first_queued_after_h),
            (&second_queued, &second_queued_after_h),
        ] {
            assert_eq!(
                (
                    after.id.as_str(),
                    after.seq,
                    after.options_json.as_str(),
                    after.state.as_str()
                ),
                (
                    before.id.as_str(),
                    before.seq,
                    before.options_json.as_str(),
                    "queued"
                )
            );
            assert!(after.revision.is_none() && after.finished_at.is_none());
        }
        tick_one(state.clone(), "Q1").await;
        let first = store.request_by_id(&ids[0]).unwrap().unwrap();
        let second_still_queued = store.request_by_id(&ids[1]).unwrap().unwrap();
        assert_eq!(
            (
                first.id.as_str(),
                first.seq,
                first.options_json.as_str(),
                first.state.as_str()
            ),
            (
                first_queued.id.as_str(),
                first_queued.seq,
                expected_options.as_str(),
                "done"
            )
        );
        let first_pin = first.revision.unwrap();
        assert_eq!(first_pin.index_generation, q0_pin.index_generation);
        assert_eq!(first_pin.index_revision, q0_pin.index_revision + 1);
        assert_eq!(
            store.status().unwrap().revision,
            first_pin,
            "no partial Q2 publication"
        );
        assert_eq!(
            (
                second_still_queued.id.as_str(),
                second_still_queued.seq,
                second_still_queued.options_json.as_str(),
                second_still_queued.state.as_str()
            ),
            (
                second_queued.id.as_str(),
                second_queued.seq,
                expected_options.as_str(),
                "queued"
            )
        );
        assert!(
            second_still_queued.revision.is_none() && second_still_queued.finished_at.is_none()
        );
        tick_one(state.clone(), "Q2").await;
        let second = store.request_by_id(&ids[1]).unwrap().unwrap();
        assert_eq!(
            (
                second.id.as_str(),
                second.seq,
                second.options_json.as_str(),
                second.state.as_str()
            ),
            (
                second_queued.id.as_str(),
                second_queued.seq,
                expected_options.as_str(),
                "done"
            )
        );
        let second_pin = second.revision.unwrap();
        assert_eq!(second_pin.index_generation, first_pin.index_generation);
        assert_eq!(second_pin.index_revision, first_pin.index_revision + 1);
        assert_eq!(store.status().unwrap().revision, second_pin);
        assert!(store.earliest_unfinished_request().unwrap().is_none());
        let newest = store.current_request().unwrap().unwrap();
        assert_eq!(
            (newest.id.as_str(), newest.seq),
            (second.id.as_str(), second.seq)
        );
        let owner = state.retained_serving_session().unwrap();
        store.verify_writer_session(&owner).unwrap();
        store
            .verify_writer_session(&state.retained_serving_session().unwrap())
            .unwrap();
        assert!(state.jobs.lock().unwrap().jobs.is_empty());
    }

    #[tokio::test]
    async fn multiple_posts_commit_fifo_with_local_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(&tmp.path().join("state"), &root).unwrap();
        let state = new(
            store.clone(),
            IndexOptions::new(root),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        let (first_code, first) = start_index(State(state.clone()), Bytes::new())
            .await
            .unwrap();
        let (second_code, second) = start_index(State(state.clone()), Bytes::new())
            .await
            .unwrap();
        assert_eq!(
            (first_code, second_code),
            (StatusCode::ACCEPTED, StatusCode::ACCEPTED)
        );
        assert_eq!(
            (first.0.state.as_str(), second.0.state.as_str()),
            ("queued", "queued")
        );
        let a = store.request_by_id(&first.0.id).unwrap().unwrap();
        let b = store.request_by_id(&second.0.id).unwrap().unwrap();
        assert!(a.seq < b.seq);
        assert_eq!(store.current_request().unwrap().unwrap().id, b.id);
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if store
                    .request_by_id(&second.0.id)
                    .unwrap()
                    .unwrap()
                    .finished_at
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(store.request_by_id(&a.id).unwrap().unwrap().state, "done");
        assert_eq!(store.request_by_id(&b.id).unwrap().unwrap().state, "done");
    }

    #[tokio::test]
    async fn writer_session_is_valid_across_worker_result_and_atomic_install() {
        let (_tmp, store, state, _roots, _identity) = fixture();
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let options = state.options.clone();
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut jobs = state.jobs.lock().unwrap();
            jobs.current = Some(id.clone());
            jobs.cancel = cancel.clone();
            jobs.jobs.insert(
                id.clone(),
                IndexJob {
                    id: id.clone(),
                    state: "running".into(),
                    progress: IndexProgress::default(),
                    revision: None,
                    error: None,
                    submitted_at: now(),
                    started_at: Some(now()),
                    finished_at: None,
                },
            );
        }
        let result =
            crate::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {})
                .unwrap();
        let pin = result.0;
        let before = &state;
        finish_exceptional_index_job(&state, &id, Ok(result), &cancel, |session| {
            assert!(session.verify().is_ok());
            assert!(before.retained_serving_session().is_err());
            store.verify_writer_session(session).unwrap();
            assert_eq!(store.status().unwrap().revision, pin);
        });
        assert_eq!(state.jobs.lock().unwrap().jobs[&id].state, "done");
        assert_eq!(state.jobs.lock().unwrap().jobs[&id].revision, Some(pin));
        assert!(state.retained_serving_session().unwrap().verify().is_ok());
        store
            .verify_writer_session(&state.retained_serving_session().unwrap())
            .unwrap();
    }

    #[tokio::test]
    async fn cancellation_after_durable_recovery_reports_completed_with_new_owner() {
        let (_tmp, store, state, _roots, _identity) = fixture();
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut jobs = state.jobs.lock().unwrap();
            jobs.current = Some(id.clone());
            jobs.cancel = cancel.clone();
            jobs.jobs.insert(
                id.clone(),
                IndexJob {
                    id: id.clone(),
                    state: "running".into(),
                    progress: IndexProgress::default(),
                    revision: None,
                    error: None,
                    submitted_at: now(),
                    started_at: Some(now()),
                    finished_at: None,
                },
            );
        }
        let result =
            crate::index_coordinator::reconcile_workspace(&store, &state.options, &cancel, |_| {})
                .unwrap();
        let pin = result.0;
        cancel.store(true, Ordering::Release);
        finish_exceptional_index_job(&state, &id, Ok(result), &cancel, |_| {});
        let job = state.jobs.lock().unwrap().jobs[&id].clone();
        assert_eq!(job.state, "done");
        assert_eq!(job.revision, Some(pin));
        assert!(state.retained_serving_session().unwrap().verify().is_ok());
        assert_eq!(store.status().unwrap().revision, pin);
        store
            .verify_writer_session(&state.retained_serving_session().unwrap())
            .unwrap();
    }

    #[tokio::test]
    async fn failed_worker_never_installs_owner_or_reports_completed() {
        let (_tmp, store, state, roots, identity) = fixture();
        let before = std::fs::read(roots.index_db(&identity)).unwrap();
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut jobs = state.jobs.lock().unwrap();
            jobs.current = Some(id.clone());
            jobs.jobs.insert(
                id.clone(),
                IndexJob {
                    id: id.clone(),
                    state: "running".into(),
                    progress: IndexProgress::default(),
                    revision: None,
                    error: None,
                    submitted_at: now(),
                    started_at: Some(now()),
                    finished_at: None,
                },
            );
        }
        // The Store's own fault tests cover actual post-rename failures. Here
        // exercise the HTTP handoff for their error result: no false success.
        finish_exceptional_index_job(
            &state,
            &id,
            Err(anyhow::anyhow!("after rename fsync failed")),
            &cancel,
            |_| panic!("failed worker cannot cross install barrier"),
        );
        assert_eq!(state.jobs.lock().unwrap().jobs[&id].state, "failed");
        assert!(state.jobs.lock().unwrap().jobs[&id].revision.is_none());
        assert!(state.retained_serving_session().is_err());
        assert!(store.evidence_response().is_err());
        assert_eq!(std::fs::read(roots.index_db(&identity)).unwrap(), before);
    }
}

#[cfg(all(test, feature = "test-support"))]
mod normal_post_capture_cancellation_tests {
    use super::*;
    #[cfg(feature = "test-support")]
    use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
    use rusqlite::{OpenFlags, types::Value as SqlValue};
    #[cfg(feature = "test-support")]
    use std::{fs, time::Duration};
    use tower::ServiceExt;

    #[derive(Debug, PartialEq)]
    struct PairSnapshot {
        pin: IndexPin,
        rows: BTreeMap<String, Vec<String>>,
    }

    // Read all derived graph, native, class and source rows in one SQLite snapshot.
    fn pair_snapshot(index: &std::path::Path) -> PairSnapshot {
        let db =
            rusqlite::Connection::open_with_flags(index, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.execute_batch("BEGIN DEFERRED").unwrap();
        let (generation, revision): (String, i64) = db
            .query_row(
                "SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let pin = IndexPin {
            index_generation: uuid::Uuid::parse_str(&generation).unwrap(),
            index_revision: u64::try_from(revision).unwrap(),
        };
        let tables: Vec<String> = db
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name!='index_metadata' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            tables.len(),
            35,
            "compare exact v9 derived evidence inventory"
        );
        let expected = vec![
            "class_projections",
            "class_relations",
            "classes",
            "document_versions",
            "graph_calls",
            "graph_nodes",
            "graph_projections",
            "graph_regions",
            "native_binding_epoch",
            "native_producer_inputs",
            "native_producer_languages",
            "native_producers",
            "native_release_candidate_classes",
            "native_release_candidate_graphs",
            "native_release_candidate_versions",
            "native_revision_release_debt",
            "native_revision_supersessions",
            "native_revisions",
            "native_source_set_dependencies",
            "native_source_set_languages",
            "native_source_sets",
            "native_version_ancestor_signature_types",
            "native_version_call_regions",
            "native_version_calls",
            "native_version_control_regions",
            "native_version_coverage_roles",
            "native_version_declaration_ancestors",
            "native_version_declarations",
            "native_version_header_items",
            "native_version_headers",
            "native_version_own_signature_types",
            "native_version_parameters",
            "revision_capture_inputs",
            "revision_documents",
            "revision_producer_bindings",
        ];
        assert_eq!(
            tables.iter().map(String::as_str).collect::<Vec<_>>(),
            expected,
            "exact v9 table names: complete paired producer bindings and durable release debt"
        );
        for required in [
            "document_versions",
            "revision_documents",
            "graph_nodes",
            "native_version_declarations",
            "class_projections",
        ] {
            assert!(
                tables.iter().any(|table| table == required),
                "missing {required}"
            );
        }
        let mut rows = BTreeMap::new();
        for table in tables {
            assert!(
                table
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            );
            let mut stmt = db.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let columns = stmt.column_count();
            let mut values = stmt
                .query_map([], |row| {
                    let cells = (0..columns)
                        .map(|column| row.get::<_, SqlValue>(column))
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(format!("{cells:?}"))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            values.sort();
            rows.insert(table, values);
        }
        db.execute_batch("ROLLBACK").unwrap();
        PairSnapshot { pin, rows }
    }

    async fn api(
        app: &Router,
        token: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1:7331")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(
                body.map(|value| value.to_string()).unwrap_or_default(),
            ))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    #[cfg(feature = "test-support")]
    async fn accepted_job_cannot_be_cancelled_and_full_pair_changes_only_after_leader_drains() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("one.js"),
            "function seed() { old_step(); } function old_step() {}\n",
        )
        .unwrap();
        let state_root = tmp.path().join("state");
        let roots =
            TopologyRoots::isolated_for_tests(state_root.join("cache"), state_root.join("data"));
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let index = roots.index_db(&identity);
        let store = Store::open_for_tests(&state_root, &root).unwrap();
        let options = IndexOptions::new(root.clone());
        let (_, owner) = crate::index_coordinator::reconcile_workspace(
            &store,
            &options,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let before = pair_snapshot(&index);
        fs::write(
            root.join("one.js"),
            "function seed() { fresh(); } function fresh() {}\n",
        )
        .unwrap();
        fs::write(root.join("two.js"), "function extra() {}\n").unwrap();
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let state = new(
            store.clone(),
            options,
            token.into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        let app = router(state.clone());
        let (code, accepted) = api(&app, token, "POST", "/api/index", Some(json!({}))).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{accepted}");
        assert_eq!(accepted["state"], "queued");
        let id = accepted["id"].as_str().unwrap();
        let (code, rejected) =
            api(&app, token, "POST", &format!("/api/jobs/{id}/cancel"), None).await;
        assert_eq!(code, StatusCode::CONFLICT, "{rejected}");
        assert_eq!(rejected["error"]["code"], "request_not_cancellable");
        assert_eq!(pair_snapshot(&index), before);
        assert_eq!(store.request_by_id(id).unwrap().unwrap().state, "queued");
        drop(owner);
        let completed = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let (code, row) = api(&app, token, "GET", &format!("/api/jobs/{id}"), None).await;
                assert_eq!(code, StatusCode::OK, "{row}");
                if !row["finishedAt"].is_null() {
                    break row;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(completed["state"], "done", "{completed}");
        let after = pair_snapshot(&index);
        assert!(after.pin.index_revision > before.pin.index_revision);
        assert_ne!(
            after.rows["revision_documents"],
            before.rows["revision_documents"]
        );
        assert_ne!(
            after.rows["document_versions"],
            before.rows["document_versions"]
        );
        assert!(
            after.rows["document_versions"]
                .iter()
                .any(|row| row.contains("two.js"))
        );
        assert_eq!(
            after.pin,
            serde_json::from_value(completed["revision"].clone()).unwrap()
        );
    }
}

#[cfg(all(test, feature = "test-support"))]
mod dependency_lifecycle_tests {
    use super::*;
    fn catalog(id: &str, revision: IndexPin) -> Catalog {
        Catalog {
            id: id.into(),
            workspace_revision: revision,
            packages: vec![],
            symbols: vec![],
            warnings: vec![],
            sources: Default::default(),
        }
    }
    #[tokio::test]
    #[cfg(feature = "test-support")]
    async fn dependency_status_whole_response_fence() {
        async fn call(app: &Router, path: &str) -> (StatusCode, Value) {
            use tower::ServiceExt;
            let request = axum::http::Request::builder()
                .uri(path)
                .header("host", "127.0.0.1:7331")
                .header(
                    "authorization",
                    "Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let body =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            (status, body)
        }
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let options = IndexOptions::new(workspace.clone());
        let (pin, session) =
            crate::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {})
                .unwrap();
        let state = new(
            store.clone(),
            options.clone(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap();
        state.retain_serving_session(session);
        {
            let mut deps = state.dependencies.lock().unwrap();
            deps.state = "ready";
            deps.catalog = Some(Arc::new(catalog("matching", pin)));
        }
        let app = router(state.clone());
        let (valid_status, valid) = call(&app, "/api/dependencies").await;
        assert_eq!(valid_status, StatusCode::OK);
        assert_eq!(valid["workspaceRevision"], json!(pin));
        assert_eq!(valid["catalogId"], "matching");
        let mismatched = IndexPin {
            index_revision: pin.index_revision + 1,
            ..pin
        };
        state.dependencies.lock().unwrap().catalog = Some(Arc::new(catalog("old", mismatched)));
        let (failed_status, failed) = call(&app, "/api/dependencies").await;
        assert_eq!(failed_status, StatusCode::OK);
        assert_eq!(failed["state"], "failed");
        assert_eq!(failed["catalogId"], Value::Null);
        assert_eq!(failed["packages"], json!([]));
        assert_eq!(failed["symbolCount"], 0);
        assert_eq!(
            failed["warnings"],
            json!(["Workspace changed; refresh the dependency catalog"])
        );
        let (browse_status, browse) = call(&app, "/api/files").await;
        assert_eq!(browse_status, StatusCode::OK);
        assert_eq!(browse["revision"], json!(pin));
        let publishing_store = store.clone();
        let publishing_options = options.clone();
        let publishing_session = state.retained_serving_session().unwrap();
        *state.outer_fence_hook.lock().unwrap() = Some(Arc::new(move || {
            let cancel = Arc::new(AtomicBool::new(false));
            let (graph, native, capture) = crate::indexer::index_workspace_bundle(
                &publishing_options,
                publishing_store.root_id(),
                &cancel,
                |_| {},
            )
            .unwrap();
            publishing_store
                .publish_native(
                    &graph,
                    &capture,
                    &native,
                    publishing_session.writer_guard().unwrap(),
                    pin,
                    &cancel,
                )
                .unwrap();
        }));
        let (published_status, old_response) = call(&app, "/api/files").await;
        assert_eq!(published_status, StatusCode::OK);
        assert_eq!(old_response["revision"], json!(pin));
        *state.outer_fence_hook.lock().unwrap() = None;
        let current = store.status().unwrap().revision;
        assert_eq!(current.index_revision, pin.index_revision + 1);
    }
    #[test]
    fn refresh_bursts_admit_one_worker_and_keep_only_latest_generation() {
        let mut index = DependencyIndex {
            generation: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            stopped: false,
            worker_running: false,
            state: "loading",
            catalog: None,
            warnings: vec![],
        };
        assert!(index.request_build());
        let first_cancel = index.cancel.clone();
        for _ in 0..1000 {
            assert!(!index.request_build());
        }
        assert!(first_cancel.load(Ordering::Acquire));
        assert!(!index.cancel.load(Ordering::Acquire));
        assert_eq!(index.generation, 1001);
        // A finished driver relinquishes the slot; the next request can restart it.
        index.worker_running = false;
        assert!(index.request_build());
        index.stopped = true;
        assert!(!index.request_build());
        assert_eq!(index.generation, 1002);
    }
    #[test]
    fn stale_generation_cancel_and_revision_never_publish() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
        crate::store::topology::assert_topology_fixture(&store, &temp.path().join("state"));
        let cancel = Arc::new(AtomicBool::new(false));
        let (graph, native, capture) = crate::indexer::index_workspace_bundle(
            &IndexOptions::new(workspace.clone()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let session = store.writer_session().unwrap();
        let pin0 = store
            .publish_native(
                &graph,
                &capture,
                &native,
                session.writer_guard().unwrap(),
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        let state = new_with_dependency_options(
            store.clone(),
            IndexOptions::new(workspace.clone()),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            "127.0.0.1:7331".parse().unwrap(),
            None,
            None,
            workspace.clone(),
            vec![],
            Some(CatalogOptions::default()),
        )
        .unwrap();
        state.retain_serving_session(session.clone());
        let active = AtomicBool::new(false);
        state.dependencies.lock().unwrap().generation = 2;
        state.publish_dependency_index(2, &active, Ok(catalog("new", pin0)));
        state.publish_dependency_index(1, &active, Ok(catalog("old", pin0)));
        state.publish_dependency_index(1, &active, Err(anyhow::anyhow!("late failure")));
        assert_eq!(state.catalog_snapshot(pin0).unwrap().id, "new");
        state.publish_dependency_index(2, &AtomicBool::new(true), Ok(catalog("cancelled", pin0)));
        assert_eq!(state.catalog_snapshot(pin0).unwrap().id, "new");
        let (updated, native, captured) = crate::indexer::index_workspace_bundle(
            &IndexOptions::new(workspace),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        store
            .publish_native(
                &updated,
                &captured,
                &native,
                session.writer_guard().unwrap(),
                pin0,
                &cancel,
            )
            .unwrap();
        let pin1 = store.status().unwrap().revision;
        state.publish_dependency_index(2, &active, Ok(catalog("stale-revision", pin0)));
        assert!(state.catalog_snapshot(pin0).is_none());
        assert_eq!(state.dependencies.lock().unwrap().state, "failed");
        state.cancel_active();
        let generation = state.dependencies.lock().unwrap().generation;
        state.publish_dependency_index(generation, &active, Ok(catalog("after-shutdown", pin1)));
        assert!(state.catalog_snapshot(pin1).is_none());
    }
}

#[cfg(test)]
mod maintenance_telemetry_tests {
    use super::*;
    #[test]
    fn maintenance_failures_are_sanitized_and_rate_limited_without_sleep() {
        let start = Instant::now();
        let mut limiter = MaintenanceErrorLimiter::default();
        assert!(limiter.permit(start));
        assert!(!limiter.permit(start + Duration::from_millis(20)));
        assert!(!limiter.permit(start + Duration::from_secs(59)));
        assert!(limiter.permit(start + Duration::from_secs(60)));
        let sensitive = "workspace/private/path-and-source";
        let opaque = anyhow::anyhow!("{sensitive}");
        let (category, code) = maintenance_error_class(&opaque);
        assert_eq!((category, code), ("other", 0));
        let notice = maintenance_error_notice(category, code);
        assert_eq!(notice, "maintenance tick failed: category=other code=0\n");
        assert!(!notice.contains(sensitive));

        let db = rusqlite::Connection::open_in_memory().unwrap();
        let sqlite = db
            .execute("INSERT INTO private_missing_table(id) VALUES (1)", [])
            .unwrap_err();
        let wrapped = anyhow::Error::new(sqlite).context(sensitive);
        let (category, code) = maintenance_error_class(&wrapped);
        assert_eq!(category, "sqlite");
        assert_ne!(code, 0);
        let notice = maintenance_error_notice(category, code);
        assert!(!notice.contains(sensitive));
        assert!(!notice.contains("private_missing_table"));
    }

    #[test]
    fn deferred_preemption_busy_and_success_keep_max_age_without_sleep() {
        let now = Instant::now();
        let mut stats = MaintenanceTelemetry {
            deferred_since: Some(now - Duration::from_secs(2)),
            ..Default::default()
        };
        stats.busy_attempts = 1; // Store's exact counter is sampled by the scheduler.
        let (age, max) = stats.deferred(now);
        assert!(age >= 2_000 && max >= 2_000);
        stats.observe_due_age(Some(899));
        assert_eq!(stats.max_deferred_age_s, 0);
        stats.observe_due_age(Some(905));
        assert_eq!(stats.max_deferred_age_s, 5);
        stats.observe_due_age(None);
        assert_eq!(stats.max_deferred_age_s, 5);
        assert_eq!((stats.preemptions, stats.busy_attempts), (1, 1));
        let (age, max) = stats.progressed(now);
        assert!(age >= 2_000 && max >= 2_000);
        assert_eq!((stats.successful_units, stats.preemptions), (1, 1));
        assert!(stats.deferred_since.is_none());
    }
}

/// The socket-only daemon has no HTTP state. This router is constructed only
/// after a validated explicit serve registration binds a loopback listener.
type SelectedResponseHook = Arc<dyn Fn(&'static str) + Send + Sync>;

#[derive(Clone)]
pub struct ProvisionedBrowser {
    registry: Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    token: String,
    hosts: Vec<String>,
    origins: Vec<String>,
    response_hook: Arc<Mutex<Option<SelectedResponseHook>>>,
}

impl ProvisionedBrowser {
    pub fn new(
        registry: Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
        token: String,
        address: SocketAddr,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            address.ip().is_loopback() && address.port() != 0,
            "bound loopback address required"
        );
        anyhow::ensure!(valid_token(&token), "invalid bearer token");
        let hosts = vec![address.to_string(), format!("localhost:{}", address.port())];
        let origins = hosts.iter().map(|host| format!("http://{host}")).collect();
        Ok(Self {
            registry,
            token,
            hosts,
            origins,
            response_hook: Arc::new(Mutex::new(None)),
        })
    }

    #[doc(hidden)]
    pub fn set_selected_response_hook_for_tests(&self, hook: SelectedResponseHook) {
        *self.response_hook.lock().unwrap() = Some(hook);
    }

    pub fn router(self) -> Router {
        let state = Arc::new(self);
        Router::new()
            .route(
                "/",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                        include_str!("../web/index.html"),
                    )
                }),
            )
            .route(
                "/app.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("../web/app.js"),
                    )
                }),
            )
            .route(
                "/shell.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("../web/shell.js"),
                    )
                }),
            )
            .route(
                "/style.css",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                        include_str!("../web/style.css"),
                    )
                }),
            )
            .route(
                "/classes.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("../web/classes.js"),
                    )
                }),
            )
            .route(
                "/classes.css",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                        include_str!("../web/classes.css"),
                    )
                }),
            )
            .route(
                "/navigation.css",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                        include_str!("../web/navigation.css"),
                    )
                }),
            )
            .route(
                "/navigation.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("../web/navigation.js"),
                    )
                }),
            )
            .route(
                "/sequence.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("../web/sequence.js"),
                    )
                }),
            )
            .route(
                "/fonts/jetbrains-mono-latin.woff2",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "font/woff2")],
                        &include_bytes!("../web/fonts/jetbrains-mono-latin.woff2")[..],
                    )
                }),
            )
            .route(
                "/fonts/space-grotesk-latin.woff2",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "font/woff2")],
                        &include_bytes!("../web/fonts/space-grotesk-latin.woff2")[..],
                    )
                }),
            )
            .route(
                "/fonts/JetBrainsMono-OFL.txt",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                        include_str!("../web/fonts/JetBrainsMono-OFL.txt"),
                    )
                }),
            )
            .route(
                "/fonts/SpaceGrotesk-OFL.txt",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                        include_str!("../web/fonts/SpaceGrotesk-OFL.txt"),
                    )
                }),
            )
            .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
            .route(
                "/healthz",
                get(|| async { Json(json!({"ok":true,"version":env!("CARGO_PKG_VERSION")})) }),
            )
            .route(
                "/api/checkouts",
                get(|State(state): State<Arc<Self>>| async move {
                    Json(json!({"checkouts":state.registry.lock().await.browser_checkouts()}))
                }),
            )
            .route(
                "/api/daemon/status",
                get(|State(state): State<Arc<Self>>| async move {
                    let registry = state.registry.lock().await;
                    Json(json!({"activeCheckouts":registry.active_count()}))
                }),
            )
            .route(
                "/api/checkouts/{root_key}/{*suffix}",
                get(provisioned_core)
                    .post(provisioned_core)
                    .put(provisioned_core)
                    .delete(provisioned_core),
            )
            .layer(middleware::from_fn_with_state(
                state.clone(),
                provision_guard,
            ))
            .with_state(state)
    }
}

async fn provision_guard(
    State(state): State<Arc<ProvisionedBrowser>>,
    req: Request,
    next: Next,
) -> Response {
    guard_common(&state.token, &state.hosts, &state.origins, req, next).await
}

/// The provisioned listener has one selection boundary for core checkout routes.
/// The direct single-checkout router is a test-support fixture only.
async fn provisioned_core(
    State(browser): State<Arc<ProvisionedBrowser>>,
    Path((root_key, suffix)): Path<(String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    use crate::daemon::registry::SelectionError;
    let identity = {
        let mut registry = browser.registry.lock().await;
        match registry.browser_identity(&root_key) {
            Ok(identity) => identity,
            Err(reason) => {
                return browser_selection_error(reason);
            }
        }
    };
    let root = identity.root.to_string_lossy().to_string();
    let runtime = {
        let mut registry = browser.registry.lock().await;
        match registry.browser_request_at(&identity, Instant::now()) {
            Ok(_) => registry.activate(&root_key).map_err(|_| {
                ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "store_unavailable",
                    "Storage is unavailable",
                )
            }),
            Err(SelectionError::CheckoutCapacity) => Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "checkout_capacity",
                "Checkout capacity reached",
            )),
            Err(error) => Err(ApiError::from(anyhow::anyhow!("{}", error.reason()))),
        }
    };
    let answer: Result<(Response, bool), ApiError> = match runtime {
        Err(error) => Err(error),
        Ok(runtime) => {
            let hook = browser.response_hook.lock().unwrap().clone();
            let result =
                provisioned_core_answer(&runtime, &method, &suffix, &uri, body, hook.clone()).await;
            if let Some(hook) = hook {
                hook("after_handler");
            }
            result
        }
    };
    let (mut response, catching_up) = match answer {
        Ok(pair) => pair,
        Err(error) => {
            let catching_up = browser
                .registry
                .lock()
                .await
                .runtime(&root_key)
                .is_none_or(|runtime| runtime.catching_up());
            (error.into_response(), catching_up)
        }
    };
    if identity.verify_readonly().is_err() {
        let outcome = response
            .headers()
            .get("X-Trellis-Mutation-Outcome")
            .and_then(|value| value.to_str().ok());
        response = match outcome {
            Some(outcome) => selected_mutation_error(
                ApiError(
                    StatusCode::CONFLICT,
                    "root_changed",
                    "Selected checkout changed",
                ),
                outcome,
            ),
            None => ApiError::from(anyhow::anyhow!("root_changed: selected checkout changed"))
                .into_response(),
        };
    }
    if let Ok(value) = root.parse() {
        response.headers_mut().insert("X-Trellis-Workspace", value);
    }
    response.headers_mut().insert(
        "X-Trellis-Catching-Up",
        if catching_up { "true" } else { "false" }.parse().unwrap(),
    );
    response
}

fn browser_selection_error(reason: crate::daemon::registry::SelectionError) -> Response {
    let status = match reason {
        crate::daemon::registry::SelectionError::NotCheckout
        | crate::daemon::registry::SelectionError::NotAbsolute => StatusCode::BAD_REQUEST,
        crate::daemon::registry::SelectionError::IdentityChanged => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(json!({"error":{"code":"workspace_selection_failed", "reason":reason.reason(), "message":"Checkout selection failed"}}))).into_response()
}

async fn selected_evidence<T: Send + 'static>(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    hook: Option<SelectedResponseHook>,
    work: impl FnOnce(&crate::daemon::registry::CheckoutEvidence) -> anyhow::Result<T> + Send + 'static,
) -> Result<(T, bool), ApiError> {
    selected_evidence_map(runtime, hook, work, ApiError::from).await
}

async fn selected_question<T: Send + 'static>(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    hook: Option<SelectedResponseHook>,
    work: impl FnOnce(&crate::daemon::registry::CheckoutEvidence) -> anyhow::Result<T> + Send + 'static,
) -> Result<(T, bool), ApiError> {
    selected_evidence_map(runtime, hook, work, question_error).await
}

async fn selected_evidence_map<T: Send + 'static>(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    hook: Option<SelectedResponseHook>,
    work: impl FnOnce(&crate::daemon::registry::CheckoutEvidence) -> anyhow::Result<T> + Send + 'static,
    map_error: fn(anyhow::Error) -> ApiError,
) -> Result<(T, bool), ApiError> {
    let runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        let (response, catching_up) = runtime.evidence_response()?;
        let result = work(&response)?;
        if let Some(hook) = hook {
            hook("before_read_finish");
        }
        Ok::<_, anyhow::Error>((response.finish(result)?, catching_up))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
    .map_err(map_error)
}

async fn selected_provider_read<T: Send + 'static>(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    hook: Option<SelectedResponseHook>,
    work: impl FnOnce(&crate::daemon::registry::CheckoutEvidence) -> Result<T, ApiError>
    + Send
    + 'static,
) -> Result<(T, bool), ApiError> {
    let runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        let (response, catching_up) = runtime.evidence_response().map_err(ApiError::from)?;
        let value = work(&response)?;
        if let Some(hook) = hook {
            hook("before_read_finish");
        }
        Ok((response.finish(value).map_err(ApiError::from)?, catching_up))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
}

fn selected_catalog(
    state: &DaemonState,
    revision: IndexPin,
    id: &str,
) -> Result<Arc<Catalog>, ApiError> {
    state
        .catalog_snapshot(revision)
        .filter(|catalog| catalog.id == id)
        .ok_or_else(dependency_stale)
}

async fn selected_provider_preflight(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    hook: Option<SelectedResponseHook>,
    packet_id: Option<String>,
) -> Result<(Option<Arc<QuestionPacket>>, IndexPin, bool), ApiError> {
    let runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        runtime.require_mutation_ready().map_err(question_error)?;
        let (response, catching_up) = runtime.evidence_response().map_err(question_error)?;
        response.require_mutation_ready().map_err(question_error)?;
        let revision = response.status().map_err(question_error)?.revision;
        let packet = if let Some(id) = packet_id {
            let state = runtime.browser_scheduler().map_err(question_error)?;
            let packet = state
                .packets
                .lock()
                .unwrap()
                .packets
                .iter()
                .find(|(packet, _)| packet.packet_id == id)
                .map(|(packet, _)| packet.clone())
                .ok_or_else(missing)?;
            if packet.revision != revision {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "revision_conflict",
                    "The index revision changed",
                ));
            }
            response
                .validate_selected_view(&packet.context, &packet.source_files)
                .map_err(question_error)?;
            Some(packet)
        } else {
            None
        };
        response.finish(()).map_err(question_error)?;
        if let Some(hook) = hook {
            hook("before_mutation");
        }
        runtime.require_mutation_ready().map_err(question_error)?;
        response.finish(()).map_err(question_error)?;
        Ok((packet, revision, catching_up))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
}

async fn selected_provider_postflight(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    revision: IndexPin,
) -> Result<(), ApiError> {
    let runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        runtime.require_mutation_ready()?;
        let (response, _) = runtime.evidence_response()?;
        response.require_mutation_ready()?;
        anyhow::ensure!(
            response.status()?.revision == revision,
            "revision conflict: provider basis changed"
        );
        response.finish(())?;
        runtime.require_mutation_ready()
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
    .map_err(ApiError::from)
}

fn selected_mutation_error(error: ApiError, outcome: &str) -> Response {
    let mut response = (
        error.0,
        Json(json!({
            "error": {"code": error.1, "message": error.2},
            "mutationOutcome": outcome
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert("X-Trellis-Mutation-Outcome", outcome.parse().unwrap());
    response
}

/// Only a strict current head grants sidecar write eligibility. Once the
/// sidecar is called, later failures carry an explicit committed/unknown
/// outcome; they cannot claim that the write was rolled back.
async fn selected_mutation<T: IntoResponse + Send + 'static>(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    hook: Option<SelectedResponseHook>,
    preflight: impl FnOnce(&crate::daemon::registry::CheckoutEvidence) -> anyhow::Result<()>
    + Send
    + 'static,
    work: impl FnOnce(&crate::daemon::registry::CheckoutEvidence) -> anyhow::Result<T> + Send + 'static,
) -> Result<(Response, bool), ApiError> {
    let runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        runtime.require_mutation_ready()?;
        let (response, catching_up) = runtime.evidence_response()?;
        response.require_mutation_ready()?;
        preflight(&response)?;
        response.finish(())?;
        if let Some(hook) = &hook {
            hook("before_mutation");
        }
        runtime.require_mutation_ready()?;
        response.finish(())?;
        let result = work(&response);
        if let Some(hook) = hook {
            hook("before_read_finish");
        }
        let mut answer = match result {
            Ok(value) => match response.finish(value) {
                Ok(value) => value.into_response(),
                Err(error) => selected_mutation_error(ApiError::from(error), "committed"),
            },
            Err(_) => selected_mutation_error(
                ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "outcome_unknown",
                    "Write outcome is unknown; inspect the saved record before retrying",
                ),
                "unknown",
            ),
        };
        if !answer.headers().contains_key("X-Trellis-Mutation-Outcome") {
            answer
                .headers_mut()
                .insert("X-Trellis-Mutation-Outcome", "committed".parse().unwrap());
        }
        Ok::<_, anyhow::Error>((answer, catching_up))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
    .map_err(ApiError::from)
}

async fn provisioned_core_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    method: &axum::http::Method,
    suffix: &str,
    uri: &axum::http::Uri,
    body: Bytes,
    hook: Option<SelectedResponseHook>,
) -> Result<(Response, bool), ApiError> {
    use axum::http::Method;
    match (method, suffix) {
        (&Method::POST, "dependencies/refresh") => {
            let request: Value = serde_json::from_slice(&body).map_err(|_| invalid())?;
            if !request.as_object().is_some_and(|object| object.is_empty()) {
                return Err(invalid());
            }
            let state = runtime.browser_scheduler()?;
            let (_, revision, catching_up) =
                selected_provider_preflight(runtime, hook, None).await?;
            if state.dependency_options.is_none() {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "dependencies_disabled",
                    "Dependency catalog is disabled",
                ));
            }
            if state.dependencies.lock().unwrap().stopped {
                return Err(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "shutting_down",
                    "Daemon is shutting down",
                ));
            }
            // Check once more immediately before scheduling work. Once scheduled,
            // a later root or head change cannot undo the requested refresh.
            selected_provider_postflight(runtime, revision).await?;
            runtime.require_mutation_ready()?;
            state.start_dependency_index();
            let mut response = match selected_provider_postflight(runtime, revision).await {
                Ok(()) => (StatusCode::ACCEPTED, Json(json!({"state":"loading"}))).into_response(),
                Err(error) => selected_mutation_error(error, "committed"),
            };
            response
                .headers_mut()
                .insert("X-Trellis-Mutation-Outcome", "committed".parse().unwrap());
            Ok((response, catching_up))
        }
        _ if method == Method::POST
            && suffix.starts_with("questions/")
            && (suffix.ends_with("/jev-run") || suffix.ends_with("/acp-answer")) =>
        {
            let tail = suffix.strip_prefix("questions/").unwrap();
            let (id, action) = tail.split_once('/').ok_or_else(missing)?;
            if id.is_empty() || action.contains('/') {
                return Err(missing());
            }
            let jev = action == "jev-run";
            if jev {
                if !body.is_empty() && body.as_ref() != b"{}" {
                    return Err(invalid());
                }
            } else {
                let request: Value = serde_json::from_slice(&body).map_err(|_| invalid())?;
                if !request.as_object().is_some_and(|object| object.is_empty()) {
                    return Err(invalid());
                }
            }
            let state = runtime.browser_scheduler()?;
            let (packet, revision, catching_up) =
                selected_provider_preflight(runtime, hook, Some(id.to_owned())).await?;
            let packet = packet.unwrap();
            if jev {
                let provider = state.provider.clone().ok_or(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "jev_disabled",
                    "Live Jev is disabled",
                ))?;
                // No await before the final current-head check or the provider call.
                selected_provider_postflight(runtime, revision).await?;
                let result = provider.run(&packet).await;
                if let Err(error) = selected_provider_postflight(runtime, revision).await {
                    return Ok((selected_mutation_error(error, "unknown"), catching_up));
                }
                let response = match result {
                    Err(error) => live_jev_error(error).into_response(),
                    Ok(result) => {
                        let selection = result.selection.clone();
                        let mut view = match planning::assemble(&packet, &selection, "liveJev") {
                            Ok(view) => view,
                            Err(error) => {
                                return Ok((
                                    selected_mutation_error(question_error(error), "unknown"),
                                    catching_up,
                                ));
                            }
                        };
                        view.warnings.extend(result.warnings.clone());
                        Json(json!({"selection":result.selection,"view":view,
                            "attemptId":result.attempt_id,"latencyMs":result.latency_ms,
                            "estimatedUsd":result.estimated_usd,"usage":result.usage,
                            "warnings":result.warnings}))
                        .into_response()
                    }
                };
                let mut response = response;
                let outcome = if response.status().is_success() {
                    "committed"
                } else {
                    "unknown"
                };
                response
                    .headers_mut()
                    .insert("X-Trellis-Mutation-Outcome", outcome.parse().unwrap());
                Ok((response, catching_up))
            } else {
                let provider = state.acp.clone().ok_or(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "acp_disabled",
                    "Live ACP is disabled",
                ))?;
                selected_provider_postflight(runtime, revision).await?;
                let result = provider.run(&packet).await;
                if let Err(error) = selected_provider_postflight(runtime, revision).await {
                    return Ok((selected_mutation_error(error, "unknown"), catching_up));
                }
                let response = match result {
                    Err(error) => acp_error(error).into_response(),
                    Ok(result) => Json(
                        json!({"packetId":packet.packet_id,"revision":packet.revision,
                        "source":"liveAcp","attemptId":result.attempt_id,"answer":result.answer,
                        "latencyMs":result.latency_ms,"estimatedUsd":result.estimated_usd}),
                    )
                    .into_response(),
                };
                let mut response = response;
                let outcome = if response.status().is_success() {
                    "committed"
                } else {
                    "unknown"
                };
                response
                    .headers_mut()
                    .insert("X-Trellis-Mutation-Outcome", outcome.parse().unwrap());
                Ok((response, catching_up))
            }
        }
        (&Method::GET, "jev/status") | (&Method::GET, "acp/status") => {
            if uri.query().is_some() {
                return Err(invalid());
            }
            let state = runtime.browser_scheduler()?;
            let jev = suffix == "jev/status";
            let (value, catching_up) = selected_provider_read(runtime, hook, move |_| {
                if jev {
                    match &state.provider {
                        Some(provider) => Ok(json!({"enabled":true,"budget":provider.budget().map_err(ApiError::from)?})),
                        None => Ok(json!({"enabled":false,"budget":null})),
                    }
                } else {
                    match &state.acp {
                        Some(provider) => Ok(json!({"enabled":true,"status":provider.status().map_err(acp_error)?})),
                        None => Ok(json!({"enabled":false,"status":null})),
                    }
                }
            }).await?;
            Ok((Json(value).into_response(), catching_up))
        }
        (&Method::GET, "dependencies") => {
            if uri.query().is_some() {
                return Err(invalid());
            }
            let state = runtime.browser_scheduler()?;
            let (payload, catching_up) = selected_provider_read(runtime, hook, move |r| {
                let revision = r.status().map_err(ApiError::from)?.revision;
                let (index_state, catalog, mut warnings) = {
                    let index = state.dependencies.lock().unwrap();
                    (index.state, index.catalog.clone(), index.warnings.clone())
                };
                Ok(if let Some(catalog) = catalog {
                    if catalog.workspace_revision == revision {
                        json!({"state":index_state,"workspaceRevision":revision,
                            "catalogId":catalog.id,"packages":catalog.packages,
                            "symbolCount":catalog.symbols.len(),"warnings":catalog.warnings})
                    } else {
                        warnings.push("Workspace changed; refresh the dependency catalog".into());
                        json!({"state":"failed","workspaceRevision":revision,
                            "catalogId":null,"packages":[],"symbolCount":0,"warnings":warnings})
                    }
                } else {
                    json!({"state":index_state,"workspaceRevision":revision,
                        "catalogId":null,"packages":[],"symbolCount":0,"warnings":warnings})
                })
            })
            .await?;
            Ok((Json(payload).into_response(), catching_up))
        }
        (&Method::GET, "dependencies/symbols") => {
            let Query(q) =
                Query::<DependencySymbolsQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            if q.catalog_id.is_empty()
                || q.catalog_id.len() > 8192
                || q.q.len() > 8192
                || q.package_id.as_ref().is_some_and(|id| id.len() > 8192)
                || !(1..=200).contains(&q.limit)
                || q.offset > 50_000
            {
                return Err(invalid());
            }
            let state = runtime.browser_scheduler()?;
            let (payload, catching_up) = selected_provider_read(runtime, hook, move |r| {
                let revision = r.status().map_err(ApiError::from)?.revision;
                let catalog = selected_catalog(&state, revision, &q.catalog_id)?;
                if q.package_id.as_ref().is_some_and(|id| !catalog.packages.iter().any(|p| &p.id == id)) { return Err(missing()); }
                let search = q.q.to_lowercase();
                let mut matches = catalog.symbols.iter().filter(|symbol| {
                    q.package_id.as_ref().is_none_or(|id| &symbol.package_id == id)
                    && (search.is_empty() || symbol.qualified_name.to_lowercase().contains(&search)
                        || symbol.name.to_lowercase().contains(&search))
                }).skip(q.offset);
                let items: Vec<_> = matches.by_ref().take(q.limit).collect();
                let next_offset = matches.next().map(|_| q.offset + items.len());
                let payload = json!({"catalogId":catalog.id,"workspaceRevision":catalog.workspace_revision,
                    "items":items,"nextOffset":next_offset});
                if !state.catalog_snapshot(revision).is_some_and(|current| Arc::ptr_eq(&current, &catalog)) { return Err(dependency_stale()); }
                Ok(payload)
            }).await?;
            Ok((Json(payload).into_response(), catching_up))
        }
        (&Method::GET, "dependencies/source") => {
            let Query(q) =
                Query::<DependencySourceQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            if q.catalog_id.is_empty()
                || q.catalog_id.len() > 8192
                || q.source_ref.is_empty()
                || q.source_ref.len() > 8192
            {
                return Err(invalid());
            }
            let state = runtime.browser_scheduler()?;
            let (payload, catching_up) = selected_provider_read(runtime, hook, move |r| {
                let revision = r.status().map_err(ApiError::from)?.revision;
                let catalog = selected_catalog(&state, revision, &q.catalog_id)?;
                let source = catalog.sources.get(&q.source_ref).ok_or_else(missing)?;
                let text = source.directory.read_file(&source.path).map_err(rust_source_error)?;
                let hash = hex::encode(Sha256::digest(text.as_bytes()));
                if hash != source.hash { return Err(dependency_stale()); }
                let package = catalog.packages.iter().find(|p| p.id == source.package_id).ok_or_else(missing)?;
                let definitions: Vec<_> = catalog.symbols.iter().filter(|symbol| symbol.source_ref == q.source_ref)
                    .map(|symbol| json!({"id":symbol.id,"name":symbol.name,"kind":symbol.kind,
                        "parent":symbol.parent,"path":symbol.path,"range":symbol.range})).collect();
                let file = SourceFile { path: source.path.clone(), hash: hash.clone(), language: "rust".into(), text };
                let payload = json!({"id":q.source_ref,"rootId":package.id,"rootLabel":package.name,
                    "path":source.path,"hash":hash,"file":file,"definitions":definitions,
                    "warnings":["Definitional candidates only; not confirmed callees. Separate from the workspace graph and source-sharing scope."]});
                if !state.catalog_snapshot(revision).is_some_and(|current| Arc::ptr_eq(&current, &catalog)) { return Err(dependency_stale()); }
                Ok(payload)
            }).await?;
            Ok((Json(payload).into_response(), catching_up))
        }
        (&Method::GET, "rust-sources") => {
            if uri.query().is_some() {
                return Err(invalid());
            }
            let state = runtime.browser_scheduler()?;
            let (payload, catching_up) = selected_provider_read(runtime, hook, move |_| {
                Ok(json!({"roots":state.rust_sources.iter().map(|r| json!({"id":r.label,"label":r.label,
                    "path":r.directory.root.to_string_lossy()})).collect::<Vec<_>>() }))
            }).await?;
            Ok((Json(payload).into_response(), catching_up))
        }
        (&Method::GET, "rust-sources/tree") => {
            let Query(q) = Query::<RustTreeQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            if !crate::file_tree::valid_path(&q.path)
                || !(1..=200).contains(&q.limit)
                || q.offset > crate::file_tree::SCAN_LIMIT
            {
                return Err(browse_invalid());
            }
            let state = runtime.browser_scheduler()?;
            let (page, catching_up) = selected_provider_read(runtime, hook, move |r| {
                let revision = r.status().map_err(ApiError::from)?.revision;
                let root = state
                    .rust_sources
                    .iter()
                    .find(|root| root.label == q.root)
                    .ok_or_else(missing)?;
                let (items, next_offset, truncated) = root
                    .directory
                    .list(&q.path, q.offset, q.limit)
                    .map_err(rust_source_error)?;
                Ok(crate::file_tree::Page {
                    root: root.directory.root.to_string_lossy().into_owned(),
                    indexed_workspace: String::new(),
                    path: q.path,
                    revision,
                    items,
                    next_offset,
                    truncated,
                })
            })
            .await?;
            Ok((Json(page).into_response(), catching_up))
        }
        (&Method::GET, "rust-sources/file") => {
            let Query(q) = Query::<RustFileQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            if q.path.is_empty()
                || !crate::file_tree::valid_path(&q.path)
                || !q.path.ends_with(".rs")
            {
                return Err(browse_invalid());
            }
            let state = runtime.browser_scheduler()?;
            let (file, catching_up) = selected_provider_read(runtime, hook, move |_| {
                let root = state
                    .rust_sources
                    .iter()
                    .find(|root| root.label == q.root)
                    .ok_or_else(missing)?;
                root.snapshot(&q.path).map_err(rust_source_error)
            })
            .await?;
            Ok((Json(file).into_response(), catching_up))
        }
        (&Method::GET, "status") => {
            if uri.query().is_some() {
                return Err(invalid());
            }
            let runtime = runtime.clone();
            let (status, catching_up) = tokio::task::spawn_blocking(move || {
                let (response, catching_up) = runtime.evidence_response()?;
                let mut status = response.status()?;
                status.catching_up = catching_up;
                if let Some(hook) = hook {
                    hook("before_read_finish");
                }
                Ok::<_, anyhow::Error>((response.finish(status)?, catching_up))
            })
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Operation failed",
                )
            })??;
            Ok((Json(status).into_response(), catching_up))
        }
        (&Method::GET, "tree") => {
            let Query(q) = Query::<TreeQuery>::try_from_uri(uri).map_err(|_| browse_invalid())?;
            if !crate::file_tree::valid_path(&q.path)
                || !(1..=200).contains(&q.limit)
                || q.offset > crate::file_tree::SCAN_LIMIT
            {
                return Err(browse_invalid());
            }
            let state = runtime.browser_scheduler()?;
            let (page, catching_up) = selected_evidence(runtime, hook, move |response| {
                let (mut items, next_offset, truncated) = state
                    .browser
                    .list(&q.path, q.offset, q.limit)
                    .map_err(BrowseDirectoryError)?;
                let (revision, indexed_workspace) =
                    response.tree_metadata(&state.browser.root, &mut items)?;
                for item in items
                    .iter_mut()
                    .filter(|item| item.kind == "file" && item.indexed_path.is_none())
                {
                    let absolute = state.browser.root.join(&item.path);
                    let reason = if !absolute.starts_with(&indexed_workspace) {
                        "Outside indexed workspace"
                    } else {
                        match absolute.extension().and_then(|e| e.to_str()) {
                            Some("ts" | "tsx") => "TypeScript indexing not supported yet",
                            Some("js" | "mjs" | "cjs" | "rs" | "java" | "py") => {
                                "Not indexed yet (may be excluded or size-limited)"
                            }
                            _ => "Unsupported source type",
                        }
                    };
                    item.unindexed_reason = Some(reason.into());
                }
                Ok(crate::file_tree::Page {
                    root: state.browser.root.to_string_lossy().into_owned(),
                    indexed_workspace,
                    path: q.path,
                    revision,
                    items,
                    next_offset,
                    truncated,
                })
            })
            .await?;
            Ok((Json(page).into_response(), catching_up))
        }
        (&Method::GET, "files") => {
            let Query(q) = Query::<FilesQuery>::try_from_uri(uri).map_err(|_| browse_invalid())?;
            if !(1..=200).contains(&q.limit) || q.offset > i64::MAX as usize {
                return Err(browse_invalid());
            }
            let pin = q.pin.pin()?;
            let (value, catching_up) =
                selected_evidence(runtime, hook, move |r| r.files_at(pin, q.offset, q.limit))
                    .await?;
            Ok((Json(value).into_response(), catching_up))
        }
        (&Method::GET, "methods") => {
            let Query(q) = Query::<SourceQuery>::try_from_uri(uri).map_err(|_| browse_invalid())?;
            if q.path.is_empty()
                || q.path.len() > 8192
                || q.path.contains(['\0', '\\', ':'])
                || q.path
                    .split('/')
                    .any(|p| p.is_empty() || p == "." || p == "..")
            {
                return Err(browse_invalid());
            }
            let pin = q.pin.pin()?;
            let (value, catching_up) =
                selected_evidence(runtime, hook, move |r| r.methods_at(&q.path, pin)).await?;
            Ok((
                Json(value.ok_or_else(missing)?).into_response(),
                catching_up,
            ))
        }
        (&Method::GET, "classes") => {
            let Query(q) = Query::<ClassesQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            let pin = q.pin.pin()?;
            let (value, catching_up) = selected_evidence(runtime, hook, move |r| {
                r.classes_at(q.path.as_deref(), &q.q, pin, q.offset, q.limit)
            })
            .await?;
            Ok((Json(value).into_response(), catching_up))
        }
        (&Method::GET, "symbols") => {
            let Query(q) = Query::<SymbolsQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            if q.q.len() > 8192 || !(1..=150).contains(&q.limit) {
                return Err(invalid());
            }
            let ((revision, items), catching_up) =
                selected_evidence(runtime, hook, move |r| r.symbols_at(&q.q, q.limit)).await?;
            Ok((
                Json(json!({"revision":revision,"items":items})).into_response(),
                catching_up,
            ))
        }
        (&Method::GET, "symbol") => {
            let Query(q) = Query::<SymbolQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            if q.id.is_empty() || q.id.len() > 8192 || q.id.contains('\0') {
                return Err(invalid());
            }
            let pin = q.pin.pin()?;
            let (value, catching_up) =
                selected_evidence(runtime, hook, move |r| r.symbol_at(&q.id, pin)).await?;
            let (revision, symbol) = value.ok_or_else(missing)?;
            Ok((
                Json(json!({"revision":revision,"symbol":symbol})).into_response(),
                catching_up,
            ))
        }
        (&Method::POST, "sequence") => {
            let q: SequenceRequest = serde_json::from_slice(&body).map_err(|_| browse_invalid())?;
            if q.seed.is_empty() || q.seed.len() > 8192 || q.seed.contains('\0') {
                return Err(browse_invalid());
            }
            let (value, catching_up) = selected_evidence(runtime, hook, move |r| {
                r.sequence_at(&q.seed, q.expected_revision, q.show_all)
            })
            .await?;
            Ok((
                Json(value.ok_or_else(missing)?).into_response(),
                catching_up,
            ))
        }
        (&Method::POST, "class-diagram") => {
            let q: crate::class_diagram::ClassDiagramRequest =
                serde_json::from_slice(&body).map_err(|_| invalid())?;
            q.validate()?;
            let (value, catching_up) =
                selected_evidence(runtime, hook, move |r| r.class_diagram_at(&q)).await?;
            Ok((Json(value).into_response(), catching_up))
        }
        (&Method::POST, "navigation") => {
            if body.len() > 32 * 1024 {
                return Err(invalid());
            }
            let q: crate::navigation::NavigationRequest =
                serde_json::from_slice(&body).map_err(|_| invalid())?;
            q.validate()?;
            let (value, catching_up) =
                selected_evidence(runtime, hook, move |r| r.navigation_at(&q)).await?;
            Ok((Json(value).into_response(), catching_up))
        }
        (&Method::GET, "views") => {
            let Query(q) = Query::<PinQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            let pin = q.pin()?;
            let (value, catching_up) =
                selected_evidence(runtime, hook, move |r| r.saved_views_at(pin)).await?;
            Ok((Json(value).into_response(), catching_up))
        }
        (&Method::GET, "annotations") => {
            let Query(q) = Query::<PinQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            let pin = q.pin()?;
            let (value, catching_up) =
                selected_evidence(runtime, hook, move |r| r.saved_annotations_at(pin)).await?;
            Ok((Json(value).into_response(), catching_up))
        }
        (&Method::POST, "questions/preview") => {
            let request: QuestionRequest = serde_json::from_slice(&body).map_err(|_| invalid())?;
            request.validate().map_err(|_| invalid())?;
            let state = runtime.browser_scheduler()?;
            let (value, catching_up) = selected_question(runtime, hook, move |r| {
                r.validate_pin(request.expected_revision)?;
                let packet = planning::prepare_in(r, request)?;
                let selection = planning::preview(&packet)?;
                let view = planning::assemble(&packet, &selection, "localPreview")?;
                let bytes = serde_json::to_vec(&packet)?.len();
                anyhow::ensure!(
                    bytes <= MAX_PACKET_BYTES,
                    "complete question packet exceeds 1 MiB"
                );
                let result = QuestionPreview {
                    packet: packet.clone(),
                    selection,
                    view,
                };
                state
                    .packets
                    .lock()
                    .unwrap()
                    .remember_fenced(Arc::new(packet), bytes, || r.finish(()))?;
                Ok(result)
            })
            .await?;
            Ok((Json(value).into_response(), catching_up))
        }
        (&Method::GET, "source") => {
            let Query(q) = Query::<SourceQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            if q.path.is_empty()
                || q.path.len() > 8192
                || q.path.contains(['\0', '\\', ':'])
                || q.path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
            {
                return Err(invalid());
            }
            let pin = q.pin.pin()?;
            let runtime = runtime.clone();
            let (file, catching_up) = tokio::task::spawn_blocking(move || {
                let (response, catching_up) = runtime.evidence_response()?;
                let file = response.source_at(&q.path, pin)?;
                Ok::<_, anyhow::Error>((response.finish(file)?, catching_up))
            })
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Operation failed",
                )
            })??;
            let (revision, file) = file.ok_or_else(missing)?;
            Ok((
                Json(json!({"revision":revision,"file":file})).into_response(),
                catching_up,
            ))
        }
        (&Method::POST, "query") => {
            let Query(pin) = Query::<PinQuery>::try_from_uri(uri).map_err(|_| invalid())?;
            let expected = pin.pin()?;
            let q: ViewQuery = serde_json::from_slice(&body).map_err(|_| invalid())?;
            q.validate().map_err(|_| invalid())?;
            let runtime = runtime.clone();
            let (view, catching_up) = tokio::task::spawn_blocking(move || {
                let (response, catching_up) = runtime.evidence_response()?;
                let view = response.query_view_at(&q, expected.as_ref())?;
                Ok::<_, anyhow::Error>((response.finish(view)?, catching_up))
            })
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Operation failed",
                )
            })??;
            Ok((Json(view.ok_or_else(missing)?).into_response(), catching_up))
        }
        _ if suffix.starts_with("views/") => {
            let id = suffix.strip_prefix("views/").unwrap();
            if id.contains('/') {
                return Err(missing());
            }
            validate_record_id(id).map_err(|_| invalid())?;
            match *method {
                Method::GET => {
                    let Query(q) = Query::<PinQuery>::try_from_uri(uri).map_err(|_| invalid())?;
                    let pin = q.pin()?;
                    let id = id.to_owned();
                    let (value, catching_up) =
                        selected_evidence(runtime, hook, move |r| r.saved_view_at(&id, pin))
                            .await?;
                    Ok((
                        Json(value.ok_or_else(missing)?).into_response(),
                        catching_up,
                    ))
                }
                Method::PUT => {
                    let Query(q) = Query::<PinQuery>::try_from_uri(uri).map_err(|_| invalid())?;
                    let pin = q.pin()?.ok_or_else(invalid)?;
                    let value: SavedViewRequest =
                        serde_json::from_slice(&body).map_err(|_| invalid())?;
                    if value.id != id {
                        return Err(invalid());
                    }
                    value.validate().map_err(|_| invalid())?;
                    return selected_mutation(
                        runtime,
                        hook,
                        move |r| {
                            r.validate_pin(pin)?;
                            anyhow::ensure!(
                                r.status()?.revision == pin,
                                "revision conflict: mutation requires head"
                            );
                            Ok(())
                        },
                        move |r| r.save_view_at(pin, &value).map(Json),
                    )
                    .await;
                }
                Method::DELETE => {
                    let id = id.to_owned();
                    return selected_mutation(
                        runtime,
                        hook,
                        |_| Ok(()),
                        move |r| r.delete_view(&id).map(|_| StatusCode::NO_CONTENT),
                    )
                    .await;
                }
                _ => Err(missing()),
            }
        }
        _ if suffix.starts_with("annotations/") => {
            let id = suffix.strip_prefix("annotations/").unwrap();
            if id.contains('/') {
                return Err(missing());
            }
            validate_record_id(id).map_err(|_| invalid())?;
            match *method {
                Method::PUT => {
                    let Query(q) = Query::<PinQuery>::try_from_uri(uri).map_err(|_| invalid())?;
                    let pin = q.pin()?.ok_or_else(invalid)?;
                    let value: AnnotationRequest =
                        serde_json::from_slice(&body).map_err(|_| invalid())?;
                    if value.id != id {
                        return Err(invalid());
                    }
                    value.validate().map_err(|_| invalid())?;
                    return selected_mutation(
                        runtime,
                        hook,
                        move |r| {
                            r.validate_pin(pin)?;
                            anyhow::ensure!(
                                r.status()?.revision == pin,
                                "revision conflict: mutation requires head"
                            );
                            Ok(())
                        },
                        move |r| r.save_annotation_at(pin, &value).map(Json),
                    )
                    .await;
                }
                Method::DELETE => {
                    let id = id.to_owned();
                    return selected_mutation(
                        runtime,
                        hook,
                        |_| Ok(()),
                        move |r| r.delete_annotation(&id).map(|_| StatusCode::NO_CONTENT),
                    )
                    .await;
                }
                _ => Err(missing()),
            }
        }
        _ if suffix.starts_with("questions/") => {
            let tail = suffix.strip_prefix("questions/").unwrap();
            let Some((id, action)) = tail.split_once('/') else {
                return Err(missing());
            };
            if id.is_empty() || action.contains('/') {
                return Err(missing());
            }
            if !matches!(
                (method.clone(), action),
                (Method::GET, "jev-request")
                    | (Method::POST, "jev-response")
                    | (Method::POST, "selection")
            ) {
                return Err(missing());
            }
            let state = runtime.browser_scheduler()?;
            let id = id.to_owned();
            let bytes = body.clone();
            let action = action.to_owned();
            let (result, catching_up) = selected_question(runtime, hook, move |r| {
                let revision = r.status()?.revision;
                let packet = state
                    .packets
                    .lock()
                    .unwrap()
                    .packets
                    .iter()
                    .find(|(packet, _)| packet.packet_id == id)
                    .map(|(packet, _)| packet.clone())
                    .ok_or_else(|| anyhow::anyhow!("question seed not found"))?;
                anyhow::ensure!(
                    revision == packet.revision,
                    "revision conflict: cached packet changed"
                );
                r.validate_selected_view(&packet.context, &packet.source_files)?;
                match action.as_str() {
                    "jev-request" => Ok(serde_json::to_value(jev::request_for(&packet)?)?),
                    "jev-response" => {
                        let value: Value = serde_json::from_slice(&bytes)
                            .map_err(|_| anyhow::anyhow!("invalid_question_selection"))?;
                        let selection = jev::parse_response(&packet, &value)?;
                        let warnings = jev::response_warnings(&value);
                        let mut view = planning::assemble(&packet, &selection, "importedJev")?;
                        view.warnings.extend(warnings);
                        Ok(json!({"selection":selection,"view":view}))
                    }
                    "selection" => {
                        let selection: SelectionEnvelope = serde_json::from_slice(&bytes)
                            .map_err(|_| anyhow::anyhow!("invalid_question_selection"))?;
                        let view = planning::assemble(&packet, &selection, "manual")?;
                        Ok(json!({"selection":selection,"view":view}))
                    }
                    _ => unreachable!(),
                }
            })
            .await?;
            Ok((Json(result).into_response(), catching_up))
        }
        (&Method::POST, "index") | (&Method::GET, "jobs/current") => {
            let state = runtime.browser_scheduler()?;
            let response = if method == Method::POST {
                start_index(State(state), body).await?.into_response()
            } else {
                current_job(State(state)).await?.into_response()
            };
            Ok((response, runtime.catching_up()))
        }
        _ if suffix.starts_with("jobs/") => {
            let state = runtime.browser_scheduler()?;
            let tail = suffix.strip_prefix("jobs/").unwrap();
            let response = if method == Method::GET && !tail.contains('/') {
                job(State(state), Path(tail.to_owned()))
                    .await?
                    .into_response()
            } else if method == Method::POST && tail.ends_with("/cancel") {
                let id = tail.strip_suffix("/cancel").unwrap();
                if id.contains('/') {
                    return Err(missing());
                }
                cancel_job(State(state), Path(id.to_owned()))
                    .await?
                    .into_response()
            } else {
                return Err(missing());
            };
            Ok((response, runtime.catching_up()))
        }
        _ => Err(missing()),
    }
}
