//! Loopback-only HTTP API. Database and indexing work runs on blocking workers.
use crate::{
    auth::valid_token,
    dependencies::{Catalog, CatalogOptions},
    indexer::IndexOptions,
    model::*,
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
use axum::{extract::DefaultBodyLimit, routing::post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

mod api_error;
pub(crate) mod architecture;
mod cli_api;
mod config;
#[cfg(feature = "test-support")]
mod direct;
mod query;
mod selection;
mod state;
mod worker;
use api_error::{ApiError, error, invalid, missing};
pub use architecture::ArchitectureBuilding;
pub use cli_api::{CLI_API_BODY_LIMIT, CLI_API_RESPONSE_LIMIT, cli_api, cli_api_route};
pub use config::BrowserConfig;
#[cfg(all(test, feature = "test-support"))]
use direct::native_response_route;
#[cfg(feature = "test-support")]
pub use direct::router;
use query::{architecture_details_query, architecture_query, metrics_query};
use selection::{
    browser_selection_error, selected_catalog, selected_catalog_read, selected_evidence,
    selected_mutation_error, selected_refresh_postflight, selected_refresh_preflight,
};
pub use state::{
    new, new_with_browser_root, new_with_config, new_with_dependency_options, new_with_source_roots,
};
pub use worker::{AdmissionLease, RetireOutcome, RuntimePhase};
use worker::{RuntimeState, STARTUP_BACKOFF_FIRST};

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
struct DependencyIndex {
    generation: u64,
    cancel: Arc<AtomicBool>,
    stopped: bool,
    worker_running: bool,
    worker: Option<std::thread::JoinHandle<()>>,
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
    /// Decision 0009 §2: the checkout's one runtime state, shared by the worker
    /// and every reader. `Ready` holds the only serving writer session.
    runtime: Mutex<RuntimeState>,
    /// The first error of the current startup reconciliation run, cleared when
    /// a validated head commits.
    startup_error: Mutex<Option<String>>,
    /// Failed non-contention recovery attempts in the current reconciliation run.
    startup_failures: AtomicU64,
    /// A queue disposition must be confirmed before reconciliation can finish.
    recovery_disposition: Mutex<Option<String>>,
    /// The typed reason recovery is blocked for, without paths (#768); cleared
    /// when a validated head commits.
    recovery_blocked: Mutex<Option<&'static str>>,
    #[cfg(test)]
    test_worker_now: Mutex<Option<Instant>>,
    #[cfg(test)]
    test_startup_error: Mutex<Option<anyhow::Error>>,
    /// The next failed startup reconciliation waits this long (doubling, capped).
    startup_backoff: Mutex<Duration>,
    /// The current browser configuration (Decision 0009 §6), replaced whole.
    config: std::sync::RwLock<Arc<BrowserConfig>>,
    /// Explicit index options a new configuration brought, applied by the
    /// worker's next full watcher inventory and then recorded.
    explicit_pending: Mutex<Option<Arc<IndexOptions>>>,
    /// The worker's wake inbox. The queue waker, registered with every Store
    /// for this root, also marks FIFO work pending at the enqueue COMMIT; the
    /// watch waker only wakes.
    inbox: Arc<worker::WorkerInbox>,
    /// Owned here; the Store registry holds it weakly.
    _queue_waker: Arc<dyn Fn() + Send + Sync>,
    watch_waker: Arc<dyn Fn() + Send + Sync>,
    /// Hold the worker at the start of its next step.
    #[cfg(feature = "test-support")]
    worker_hold: std::sync::RwLock<()>,
    /// The last step made progress, so more work may be ready at once.
    step_progress: AtomicBool,
    /// Published watcher intent; readers never take the watcher's lock.
    /// The watcher's published status. This lock is also the publication lock of
    /// `fifo`: both are written under it, so the quiet read sees one state (#1082).
    /// A leaf lock: nothing else is taken while it is held.
    watch_status: Arc<Mutex<Option<worker::WatchStatus>>>,
    /// Read and enqueue admission leases held against release.
    leases: Arc<std::sync::atomic::AtomicUsize>,
    /// Published by the worker after each step (and set by the enqueue gate):
    /// accepted FIFO work keeps the runtime from retiring.
    fifo: Arc<worker::FifoPublication>,
    /// The registry's lifecycle wake (Decision 0009 §5).
    lifecycle: Arc<std::sync::OnceLock<Arc<dyn Fn() + Send + Sync>>>,
    /// The runtime closed because its captured root was lost.
    root_lost: AtomicBool,
    /// Every runtime state transition bumps this and signals `state_changed`.
    state_version: AtomicU64,
    state_changed: std::sync::Condvar,
    /// Pause retirement before its `Closing` linearization point, and after it.
    #[cfg(feature = "test-support")]
    retire_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(feature = "test-support")]
    retirement_entered_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(feature = "test-support")]
    closing_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs as a maintenance unit begins, after its priority check passed.
    #[cfg(feature = "test-support")]
    maintenance_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs as a catalog build begins, before it reads the workspace revision
    /// (`"before_revision"`), and after it published its outcome (`"published"`).
    #[cfg(feature = "test-support")]
    dependency_hook: Mutex<Option<DependencyHook>>,
    /// Install each watcher with asynchronous OS delivery. Off by default in a
    /// test-support build, so a late OS event for a fixture's own setup write
    /// cannot be honest catch-up at an unchanged head (#703, #720); a watcher
    /// test opts in.
    #[cfg(feature = "test-support")]
    native_watcher: AtomicBool,
    dependencies: Mutex<DependencyIndex>,
    #[cfg(test)]
    dependency_idle: tokio::sync::Notify,
    #[cfg(test)]
    dependency_build_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    dependency_stop_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(all(test, feature = "test-support"))]
    dependency_capture_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(all(test, feature = "test-support"))]
    outer_fence_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(feature = "test-support")]
    token: String,
    #[cfg(feature = "test-support")]
    hosts: Vec<String>,
    #[cfg(feature = "test-support")]
    origins: Vec<String>,
    jobs: Mutex<Jobs>,
    worker_started: AtomicBool,
    search_fill: Mutex<Option<crate::daemon::search_fill::FillRegistration>>,
    /// The checkout's one Architecture owner (#556).
    architecture: architecture::ArchitectureHandle,
    /// Watcher inventories run on the worker and are never cancelled from
    /// outside it (Decision 0009 §1).
    watcher_cancel: CancelFlag,
    #[cfg(feature = "test-support")]
    startup_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// A failed startup reconciliation's backoff stays open until retirement.
    #[cfg(feature = "test-support")]
    hold_startup_backoff: AtomicBool,
    /// Startup reconciliation captures begun, for the single-owner tests.
    #[cfg(feature = "test-support")]
    startup_captures: AtomicU64,
    maintenance_telemetry: Mutex<MaintenanceTelemetry>,
    maintenance_error_limiter: Mutex<MaintenanceErrorLimiter>,
    #[cfg(test)]
    test_pending_read_failures: AtomicUsize,
    #[cfg(test)]
    test_activation_head_busy_once: AtomicBool,
    pending_requests: Mutex<Vec<String>>,
    job_progress: Mutex<BTreeMap<String, IndexProgress>>,
    /// One worker step at a time. In production only the worker takes it; test
    /// fixtures that drive steps by hand take it to serialize with the worker.
    step_lock: Mutex<()>,
    watcher_work: Mutex<
        Option<(
            std::sync::Weak<crate::store::topology::WriterSession>,
            crate::index_coordinator::WatcherWork,
        )>,
    >,
    #[cfg(feature = "test-support")]
    causal_witness: Mutex<Option<Arc<crate::daemon::causal_witness::CausalWitness>>>,
    #[cfg(feature = "test-support")]
    causal_runtime: Mutex<Option<std::sync::Weak<crate::daemon::registry::CheckoutRuntime>>>,
    #[cfg(feature = "test-support")]
    causal_lineage_ordinal: Arc<AtomicU64>,
    #[cfg(feature = "test-support")]
    causal_startup_reported: Mutex<Option<(uuid::Uuid, uuid::Uuid, u64)>>,
    #[cfg(feature = "test-support")]
    causal_watch_reported: Mutex<Option<crate::daemon::causal_witness::WatchAck>>,
    recovery_retry_after: Mutex<Option<Instant>>,
    /// Watcher health, published by the current watcher through one flag that
    /// lives as long as this state. Readers never wait for watcher work.
    watcher_health: Arc<crate::watch::WatcherHealth>,
    retention_last_run: Mutex<Instant>,
    gc_last_check: Mutex<Instant>,
    #[cfg(test)]
    test_queue_before_stream: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_after_stream: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_after_pending_snapshot: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_after_startup_drain: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_before_preclaim: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_before_watcher_reconcile: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_after_watcher_reconcile: crate::store::TestOneShotHook,
    #[cfg(test)]
    test_queue_current_phase: std::sync::atomic::AtomicU8,
    /// Runs in a session change, which holds the runtime state, before it clears the
    /// published watch status.
    #[cfg(test)]
    test_session_change_before_watch_status: crate::store::TestOneShotHook,
    /// Runs in a watcher-intent read that found no published status, before it reads the
    /// runtime state.
    #[cfg(test)]
    test_watch_pending_before_state: crate::store::TestOneShotHook,
    /// Runs in the quiet read, between its read of the FIFO flag and its read of the watch
    /// status.
    #[cfg(test)]
    test_quiet_between_reads: crate::store::TestOneShotHook,
    /// Runs in the worker's FIFO publication, after it observed the queue and before it
    /// publishes what it observed.
    #[cfg(test)]
    test_fifo_between_observation_and_publication: crate::store::TestOneShotHook,
}
impl Drop for DaemonState {
    /// A lossless shutdown: an idle worker waiting without a deadline exits.
    fn drop(&mut self) {
        self.inbox.shut_down();
    }
}

/// Diagnostic logging must never kill the durable queue worker if the
/// launching parent closed its stderr pipe after the server banner.
fn best_effort_queue_stderr(mut output: impl std::io::Write, args: std::fmt::Arguments<'_>) {
    let _ = output.write_fmt(args);
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
/// One embedded web app file: the route path, its `Content-Type`, and the bytes.
pub struct WebAsset {
    pub path: &'static str,
    pub content_type: &'static str,
    pub body: &'static [u8],
}

const fn web_asset(
    path: &'static str,
    content_type: &'static str,
    body: &'static [u8],
) -> WebAsset {
    WebAsset {
        path,
        content_type,
        body,
    }
}

const HTML: &str = "text/html; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const TEXT: &str = "text/plain; charset=utf-8";
const WOFF2: &str = "font/woff2";

/// Every embedded web app file. The checkout router and the provisioned browser
/// router both serve exactly these rows, with no per-router difference in a row.
/// A new file under `web/` needs one line here;
/// `web_asset_table_covers_the_web_directory` fails without it.
pub const WEB_ASSETS: &[WebAsset] = &[
    web_asset("/", HTML, include_bytes!("../web/index.html")),
    web_asset("/app.js", JS, include_bytes!("../web/app.js")),
    web_asset("/app-state.js", JS, include_bytes!("../web/app-state.js")),
    web_asset("/app-api.js", JS, include_bytes!("../web/app-api.js")),
    web_asset("/app-agent.js", JS, include_bytes!("../web/app-agent.js")),
    web_asset(
        "/app-navigation.js",
        JS,
        include_bytes!("../web/app-navigation.js"),
    ),
    web_asset("/app-status.js", JS, include_bytes!("../web/app-status.js")),
    web_asset("/app-source.js", JS, include_bytes!("../web/app-source.js")),
    web_asset("/app-files.js", JS, include_bytes!("../web/app-files.js")),
    web_asset("/app-tree.js", JS, include_bytes!("../web/app-tree.js")),
    web_asset(
        "/app-sequence.js",
        JS,
        include_bytes!("../web/app-sequence.js"),
    ),
    web_asset(
        "/app-libraries.js",
        JS,
        include_bytes!("../web/app-libraries.js"),
    ),
    web_asset("/app-views.js", JS, include_bytes!("../web/app-views.js")),
    web_asset(
        "/viewer-metrics.js",
        JS,
        include_bytes!("../web/viewer-metrics.js"),
    ),
    web_asset(
        "/metric-heat.js",
        JS,
        include_bytes!("../web/metric-heat.js"),
    ),
    web_asset("/files.js", JS, include_bytes!("../web/files.js")),
    web_asset("/treemap.js", JS, include_bytes!("../web/treemap.js")),
    web_asset(
        "/treemap-pele.js",
        JS,
        include_bytes!("../web/treemap-pele.js"),
    ),
    web_asset("/treemap.css", CSS, include_bytes!("../web/treemap.css")),
    web_asset(
        "/architecture-apps.js",
        JS,
        include_bytes!("../web/architecture-apps.js"),
    ),
    web_asset(
        "/architecture-display.js",
        JS,
        include_bytes!("../web/architecture-display.js"),
    ),
    web_asset(
        "/deviations-model.js",
        JS,
        include_bytes!("../web/deviations-model.js"),
    ),
    web_asset(
        "/architecture-deviations.js",
        JS,
        include_bytes!("../web/architecture-deviations.js"),
    ),
    web_asset(
        "/architecture-drawing.js",
        JS,
        include_bytes!("../web/architecture-drawing.js"),
    ),
    web_asset(
        "/architecture-app-columns.js",
        JS,
        include_bytes!("../web/architecture-app-columns.js"),
    ),
    web_asset(
        "/architecture-inspector.js",
        JS,
        include_bytes!("../web/architecture-inspector.js"),
    ),
    web_asset(
        "/architecture.js",
        JS,
        include_bytes!("../web/architecture.js"),
    ),
    web_asset(
        "/architecture-routing.js",
        JS,
        include_bytes!("../web/architecture-routing.js"),
    ),
    web_asset(
        "/architecture-pele.js",
        JS,
        include_bytes!("../web/architecture-pele.js"),
    ),
    web_asset(
        "/architecture.css",
        CSS,
        include_bytes!("../web/architecture.css"),
    ),
    web_asset("/files.css", CSS, include_bytes!("../web/files.css")),
    web_asset("/shell.js", JS, include_bytes!("../web/shell.js")),
    web_asset(
        "/fonts/jetbrains-mono-latin.woff2",
        WOFF2,
        include_bytes!("../web/fonts/jetbrains-mono-latin.woff2"),
    ),
    web_asset(
        "/fonts/inter-variable.woff2",
        WOFF2,
        include_bytes!("../web/fonts/inter-variable.woff2"),
    ),
    web_asset(
        "/fonts/manrope-variable.woff2",
        WOFF2,
        include_bytes!("../web/fonts/manrope-variable.woff2"),
    ),
    web_asset(
        "/fonts/JetBrainsMono-OFL.txt",
        TEXT,
        include_bytes!("../web/fonts/JetBrainsMono-OFL.txt"),
    ),
    web_asset(
        "/fonts/Inter-OFL.txt",
        TEXT,
        include_bytes!("../web/fonts/Inter-OFL.txt"),
    ),
    web_asset(
        "/fonts/Manrope-OFL.txt",
        TEXT,
        include_bytes!("../web/fonts/Manrope-OFL.txt"),
    ),
    web_asset(
        "/brand/trellis-mark-transparent-v1.png",
        "image/png",
        include_bytes!("../web/brand/trellis-mark-transparent-v1.png"),
    ),
    web_asset(
        "/brand/trellis-logo-dark-v1.webp",
        "image/webp",
        include_bytes!("../web/brand/trellis-logo-dark-v1.webp"),
    ),
    web_asset("/style.css", CSS, include_bytes!("../web/style.css")),
    web_asset("/classes.js", JS, include_bytes!("../web/classes.js")),
    web_asset("/classes.css", CSS, include_bytes!("../web/classes.css")),
    web_asset("/navigation.js", JS, include_bytes!("../web/navigation.js")),
    web_asset(
        "/navigation.css",
        CSS,
        include_bytes!("../web/navigation.css"),
    ),
    web_asset("/index-pin.js", JS, include_bytes!("../web/index-pin.js")),
    web_asset("/dto-core.js", JS, include_bytes!("../web/dto-core.js")),
    web_asset(
        "/source-witness.js",
        JS,
        include_bytes!("../web/source-witness.js"),
    ),
    web_asset(
        "/context-menu.js",
        JS,
        include_bytes!("../web/context-menu.js"),
    ),
    web_asset("/pele-svg.js", JS, include_bytes!("../web/pele-svg.js")),
    web_asset(
        "/sequence-model.js",
        JS,
        include_bytes!("../web/sequence-model.js"),
    ),
    web_asset("/sequence.js", JS, include_bytes!("../web/sequence.js")),
    web_asset(
        "/sequence-pele.js",
        JS,
        include_bytes!("../web/sequence-pele.js"),
    ),
    web_asset(
        "/vendor/pele-1.10.0-trellis.2.js",
        JS,
        include_bytes!("../web/vendor/pele-1.10.0-trellis.2.js"),
    ),
];

/// Adds one GET route per `WEB_ASSETS` row. Each router keeps its own layers
/// and its own answer for unknown paths.
fn web_asset_routes<S: Clone + Send + Sync + 'static>(mut router: Router<S>) -> Router<S> {
    for asset in WEB_ASSETS {
        router = router.route(
            asset.path,
            get(move || async move { ([(header::CONTENT_TYPE, asset.content_type)], asset.body) }),
        );
    }
    router
}
fn dependency_stale() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "stale_catalog",
        "Dependency catalog or source changed; refresh the catalog",
    )
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
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DependencySourceQuery {
    catalog_id: String,
    source_ref: String,
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
    /// Optional path filter over the whole pinned catalog.
    #[serde(default)]
    q: Option<String>,
}
/// Longest accepted catalog path filter, in bytes.
const FILES_QUERY_MAX: usize = 256;
impl FilesQuery {
    fn validate(&self) -> Result<(), ApiError> {
        let query_ok = self.q.as_deref().is_none_or(|q| {
            !q.is_empty() && q.len() <= FILES_QUERY_MAX && !q.chars().any(char::is_control)
        });
        if !(1..=200).contains(&self.limit) || self.offset > i64::MAX as usize || !query_ok {
            return Err(browse_invalid());
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSymbolsQuery {
    path: String,
    #[serde(flatten)]
    pin: PinQuery,
    #[serde(default)]
    offset: usize,
    #[serde(default = "file_limit")]
    limit: usize,
}
impl FileSymbolsQuery {
    fn validate(&self) -> Result<IndexPin, ApiError> {
        if !crate::files_view::valid_path(&self.path)
            || !(1..=200).contains(&self.limit)
            || self.offset > i64::MAX as usize
        {
            return Err(invalid());
        }
        self.pin.pin()?.ok_or_else(invalid)
    }
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
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SequenceRequest {
    seed: String,
    expected_revision: IndexPin,
    #[serde(default)]
    show_all: bool,
    /// The browser's Interactions view (#663): its candidates before the visible step
    /// bound. Show all keeps the raw bounded view.
    #[serde(default)]
    interactions: bool,
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

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IndexRequest {
    expected_revision: Option<IndexPin>,
}
impl From<crate::store::requests::Request> for IndexJob {
    fn from(row: crate::store::requests::Request) -> Self {
        let error =
            row.error_code.as_deref().map(|stored| {
                match crate::store::requests::split_error(stored) {
                    (code, Some(reason)) => {
                        json!({"code":code,"reason":reason,"message":"Index job failed"})
                    }
                    (code, None) => json!({"code":code,"message":"Index job failed"}),
                }
            });
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
#[cfg(test)]
mod index_job_error_tests {
    use super::IndexJob;
    use crate::store::requests::{Request, RequestKind};
    use serde_json::json;

    fn failed(error_code: &str) -> Request {
        Request {
            seq: 1,
            id: "00000000-0000-4000-8000-000000000001".into(),
            request_kind: RequestKind::SemanticImport,
            state: "failed".into(),
            root_device: "1".into(),
            root_inode: "2".into(),
            options_json: "{}".into(),
            expected: None,
            claim_incarnation: Some("claim".into()),
            revision: None,
            error_code: Some(error_code.into()),
            submitted_at: "1".into(),
            started_at: Some("2".into()),
            finished_at: Some("3".into()),
        }
    }

    #[test]
    fn job_status_exposes_the_recorded_code_and_reason() {
        let job = IndexJob::from(failed("invalid_artifact: empty or reversed SCIP range"));
        assert_eq!(
            job.error,
            Some(
                json!({"code":"invalid_artifact","reason":"empty or reversed SCIP range",
                "message":"Index job failed"})
            )
        );
        let bare = IndexJob::from(failed("revision_conflict"));
        assert_eq!(
            bare.error,
            Some(json!({"code":"revision_conflict","message":"Index job failed"}))
        );
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
    let options = s.config().options.clone();
    let scheduler = s.clone();
    let row = db(s.clone(), move |_| {
        scheduler.enqueue_index(&options, request.expected_revision)
    })
    .await?;
    let job = IndexJob::from(row);
    // A later durable admission supersedes the legacy exceptional display slot.
    s.jobs.lock().unwrap().current = None;
    s.start_worker();
    Ok((StatusCode::ACCEPTED, Json(job)))
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
struct SourceQuery {
    path: String,
    #[serde(flatten)]
    pin: PinQuery,
}

#[cfg(all(test, feature = "test-support"))]
mod live_tests;

#[cfg(test)]
mod serving_holder_tests;

#[cfg(test)]
mod exceptional_recovery_tests;

#[cfg(test)]
mod rebuild_recovery_tests;

#[cfg(test)]
mod recovery_blocked_tests;

#[cfg(all(test, feature = "test-support"))]
mod normal_post_capture_cancellation_tests;

#[cfg(all(test, feature = "test-support"))]
mod dependency_lifecycle_tests;

#[cfg(test)]
mod maintenance_telemetry_tests;

/// The socket-only daemon has no HTTP state. This router is constructed only
/// after a validated explicit serve registration binds a loopback listener.
type SelectedResponseHook = Arc<dyn Fn(&'static str) + Send + Sync>;
/// Observes a dependency catalog build's phases (#700).
#[cfg(feature = "test-support")]
type DependencyHook = Arc<dyn Fn(&'static str) + Send + Sync>;

#[derive(Clone)]
pub struct ProvisionedBrowser {
    registry: Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    token: String,
    hosts: Vec<String>,
    origins: Vec<String>,
    response_hook: Arc<Mutex<Option<SelectedResponseHook>>>,
    metrics_limit: Arc<std::sync::atomic::AtomicUsize>,
    metrics_deadline_ms: Arc<std::sync::atomic::AtomicU64>,
    metrics_test_cancel: Arc<std::sync::atomic::AtomicBool>,
    metrics_clock_skew_ms: Arc<std::sync::atomic::AtomicU64>,
    #[cfg(feature = "test-support")]
    metrics_clock: Arc<Mutex<Option<Instant>>>,
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
        Ok(Self::with_guard(registry, token, hosts, origins))
    }

    /// The same checkout routes for `trellis api` (#817), called directly and never served:
    /// the private daemon socket is the caller's authentication, so it has no token or hosts.
    fn for_socket(
        registry: Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    ) -> Self {
        Self::with_guard(registry, String::new(), Vec::new(), Vec::new())
    }

    fn with_guard(
        registry: Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
        token: String,
        hosts: Vec<String>,
        origins: Vec<String>,
    ) -> Self {
        Self {
            registry,
            token,
            hosts,
            origins,
            response_hook: Arc::new(Mutex::new(None)),
            metrics_limit: Arc::new(std::sync::atomic::AtomicUsize::new(64 * 1024 * 1024)),
            metrics_deadline_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            metrics_test_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            metrics_clock_skew_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(feature = "test-support")]
            metrics_clock: Arc::new(Mutex::new(None)),
        }
    }

    #[doc(hidden)]
    pub fn set_selected_response_hook_for_tests(&self, hook: SelectedResponseHook) {
        *self.response_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(feature = "test-support")]
    pub fn set_metrics_controls_for_tests(
        &self,
        limit: usize,
        deadline: Option<Duration>,
        cancel: bool,
    ) {
        use std::sync::atomic::Ordering;
        self.metrics_limit.store(limit, Ordering::Relaxed);
        self.metrics_deadline_ms.store(
            deadline.map_or(0, |d| d.as_millis().max(1) as u64),
            Ordering::Relaxed,
        );
        self.metrics_test_cancel.store(cancel, Ordering::Relaxed);
    }

    #[cfg(feature = "test-support")]
    pub fn set_metrics_clock_skew_for_tests(&self, milliseconds: u64) {
        self.metrics_clock_skew_ms
            .store(milliseconds, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(feature = "test-support")]
    pub fn set_metrics_clock_for_tests(&self, now: Instant) {
        *self.metrics_clock.lock().unwrap() = Some(now);
    }

    pub fn router(self) -> Router {
        let state = Arc::new(self);
        web_asset_routes(Router::new())
            .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
            .route(
                "/healthz",
                get(|| async { Json(json!({"ok":true,"version":env!("CARGO_PKG_VERSION")})) }),
            )
            .route("/api/checkouts", get(provisioned_checkouts))
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

async fn provisioned_checkouts(
    State(state): State<Arc<ProvisionedBrowser>>,
) -> Result<Response, ApiError> {
    let probe = state.registry.lock().await.browser_checkouts_probe();
    let mut checkouts = tokio::task::spawn_blocking(move || probe.check())
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Operation failed",
            )
        })?;
    enrich_checkout_git(&mut checkouts).await;
    checkout_list_response(checkouts)
}

async fn provision_guard(
    State(state): State<Arc<ProvisionedBrowser>>,
    req: Request,
    next: Next,
) -> Response {
    guard_common(&state.token, &state.hosts, &state.origins, req, next).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GitDiffQuery {
    path: String,
}

fn git_query(route: BrowserCoreRoute, uri: &axum::http::Uri) -> Result<Option<String>, ApiError> {
    if matches!(route, BrowserCoreRoute::GitStatus) {
        return if uri.query().is_none() {
            Ok(None)
        } else {
            Err(invalid())
        };
    }
    let Query(query) = Query::<GitDiffQuery>::try_from_uri(uri).map_err(|_| invalid())?;
    crate::git_status::validate_path(&query.path).map_err(git_request_error)?;
    Ok(Some(query.path))
}

fn git_request_error(error: crate::git_status::GitRequestError) -> ApiError {
    match error {
        crate::git_status::GitRequestError::InvalidPath => ApiError(
            StatusCode::BAD_REQUEST,
            "invalid_git_path",
            "Invalid Git path",
        ),
        crate::git_status::GitRequestError::NotFound => missing(),
    }
}

async fn git_answer(root: &std::path::Path, path: Option<&str>) -> Result<Response, ApiError> {
    match path {
        Some(path) => Ok(Json(
            crate::git_status::diff(root, path)
                .await
                .map_err(git_request_error)?,
        )
        .into_response()),
        None => Ok(Json(crate::git_status::status(root).await).into_response()),
    }
}

async fn enrich_checkout_git(checkouts: &mut [Value]) {
    let Ok(executable) = std::env::current_exe() else {
        for row in checkouts {
            row["git"] = json!(crate::git_status::GitSummary::Unavailable {
                reason: crate::git_status::GitReason::Unreadable,
            });
        }
        return;
    };
    enrich_checkout_git_with_reader(checkouts, &crate::git_status::GitReader::new(executable))
        .await;
}

async fn enrich_checkout_git_with_reader(
    checkouts: &mut [Value],
    reader: &crate::git_status::GitReader,
) {
    use crate::{
        git_status::{GitReason, GitSummary},
        store::topology::WorkspaceIdentity,
    };
    for row in checkouts {
        let identity = row["workspaceRoot"]
            .as_str()
            .filter(|_| row["state"] != "unavailable")
            .and_then(|root| {
                let root = std::path::Path::new(root);
                WorkspaceIdentity::discover_unattached(Some(root), root)
                    .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
                    .ok()
            })
            .filter(|identity| row["rootKey"].as_str() == Some(identity.root_key.as_str()));
        let git = match identity {
            None => GitSummary::Unavailable {
                reason: GitReason::Unreadable,
            },
            Some(identity) => {
                let result = reader.summary(&identity.root).await;
                if identity.verify_readonly().is_err() {
                    GitSummary::Unavailable {
                        reason: GitReason::Unreadable,
                    }
                } else {
                    result
                }
            }
        };
        row["git"] = json!(git);
    }
}

fn checkout_list_response(checkouts: Vec<Value>) -> Result<Response, ApiError> {
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > crate::git_status::RESPONSE_LIMIT.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("response_too_large"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded(Vec::new());
    serde_json::to_writer(&mut writer, &json!({"checkouts": checkouts})).map_err(|_| {
        ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "response_too_large",
            "Response is too large",
        )
    })?;
    Ok(([(header::CONTENT_TYPE, "application/json")], writer.0).into_response())
}

#[derive(Clone, Copy)]
enum BrowserCoreRoute {
    GitStatus,
    GitDiff,
    Metrics,
    Architecture,
    ArchitectureDetails,
    RulesInventory,
    RulesPreview,
    RulesApply,
    RefreshDependencies,
    Dependencies,
    DependencySymbols,
    DependencySource,
    RustSources,
    RustSourceTree,
    RustSourceFile,
    Status,
    StatusHead,
    Tree,
    Files,
    Methods,
    FileSymbols,
    SyntaxMatches,
    Classes,
    Sequence,
    ClassDiagram,
    Navigation,
    Source,
    Index,
    CurrentJob,
    Job,
}

impl BrowserCoreRoute {
    fn parse(method: &axum::http::Method, suffix: &str) -> Option<Self> {
        use axum::http::Method;
        match (method, suffix) {
            (&Method::GET, "git") => Some(Self::GitStatus),
            (&Method::GET, "git/diff") => Some(Self::GitDiff),
            (&Method::GET, "metrics") => Some(Self::Metrics),
            (&Method::GET, "architecture") => Some(Self::Architecture),
            (&Method::GET, "architecture/details") => Some(Self::ArchitectureDetails),
            (&Method::GET, "architecture/rules") => Some(Self::RulesInventory),
            (&Method::POST, "architecture/rules/preview") => Some(Self::RulesPreview),
            (&Method::POST, "architecture/rules/apply") => Some(Self::RulesApply),
            (&Method::POST, "dependencies/refresh") => Some(Self::RefreshDependencies),
            (&Method::GET, "dependencies") => Some(Self::Dependencies),
            (&Method::GET, "dependencies/symbols") => Some(Self::DependencySymbols),
            (&Method::GET, "dependencies/source") => Some(Self::DependencySource),
            (&Method::GET, "rust-sources") => Some(Self::RustSources),
            (&Method::GET, "rust-sources/tree") => Some(Self::RustSourceTree),
            (&Method::GET, "rust-sources/file") => Some(Self::RustSourceFile),
            (&Method::GET, "status") => Some(Self::Status),
            (&Method::GET, "status/head") => Some(Self::StatusHead),
            (&Method::GET, "tree") => Some(Self::Tree),
            (&Method::GET, "files") => Some(Self::Files),
            (&Method::GET, "methods") => Some(Self::Methods),
            (&Method::GET, "symbols") => Some(Self::FileSymbols),
            (&Method::POST, "syntax-matches") => Some(Self::SyntaxMatches),
            (&Method::GET, "classes") => Some(Self::Classes),
            (&Method::POST, "sequence") => Some(Self::Sequence),
            (&Method::POST, "class-diagram") => Some(Self::ClassDiagram),
            (&Method::POST, "navigation") => Some(Self::Navigation),
            (&Method::GET, "source") => Some(Self::Source),
            (&Method::POST, "index") => Some(Self::Index),
            (&Method::GET, "jobs/current") => Some(Self::CurrentJob),
            _ => {
                let (family, tail) = suffix.split_once('/')?;
                let single_id = !tail.is_empty() && !tail.contains('/');
                match (method, family) {
                    (&Method::GET, "jobs") if single_id => Some(Self::Job),
                    (&Method::POST, "jobs") => {
                        let id = tail.strip_suffix("/cancel")?;
                        (!id.is_empty() && !id.contains('/')).then_some(Self::Job)
                    }
                    _ => None,
                }
            }
        }
    }
}

/// The provisioned listener has one selection boundary for core checkout routes.
/// The direct single-checkout router is a test-support fixture only.
async fn provisioned_core(
    State(browser): State<Arc<ProvisionedBrowser>>,
    Path((root_key, suffix)): Path<(String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    use crate::daemon::registry::SelectionError;
    let Some(route) = BrowserCoreRoute::parse(&method, &suffix) else {
        return missing().into_response();
    };
    let is_git = matches!(
        route,
        BrowserCoreRoute::GitStatus | BrowserCoreRoute::GitDiff
    );
    let git_path = if is_git {
        match git_query(route, &uri) {
            Ok(path) => path,
            Err(error) => return error.into_response(),
        }
    } else {
        None
    };
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
    // Git reads are observational: they neither attach a browser lifetime nor
    // activate an index runtime. The common selected-root fence still applies.
    let answer: Result<(Response, bool), ApiError> = if is_git {
        let result = if identity.verify_readonly().is_err() {
            Err(ApiError::from(anyhow::anyhow!("root_changed")))
        } else {
            git_answer(&identity.root, git_path.as_deref()).await
        };
        let hook = browser.response_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook("after_handler");
        }
        // Sampled outside the registry lock: the flag may read the index (#905).
        let runtime = browser.registry.lock().await.runtime(&root_key);
        let catching_up = runtime.is_some_and(|runtime| runtime.catching_up());
        result.map(|response| (response, catching_up))
    } else {
        let runtime = {
            let mut registry = browser.registry.lock().await;
            match registry.browser_request_at(&identity, Instant::now()) {
                Ok(_) => registry.activate(&root_key).map_err(|error| {
                    if crate::store::transient_storage_contention(&error) {
                        ApiError::from(error)
                    } else {
                        ApiError(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "store_unavailable",
                            "Storage is unavailable",
                        )
                    }
                }),
                Err(SelectionError::CheckoutCapacity) => Err(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "checkout_capacity",
                    "Checkout capacity reached",
                )),
                Err(error) => Err(ApiError::from(anyhow::anyhow!("{}", error.reason()))),
            }
        };
        match runtime {
            Err(error) => Err(error),
            Ok(runtime) => {
                let hook = browser.response_hook.lock().unwrap().clone();
                #[cfg(feature = "test-support")]
                let frozen_now = *browser.metrics_clock.lock().unwrap();
                let result = provisioned_core_answer(
                    &runtime,
                    BrowserCoreRequest {
                        route,
                        method: &method,
                        suffix: &suffix,
                        uri: &uri,
                        if_none_match: headers
                            .get(header::IF_NONE_MATCH)
                            .and_then(|value| value.to_str().ok()),
                        body,
                    },
                    hook.clone(),
                    MetricsHttpControls {
                        limit: browser
                            .metrics_limit
                            .load(std::sync::atomic::Ordering::Relaxed),
                        deadline_ms: browser
                            .metrics_deadline_ms
                            .load(std::sync::atomic::Ordering::Relaxed),
                        test_cancel: browser.metrics_test_cancel.clone(),
                        clock_skew_ms: browser.metrics_clock_skew_ms.clone(),
                        #[cfg(feature = "test-support")]
                        frozen_now,
                    },
                )
                .await;
                if let Some(hook) = hook {
                    hook("after_handler");
                }
                result
            }
        }
    };
    let (mut response, catching_up) = match answer {
        Ok(pair) => pair,
        Err(error) => {
            let runtime = browser.registry.lock().await.runtime(&root_key);
            let catching_up = runtime.map_or(!is_git, |runtime| runtime.catching_up());
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

struct MetricsHttpControls {
    limit: usize,
    deadline_ms: u64,
    test_cancel: Arc<std::sync::atomic::AtomicBool>,
    clock_skew_ms: Arc<std::sync::atomic::AtomicU64>,
    #[cfg(feature = "test-support")]
    frozen_now: Option<Instant>,
}

impl MetricsHttpControls {
    fn now(&self) -> Instant {
        #[cfg(feature = "test-support")]
        if let Some(now) = self.frozen_now {
            return now;
        }
        Instant::now()
    }
}

struct BrowserCoreRequest<'a> {
    route: BrowserCoreRoute,
    method: &'a axum::http::Method,
    suffix: &'a str,
    uri: &'a axum::http::Uri,
    /// The browser's `If-None-Match`, honoured by `/status/head` only (#666).
    if_none_match: Option<&'a str>,
    body: Bytes,
}

/// The head digest with a strong `ETag` over its exact bytes (#666). A poll whose
/// `If-None-Match` names the current digest gets `304 Not Modified` and no body.
fn status_head_response(digest: &Value, if_none_match: Option<&str>) -> Response {
    let bytes = serde_json::to_vec(digest).expect("head digest serializes");
    let etag = format!("\"{}\"", &hex::encode(Sha256::digest(&bytes))[..32]);
    let unchanged = if_none_match.is_some_and(|tags| {
        tags.split(',')
            .map(|tag| tag.trim().trim_start_matches("W/"))
            .any(|tag| tag == etag)
    });
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (
            [(header::CONTENT_TYPE, "application/json")],
            Bytes::from(bytes),
        )
            .into_response()
    };
    response.headers_mut().insert(
        header::ETAG,
        etag.parse().expect("hex ETag is a header value"),
    );
    response
}

/// What every provisioned core route family answers: the response and whether the
/// checkout is still catching up.
type CoreAnswer = Result<(Response, bool), ApiError>;

async fn provisioned_core_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    request: BrowserCoreRequest<'_>,
    hook: Option<SelectedResponseHook>,
    metrics: MetricsHttpControls,
) -> CoreAnswer {
    let BrowserCoreRequest {
        route,
        method,
        suffix,
        uri,
        if_none_match,
        body,
    } = request;
    match route {
        BrowserCoreRoute::GitStatus | BrowserCoreRoute::GitDiff => {
            unreachable!("Git reads bypass index activation")
        }
        BrowserCoreRoute::Metrics
        | BrowserCoreRoute::Architecture
        | BrowserCoreRoute::ArchitectureDetails => {
            metrics_family_answer(runtime, route, uri, hook, metrics).await
        }
        BrowserCoreRoute::RulesInventory
        | BrowserCoreRoute::RulesPreview
        | BrowserCoreRoute::RulesApply => {
            rules_answer(runtime, route, uri, body, hook, metrics).await
        }
        BrowserCoreRoute::RefreshDependencies => {
            refresh_dependencies_answer(runtime, body, hook).await
        }
        BrowserCoreRoute::Dependencies => dependencies_answer(runtime, uri, hook).await,
        BrowserCoreRoute::DependencySymbols => dependency_symbols_answer(runtime, uri, hook).await,
        BrowserCoreRoute::DependencySource => dependency_source_answer(runtime, uri, hook).await,
        BrowserCoreRoute::RustSources => rust_sources_answer(runtime, uri, hook).await,
        BrowserCoreRoute::RustSourceTree => rust_source_tree_answer(runtime, uri, hook).await,
        BrowserCoreRoute::RustSourceFile => rust_source_file_answer(runtime, uri, hook).await,
        BrowserCoreRoute::Status | BrowserCoreRoute::StatusHead => {
            status_answer(runtime, route, uri, if_none_match, hook).await
        }
        BrowserCoreRoute::Tree => tree_answer(runtime, uri, hook).await,
        BrowserCoreRoute::Files => files_answer(runtime, uri, hook).await,
        BrowserCoreRoute::FileSymbols => file_symbols_answer(runtime, uri, hook).await,
        BrowserCoreRoute::SyntaxMatches => syntax_matches_answer(runtime, body, hook).await,
        BrowserCoreRoute::Methods => methods_answer(runtime, uri, hook).await,
        BrowserCoreRoute::Classes => classes_answer(runtime, uri, hook).await,
        BrowserCoreRoute::Sequence => sequence_answer(runtime, body, hook).await,
        BrowserCoreRoute::ClassDiagram => class_diagram_answer(runtime, body, hook).await,
        BrowserCoreRoute::Navigation => navigation_answer(runtime, body, hook).await,
        BrowserCoreRoute::Source => source_answer(runtime, uri).await,
        BrowserCoreRoute::Index | BrowserCoreRoute::CurrentJob => {
            index_answer(runtime, method, body).await
        }
        BrowserCoreRoute::Job => job_answer(runtime, method, suffix).await,
    }
}

/// `metrics`, `architecture` and `architecture/details`: one bounded, cancellable worker.
async fn metrics_family_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    route: BrowserCoreRoute,
    uri: &axum::http::Uri,
    hook: Option<SelectedResponseHook>,
    metrics: MetricsHttpControls,
) -> CoreAnswer {
    let architecture = matches!(route, BrowserCoreRoute::Architecture);
    let details = if matches!(route, BrowserCoreRoute::ArchitectureDetails) {
        Some(architecture_details_query(uri)?)
    } else {
        None
    };
    let detail_read = details.is_some();
    let options = if detail_read {
        // A detail page evaluates the whole revision, like the summary.
        crate::metrics_read::MetricsOptions::default()
    } else if architecture {
        architecture_query(uri)?
    } else {
        metrics_query(uri)?
    };
    let runtime = runtime.clone();
    // The guard belongs to this request future, not to the blocking worker.
    struct CancelOnDrop(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    let guard = CancelOnDrop(Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let cancellation = guard.0.clone();
    let result = tokio::task::spawn_blocking(move || {
        let duration = if metrics.deadline_ms != 0 {
            Duration::from_millis(metrics.deadline_ms)
        } else {
            crate::metrics_read::MetricsOptions::work_budget(&options.scope)
        };
        let deadline = metrics.now() + duration;
        let work_hook = hook.clone();
        let finish_hook = hook.clone();
        let serialize_hook = hook.clone();
        let after_finish_hook = hook.clone();
        let check_hook = hook.clone();
        let mut before_work = || {
            if let Some(hook) = &work_hook {
                hook("before_work");
            }
        };
        let mut before_finish = || {
            if let Some(hook) = &finish_hook {
                hook("before_read_finish");
            }
        };
        let mut after_finish = || {
            if let Some(hook) = &after_finish_hook {
                hook("after_read_finish");
            }
        };
        let mut on_progress = || {
            if let Some(hook) = &hook {
                #[cfg(feature = "test-support")]
                if crate::metrics::in_parser_progress_for_tests() {
                    hook("metrics_parser_progress");
                    return;
                }
                hook("metrics_read_progress");
            }
        };
        let mut on_write = |written: usize| {
            if let Some(hook) = &serialize_hook {
                hook(if written >= metrics.limit.saturating_sub(80) {
                    "metrics_serialize_last"
                } else {
                    "metrics_serialize"
                });
            }
        };
        let mut check = || {
            if cancellation.load(std::sync::atomic::Ordering::Acquire)
                || metrics
                    .test_cancel
                    .load(std::sync::atomic::Ordering::Relaxed)
            {
                if let Some(hook) = &check_hook {
                    hook("metrics_cancelled");
                }
                anyhow::bail!("cancelled");
            }
            if metrics.now()
                + Duration::from_millis(
                    metrics
                        .clock_skew_ms
                        .load(std::sync::atomic::Ordering::Relaxed),
                )
                >= deadline
            {
                anyhow::bail!("deadline_exceeded");
            }
            Ok(())
        };
        let hooks = crate::daemon::registry::MetricsResponseHooks {
            before_work: &mut before_work,
            before_finish: &mut before_finish,
            after_finish: &mut after_finish,
            on_write: &mut on_write,
            on_progress: &mut on_progress,
        };
        let result = if let Some(details) = &details {
            runtime.architecture_details_response(details, &mut check, hooks, metrics.limit)
        } else if architecture {
            runtime.architecture_response(&options, &mut check, hooks, metrics.limit)
        } else {
            runtime.metrics_response(&options, &mut check, hooks, metrics.limit)
        };
        if let Some(hook) = &hook {
            hook("metrics_worker_done");
        }
        result
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?;
    drop(guard);
    let (body, catching_up) = match result {
        Ok(answer) => answer,
        Err(error) => {
            if let Some(diagnostic) = crate::rules::diagnostic(&error) {
                return Ok((
                    (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({"error": diagnostic})),
                    )
                        .into_response(),
                    false,
                ));
            }
            if let Some(building) = error.downcast_ref::<architecture::ArchitectureBuilding>() {
                if detail_read {
                    return Err(ApiError(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "architecture_building",
                        "Architecture is being computed for this revision; retry shortly",
                    ));
                }
                return Ok((
                    (
                        StatusCode::ACCEPTED,
                        Json(json!({"payload": building.payload(), "catchingUp": false})),
                    )
                        .into_response(),
                    false,
                ));
            }
            // Only detail reads add these typed refusals; other routes keep their mapping.
            for cause in error.chain().filter(|_| detail_read) {
                let text = cause.to_string();
                if text.starts_with("rules_conflict:") {
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        "rules_conflict",
                        "Rules changed on disk; reload the Architecture view",
                    ));
                }
                if text.starts_with("invalid_request:") {
                    return Err(invalid());
                }
                if text.starts_with("not_found: architecture edge") {
                    return Err(ApiError(
                        StatusCode::NOT_FOUND,
                        "not_found",
                        "No such Architecture edge at this pin and rules file",
                    ));
                }
            }
            return Err(ApiError::from(error));
        }
    };
    Ok((
        (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response(),
        catching_up,
    ))
}

/// `architecture/rules` (inventory), `architecture/rules/preview` and `architecture/rules/apply`.
async fn rules_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    route: BrowserCoreRoute,
    uri: &axum::http::Uri,
    body: Bytes,
    hook: Option<SelectedResponseHook>,
    metrics: MetricsHttpControls,
) -> CoreAnswer {
    let apply = matches!(route, BrowserCoreRoute::RulesApply);
    let inventory = matches!(route, BrowserCoreRoute::RulesInventory);
    let pin = if inventory {
        let options = architecture_query(uri)?;
        if uri.query().is_some_and(|query| {
            query.split('&').any(|pair| {
                !matches!(
                    pair.split_once('=').map(|(name, _)| name),
                    Some("indexGeneration" | "indexRevision")
                )
            })
        }) {
            return Err(invalid());
        }
        Some(options.pin.ok_or_else(invalid)?)
    } else {
        if uri.query().is_some() {
            return Err(invalid());
        }
        None
    };
    let request = if inventory {
        None
    } else {
        let request: crate::rules_edit::Request =
            serde_json::from_slice(&body).map_err(|_| invalid())?;
        request.validate(apply).map_err(|_| invalid())?;
        Some(request)
    };
    struct CancelOnDrop(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    let guard = CancelOnDrop(Arc::new(std::sync::atomic::AtomicBool::new(false)));
    let cancellation = guard.0.clone();
    let runtime = runtime.clone();
    let result = tokio::task::spawn_blocking(move || {
        let duration = if metrics.deadline_ms != 0 {
            Duration::from_millis(metrics.deadline_ms)
        } else {
            Duration::from_secs(300)
        };
        let deadline = metrics.now() + duration;
        let mut check = || {
            anyhow::ensure!(
                !cancellation.load(std::sync::atomic::Ordering::Acquire)
                    && !metrics
                        .test_cancel
                        .load(std::sync::atomic::Ordering::Relaxed),
                "cancelled"
            );
            anyhow::ensure!(
                metrics.now()
                    + Duration::from_millis(
                        metrics
                            .clock_skew_ms
                            .load(std::sync::atomic::Ordering::Relaxed)
                    )
                    < deadline,
                "deadline_exceeded"
            );
            Ok(())
        };
        let result = if let Some(request) = request {
            if let Some(hook) = &hook {
                hook("before_work");
            }
            runtime.rules_edit_response(&request, apply, &mut check, metrics.limit, &mut || {
                if let Some(hook) = &hook {
                    hook("rules_before_rename");
                }
            })
        } else {
            let (response, catching_up) = runtime.evidence_response()?;
            if let Some(hook) = &hook {
                hook("before_work");
            }
            check()?;
            let payload = match response.rules_edit_inventory(pin.expect("inventory pin")) {
                Ok(payload) => payload,
                Err(error) => {
                    if let Some(diagnostic) = crate::rules::diagnostic(&error) {
                        crate::metrics::serialize_checked(
                            &json!({"error":diagnostic}),
                            metrics.limit,
                            &mut check,
                        )?;
                    }
                    check()?;
                    response.finish(())?;
                    return Err(error);
                }
            };
            let bytes = crate::metrics::serialize_checked(
                &json!({"payload":payload,"catchingUp":catching_up}),
                metrics.limit,
                &mut check,
            )?;
            if let Some(hook) = &hook {
                hook("before_read_finish");
            }
            check()?;
            response.finish(())?;
            Ok((bytes, catching_up, None))
        };
        if let Some(hook) = &hook {
            hook("metrics_worker_done");
        }
        result
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?;
    drop(guard);
    match result {
        Ok((bytes, catching_up, outcome)) => {
            let mut response =
                ([(header::CONTENT_TYPE, "application/json")], bytes).into_response();
            if let Some(outcome) = outcome {
                response.headers_mut().insert(
                    "X-Trellis-Mutation-Outcome",
                    outcome.parse().expect("static outcome"),
                );
            }
            Ok((response, catching_up))
        }
        Err(error) => {
            if let Some(diagnostic) = crate::rules::diagnostic(&error) {
                return Ok((
                    (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({"error":diagnostic})),
                    )
                        .into_response(),
                    false,
                ));
            }
            for cause in error.chain() {
                let text = cause.to_string();
                if text.starts_with("invalid_request:") {
                    let message =
                        if text.contains("positively witnessed production layer deviation") {
                            "Selected pair is not a witnessed production layer deviation"
                        } else if text.contains("expanded rules exceed 2 MiB") {
                            "Expanded rules exceed the 2 MiB rules-file limit"
                        } else if text.contains("assignment requires an indexed unassigned file") {
                            "Assignment requires an indexed unassigned file"
                        } else {
                            "Rules change is invalid; check the operation, selections and limits"
                        };
                    return Err(ApiError(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        message,
                    ));
                }
                if text.starts_with("rules_conflict:") {
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        "rules_conflict",
                        "Rules changed on disk; reload rules and preview again",
                    ));
                }
            }
            Err(ApiError::from(error))
        }
    }
}

/// `dependencies/refresh`. A strict sequence (Decision 0009 §6): it moves as one unit.
async fn refresh_dependencies_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    body: Bytes,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    let request: Value = serde_json::from_slice(&body).map_err(|_| invalid())?;
    if !request.as_object().is_some_and(|object| object.is_empty()) {
        return Err(invalid());
    }
    let state = runtime.browser_scheduler()?;
    // One configuration version for the whole request (Decision 0009 §6).
    let config = state.config();
    let (revision, catching_up) = selected_refresh_preflight(runtime, hook).await?;
    if config.dependency_options.is_none() {
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
    selected_refresh_postflight(runtime, revision).await?;
    runtime.require_mutation_ready()?;
    // Scheduled only for the configuration version this request
    // checked; a registration in between refuses it.
    if !state.start_dependency_index_for(&config) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "configuration_changed",
            "The checkout configuration changed; retry the refresh",
        ));
    }
    let mut response = match selected_refresh_postflight(runtime, revision).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"state":"loading"}))).into_response(),
        Err(error) => selected_mutation_error(error, "committed"),
    };
    response
        .headers_mut()
        .insert("X-Trellis-Mutation-Outcome", "committed".parse().unwrap());
    Ok((response, catching_up))
}

/// `dependencies`: the dependency catalog summary.
async fn dependencies_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    uri: &axum::http::Uri,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    if uri.query().is_some() {
        return Err(invalid());
    }
    let state = runtime.browser_scheduler()?;
    let (payload, catching_up) = selected_catalog_read(runtime, hook, move |r| {
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

/// `dependencies/symbols`.
async fn dependency_symbols_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    uri: &axum::http::Uri,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    let Query(q) = Query::<DependencySymbolsQuery>::try_from_uri(uri).map_err(|_| invalid())?;
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
    let (payload, catching_up) = selected_catalog_read(runtime, hook, move |r| {
        let revision = r.status().map_err(ApiError::from)?.revision;
        let catalog = selected_catalog(&state, revision, &q.catalog_id)?;
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
        let payload = json!({"catalogId":catalog.id,"workspaceRevision":catalog.workspace_revision,
            "items":items,"nextOffset":next_offset});
        if !state
            .catalog_snapshot(revision)
            .is_some_and(|current| Arc::ptr_eq(&current, &catalog))
        {
            return Err(dependency_stale());
        }
        Ok(payload)
    })
    .await?;
    Ok((Json(payload).into_response(), catching_up))
}

/// `dependencies/source`.
async fn dependency_source_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    uri: &axum::http::Uri,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    let Query(q) = Query::<DependencySourceQuery>::try_from_uri(uri).map_err(|_| invalid())?;
    if q.catalog_id.is_empty()
        || q.catalog_id.len() > 8192
        || q.source_ref.is_empty()
        || q.source_ref.len() > 8192
    {
        return Err(invalid());
    }
    let state = runtime.browser_scheduler()?;
    let (payload, catching_up) = selected_catalog_read(runtime, hook, move |r| {
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

/// `rust-sources`: the configured Rust source roots.
async fn rust_sources_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    uri: &axum::http::Uri,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    if uri.query().is_some() {
        return Err(invalid());
    }
    let state = runtime.browser_scheduler()?;
    // One configuration version for the whole request (Decision 0009 §6).
    let config = state.config();
    let (payload, catching_up) = selected_catalog_read(runtime, hook, move |_| {
        Ok(
            json!({"roots":config.rust_sources.iter().map(|r| json!({"id":r.label,"label":r.label,
            "path":r.directory.root.to_string_lossy()})).collect::<Vec<_>>() }),
        )
    })
    .await?;
    Ok((Json(payload).into_response(), catching_up))
}

/// `rust-sources/tree`.
async fn rust_source_tree_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    uri: &axum::http::Uri,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    let Query(q) = Query::<RustTreeQuery>::try_from_uri(uri).map_err(|_| invalid())?;
    if !crate::file_tree::valid_path(&q.path)
        || !(1..=200).contains(&q.limit)
        || q.offset > crate::file_tree::SCAN_LIMIT
    {
        return Err(browse_invalid());
    }
    let state = runtime.browser_scheduler()?;
    // One configuration version for the whole request (Decision 0009 §6).
    let config = state.config();
    let (page, catching_up) = selected_catalog_read(runtime, hook, move |r| {
        let revision = r.status().map_err(ApiError::from)?.revision;
        let root = config
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

/// `rust-sources/file`.
async fn rust_source_file_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    uri: &axum::http::Uri,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    let Query(q) = Query::<RustFileQuery>::try_from_uri(uri).map_err(|_| invalid())?;
    if q.path.is_empty() || !crate::file_tree::valid_path(&q.path) || !q.path.ends_with(".rs") {
        return Err(browse_invalid());
    }
    let state = runtime.browser_scheduler()?;
    // One configuration version for the whole request (Decision 0009 §6).
    let config = state.config();
    let (file, catching_up) = selected_catalog_read(runtime, hook, move |_| {
        let root = config
            .rust_sources
            .iter()
            .find(|root| root.label == q.root)
            .ok_or_else(missing)?;
        root.snapshot(&q.path).map_err(rust_source_error)
    })
    .await?;
    Ok((Json(file).into_response(), catching_up))
}

/// `status` and `status/head`.
async fn status_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    route: BrowserCoreRoute,
    uri: &axum::http::Uri,
    if_none_match: Option<&str>,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    if uri.query().is_some() {
        return Err(invalid());
    }
    let head = matches!(route, BrowserCoreRoute::StatusHead);
    let search = runtime.search_availability();
    // A status poll is an open viewer of this checkout (#818); its answer carries the
    // latest agent view request, so a viewer follows it.
    runtime.view_requests().note_viewer(Instant::now());
    let view_request = runtime.view_requests().status_json();
    let runtime = runtime.clone();
    let (status, catching_up) = tokio::task::spawn_blocking(move || {
        let (response, catching_up) = runtime.evidence_response()?;
        #[cfg(feature = "test-support")]
        let projected = crate::store::status_semantic_projections_on_thread_for_tests();
        // #666: the head digest needs no per-language semantic projection.
        let mut status = if head {
            response.status_head()?
        } else {
            response.status()?
        };
        // Status semantic projections run by the browser's status reads
        // themselves, counted on their own blocking thread: the dependency
        // catalog build and other background readers never count (#700).
        #[cfg(feature = "test-support")]
        runtime.count_browser_status_projections(
            crate::store::status_semantic_projections_on_thread_for_tests() - projected,
        );
        let catching_up = catching_up || response.semantic_catching_up()?;
        status.catching_up |= catching_up;
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
    // The head digest (#527) is the fixed-size part of the same status read that
    // the browser polls; diagnostics and statistics stay on `status`.
    let body = if head {
        let digest = json!({
            "workspaceRoot": status.workspace_root,
            "revision": status.revision,
            "catchingUp": status.catching_up,
            "indexedAt": status.indexed_at,
            "viewRequest": view_request,
        });
        status_head_response(&digest, if_none_match)
    } else {
        let mut body = serde_json::to_value(status).map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Operation failed",
            )
        })?;
        body["searchIndex"] = serde_json::to_value(search).map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Operation failed",
            )
        })?;
        body["viewRequest"] = view_request;
        Json(body).into_response()
    };
    Ok((body, catching_up))
}

/// `tree`: one page of the browsed directory.
async fn tree_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    uri: &axum::http::Uri,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    let Query(q) = Query::<TreeQuery>::try_from_uri(uri).map_err(|_| browse_invalid())?;
    if !crate::file_tree::valid_path(&q.path)
        || !(1..=200).contains(&q.limit)
        || q.offset > crate::file_tree::SCAN_LIMIT
    {
        return Err(browse_invalid());
    }
    let state = runtime.browser_scheduler()?;
    // One configuration version for the whole request (Decision 0009 §6).
    let config = state.config();
    let (page, catching_up) = selected_evidence(runtime, hook, move |response| {
        let (mut items, next_offset, truncated) = config
            .browser
            .list(&q.path, q.offset, q.limit)
            .map_err(BrowseDirectoryError)?;
        let (revision, indexed_workspace) =
            response.tree_metadata(&config.browser.root, &mut items)?;
        for item in items
            .iter_mut()
            .filter(|item| item.kind == "file" && item.indexed_path.is_none())
        {
            let absolute = config.browser.root.join(&item.path);
            let reason = if !absolute.starts_with(&indexed_workspace) {
                "Outside indexed workspace"
            } else {
                match crate::capture::language_of_path(&absolute) {
                    Some(_) => "Not indexed yet (may be excluded or size-limited)",
                    None => "Unsupported source type",
                }
            };
            item.unindexed_reason = Some(reason.into());
        }
        Ok(crate::file_tree::Page {
            root: config.browser.root.to_string_lossy().into_owned(),
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

/// `files`.
async fn files_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    uri: &axum::http::Uri,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    let Query(q) = Query::<FilesQuery>::try_from_uri(uri).map_err(|_| browse_invalid())?;
    q.validate()?;
    let pin = q.pin.pin()?;
    let (value, catching_up) = selected_evidence(runtime, hook, move |r| match q.q.as_deref() {
        Some(filter) => r.files_matching_at(pin, filter, q.offset, q.limit),
        None => r.files_at(pin, q.offset, q.limit),
    })
    .await?;
    Ok((Json(value).into_response(), catching_up))
}

/// `symbols`: one file's outline.
async fn file_symbols_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    uri: &axum::http::Uri,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    let Query(q) = Query::<FileSymbolsQuery>::try_from_uri(uri).map_err(|_| invalid())?;
    let pin = q.validate()?;
    let (value, catching_up) = selected_evidence(runtime, hook, move |r| {
        r.file_symbols_at(&q.path, pin, q.offset, q.limit)
    })
    .await?;
    Ok((
        Json(value.ok_or_else(missing)?).into_response(),
        catching_up,
    ))
}

/// `syntax-matches`.
async fn syntax_matches_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    body: Bytes,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    let q: crate::files_view::SyntaxMatchesRequest =
        serde_json::from_slice(&body).map_err(|_| invalid())?;
    q.validate()?;
    let (value, catching_up) =
        selected_evidence(runtime, hook, move |r| r.syntax_matches_at(&q)).await?;
    Ok((
        Json(value.ok_or_else(missing)?).into_response(),
        catching_up,
    ))
}

/// `methods`.
async fn methods_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    uri: &axum::http::Uri,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
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

/// `classes`.
async fn classes_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    uri: &axum::http::Uri,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    let Query(q) = Query::<ClassesQuery>::try_from_uri(uri).map_err(|_| invalid())?;
    let pin = q.pin.pin()?;
    let (value, catching_up) = selected_evidence(runtime, hook, move |r| {
        r.classes_at(q.path.as_deref(), &q.q, pin, q.offset, q.limit)
    })
    .await?;
    Ok((Json(value).into_response(), catching_up))
}

/// `sequence`.
async fn sequence_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    body: Bytes,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    let q: SequenceRequest = serde_json::from_slice(&body).map_err(|_| browse_invalid())?;
    if q.seed.is_empty() || q.seed.len() > 8192 || q.seed.contains('\0') {
        return Err(browse_invalid());
    }
    let hops = runtime.clone();
    let (value, catching_up) = selected_evidence(runtime, hook, move |r| {
        let mut view = if q.interactions && !q.show_all {
            r.sequence_interactions_at(&q.seed, q.expected_revision)?
        } else {
            r.sequence_at(&q.seed, q.expected_revision, q.show_all)?
        };
        // #853: client calls that cross to another app's handler.
        // The Interactions candidates (#663) of an enriched view are not bound by shown
        // steps; every other view keeps that bound when a hop reveals a call.
        if let Some(view) = view.as_mut() {
            let candidates = q.interactions
                && !q.show_all
                && view.semantic.tier == crate::behavior::EvidenceTier::Semantic;
            hops.sequence_interfaces(r, view, !candidates)?;
        }
        Ok(view)
    })
    .await?;
    Ok((
        Json(value.ok_or_else(missing)?).into_response(),
        catching_up,
    ))
}

/// `class-diagram`.
async fn class_diagram_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    body: Bytes,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
    let q: crate::class_diagram::ClassDiagramRequest =
        serde_json::from_slice(&body).map_err(|_| invalid())?;
    q.validate()?;
    let (value, catching_up) =
        selected_evidence(runtime, hook, move |r| r.class_diagram_at(&q)).await?;
    Ok((Json(value).into_response(), catching_up))
}

/// `navigation`.
async fn navigation_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    body: Bytes,
    hook: Option<SelectedResponseHook>,
) -> CoreAnswer {
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

/// `source`. It fires no selected-response hook of its own.
async fn source_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    uri: &axum::http::Uri,
) -> CoreAnswer {
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
        let catching_up = catching_up || response.semantic_catching_up()?;
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

/// `index` (start) and `jobs/current`.
async fn index_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    method: &axum::http::Method,
    body: Bytes,
) -> CoreAnswer {
    use axum::http::Method;
    let state = runtime.browser_scheduler()?;
    let response = if method == Method::POST {
        start_index(State(state), body).await?.into_response()
    } else {
        current_job(State(state)).await?.into_response()
    };
    Ok((response, runtime.catching_up()))
}

/// `jobs/{id}` and `jobs/{id}/cancel`.
async fn job_answer(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    method: &axum::http::Method,
    suffix: &str,
) -> CoreAnswer {
    use axum::http::Method;
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

#[cfg(test)]
mod git_summary_tests;

#[cfg(test)]
mod browser_core_route_tests {
    use super::BrowserCoreRoute;
    use axum::http::Method;

    #[test]
    fn git_checkout_list_refuses_an_oversized_complete_envelope() {
        let row =
            serde_json::json!({"workspaceRoot": "x".repeat(crate::git_status::RESPONSE_LIMIT)});
        let error = super::checkout_list_response(vec![row]).unwrap_err();
        assert_eq!(error.0, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error.1, "response_too_large");
    }

    #[test]
    fn rules_edit_request_refusals_do_not_change_global_error_mapping() {
        let error = super::ApiError::from(anyhow::anyhow!(
            "invalid_request: malformed external destination identity"
        ));
        assert_eq!(error.0, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.1, "internal_error");
    }

    #[test]
    fn dynamic_routes_match_only_their_supported_method_and_shape() {
        for (method, suffix) in [
            (Method::GET, "git"),
            (Method::GET, "git/diff"),
            (Method::GET, "jobs/id"),
            (Method::POST, "jobs/id/cancel"),
        ] {
            assert!(
                BrowserCoreRoute::parse(&method, suffix).is_some(),
                "{method} {suffix}"
            );
        }
    }
}
