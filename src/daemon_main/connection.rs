//! One daemon connection: dispatch, admission, the MCP hand-off and the selected reads.
//! `run_daemon` stays in `main.rs`.
#[cfg(feature = "test-support")]
use super::metrics_request::{metrics_test_duration_file, metrics_test_phase};
use super::{
    io::{
        read_daemon_request, result_is_deadline, wait_for_serve_disconnect, write_daemon_reply,
        write_daemon_reply_until,
    },
    metrics_request::metrics_options_from_request,
    operation::{Marker, Operation, ReadBudget, ReadKind, ReadPolicy, ReplyTransfer, Route},
};
#[cfg(feature = "test-support")]
use crate::metrics_test_duration;
use crate::{
    CLI_FIRST_INDEX_PROGRESS, CLI_READ_WAIT, DaemonControl, DaemonStop, checked_serve_options,
    cli_first_index_wait, metrics_work_budget,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use trellis::{
    daemon::{self, protocol, registry},
    indexer::IndexOptions,
    mcp,
    model::ViewQuery,
    store::topology::{TopologyRoots, WorkspaceIdentity},
};

/// Scoped to one blocking control handler, including registration/reply errors.
pub(crate) struct ServeRegistration {
    registry: Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
    runtime: tokio::runtime::Handle,
    session: u64,
}

impl ServeRegistration {
    pub(crate) fn new(
        registry: Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
        runtime: tokio::runtime::Handle,
        session: u64,
    ) -> Self {
        runtime.block_on(async { registry.lock().await.retain_serve(session) });
        Self {
            registry,
            runtime,
            session,
        }
    }
}

impl Drop for ServeRegistration {
    fn drop(&mut self) {
        self.runtime.block_on(async {
            self.registry.lock().await.disconnect(self.session);
        });
        #[cfg(feature = "test-support")]
        if let Some(path) = std::env::var_os("TRELLIS_TEST_SERVE_RELEASE_SOCKET") {
            use std::io::Write;
            if let Ok(mut hook) = std::os::unix::net::UnixStream::connect(path) {
                let _ = hook.write_all(b"R");
            }
        }
    }
}

pub(crate) fn dispatch_connection(
    mut stream: std::os::unix::net::UnixStream,
    session: u64,
    registry: Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
    browser: Arc<tokio::sync::Mutex<daemon::BrowserProvisioner>>,
    search: Arc<daemon::search::SearchService>,
    control: DaemonControl,
    accepted_at: Instant,
) -> Result<()> {
    let DaemonControl {
        browser_shutdown,
        stop_sender,
    } = control;
    let runtime = tokio::runtime::Handle::current();
    let (request, operation) = admit_request(&mut stream, &browser_shutdown, accepted_at)?;
    match operation.route() {
        Route::Stop => hand_off_stop(stream, session, &request, &stop_sender),
        Route::Mcp => serve_mcp_connection(
            stream,
            session,
            &request,
            &registry,
            &browser_shutdown,
            &runtime,
        ),
        Route::Api => {
            // `trellis api` (#817): one allowlisted browser route, answered by its own handler.
            let result = cli_api_result(&request, &registry, &runtime);
            write_daemon_reply(&mut stream, &result_reply(request.id, result))
        }
        Route::GcReport => {
            let result = search.gc_report();
            write_daemon_reply(&mut stream, &result_reply(request.id, result))
        }
        Route::Read(kind, policy) => read_connection(
            stream,
            &ReadCtx {
                request: &request,
                operation,
                kind,
                policy,
                session,
                registry: &registry,
                browser: &browser,
                browser_shutdown: &browser_shutdown,
                runtime: &runtime,
            },
        ),
    }
}

/// Phase A: reads the request, checks the metrics admission preface against
/// its operation, and acknowledges an admitted metrics request.
fn admit_request(
    stream: &mut std::os::unix::net::UnixStream,
    browser_shutdown: &tokio::sync::watch::Sender<bool>,
    accepted_at: Instant,
) -> Result<(protocol::Request, Operation)> {
    let ingress_deadline = accepted_at + Duration::from_secs(20);
    #[cfg(feature = "test-support")]
    let ingress_deadline = std::env::var("TRELLIS_TEST_METRICS_INGRESS_DEADLINE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|milliseconds| accepted_at + Duration::from_millis(milliseconds))
        .unwrap_or(ingress_deadline);
    let (request, prefaced) = read_daemon_request(
        stream,
        browser_shutdown.subscribe(),
        ingress_deadline,
        #[cfg(test)]
        None,
    )?;
    let operation = Operation::parse(&request.operation);
    if prefaced {
        ensure!(
            operation.requires_preface(),
            "invalid prefaced daemon operation"
        );
        #[cfg(feature = "test-support")]
        if let Some(delay) = metrics_test_duration("TRELLIS_TEST_METRICS_ACK_DELAY_MS") {
            std::thread::sleep(delay);
        }
        protocol::write_frame_until(
            stream,
            &protocol::Reply {
                id: request.id,
                payload: serde_json::json!({"admitted": true}),
            },
            ingress_deadline,
        )?;
        #[cfg(feature = "test-support")]
        metrics_test_phase(b"A");
    } else {
        ensure!(
            !operation.requires_preface(),
            "metrics admission preface required"
        );
    }
    Ok((request, operation))
}

/// Phase B: hands the connection to the daemon's stop sequence.
fn hand_off_stop(
    stream: std::os::unix::net::UnixStream,
    session: u64,
    request: &protocol::Request,
    stop_sender: &tokio::sync::mpsc::UnboundedSender<DaemonStop>,
) -> Result<()> {
    ensure!(
        request.payload == serde_json::json!({}),
        "invalid daemon stop request"
    );
    stop_sender
        .send(DaemonStop {
            session,
            request: request.id,
            stream,
        })
        .map_err(|_| anyhow::anyhow!("daemon_unavailable: daemon already stopping"))?;
    Ok(())
}

/// Phase C: attaches an MCP session, answers, and serves it until it ends.
fn serve_mcp_connection(
    mut stream: std::os::unix::net::UnixStream,
    session: u64,
    request: &protocol::Request,
    registry: &Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
    browser_shutdown: &tokio::sync::watch::Sender<bool>,
    runtime: &tokio::runtime::Handle,
) -> Result<()> {
    let relayed = request
        .payload
        .get("relay")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let result = open_mcp_workspace(request, session, registry, runtime);
    let reply = protocol::Reply {
        id: request.id,
        payload: match &result {
            Ok(_) => {
                serde_json::json!({"result": true, "mcpIdentity": protocol::mcp_identity()})
            }
            Err(error) => serde_json::json!({"error": format!("{error:#}")}),
        },
    };
    let delivered = protocol::write_frame(&mut stream, &reply);
    let served = match (result, delivered) {
        // An idle MCP attachment has no read deadline. Its reader ends at
        // end of input or on the published daemon shutdown (#741).
        (Ok(workspace), Ok(())) => mcp::run_daemon_socket(
            workspace,
            stream,
            relayed,
            browser_shutdown.subscribe(),
            registry.clone(),
            session,
            runtime.clone(),
        )
        .map_err(anyhow::Error::from),
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error.into()),
    };
    runtime.block_on(async { registry.lock().await.disconnect(session) });
    served
}

fn open_mcp_workspace(
    request: &protocol::Request,
    session: u64,
    registry: &Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
    runtime: &tokio::runtime::Handle,
) -> Result<mcp::OpenedWorkspace> {
    protocol::check_mcp_identity(request.payload.get("mcpIdentity"))?;
    let root: PathBuf = serde_json::from_value(
        request
            .payload
            .get("workspace")
            .cloned()
            .context("missing workspace")?,
    )?;
    let identity = WorkspaceIdentity::discover_unattached(Some(&root), &root)?;
    let device: u64 = serde_json::from_value(
        request
            .payload
            .get("device")
            .cloned()
            .context("missing root device")?,
    )?;
    let inode: u64 = serde_json::from_value(
        request
            .payload
            .get("inode")
            .cloned()
            .context("missing root inode")?,
    )?;
    ensure!(
        (identity.device, identity.inode) == (device, inode),
        "root_changed: launch checkout identity changed"
    );
    TopologyRoots::production()?.reject_root_overlap(&identity)?;
    ensure!(identity.root.join(".git").exists(), "not a Git checkout");
    let identity = identity.attach_marker()?;
    let attachments: Vec<protocol::McpAttachment> = request
        .payload
        .get("attachments")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let probe = runtime.block_on(async { registry.lock().await.selection_probe() });
    let mut prepared = Vec::new();
    for attachment in attachments {
        // A failed old attachment cannot block the still-valid launch
        // workspace or attach a new incarnation at its old path.
        let restored = (|| -> Result<_> {
            let captured =
                WorkspaceIdentity::discover_unattached(Some(&attachment.root), &attachment.root)
                    .and_then(WorkspaceIdentity::attach_existing_marker_readonly)?;
            ensure!(attachment.matches(&captured), "selected identity changed");
            let selection = probe
                .prepare(&identity, &attachment.root)
                .map_err(|error| anyhow::anyhow!("{}", error.reason()))?;
            ensure!(
                attachment.matches(selection.identity()),
                "selected identity changed"
            );
            Ok(selection)
        })();
        if let Ok(selection) = restored {
            prepared.push(selection);
        }
    }
    runtime.block_on(async {
        let mut registry = registry.lock().await;
        registry
            .attach_launch(session, &identity)
            .map_err(|error| anyhow::anyhow!("{}", error.reason()))?;
        for selection in prepared {
            // Capacity or an incarnation change can race preparation.
            // Restore the other selections and keep launch usable.
            let _ = registry.select_prepared(session, selection);
        }
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(mcp::OpenedWorkspace::new(identity))
}

/// Phase D: the `trellis api` request, answered by the browser route's handler.
fn cli_api_result(
    request: &protocol::Request,
    registry: &Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
    runtime: &tokio::runtime::Handle,
) -> Result<serde_json::Value> {
    let field = |name: &str| {
        request
            .payload
            .get(name)
            .and_then(serde_json::Value::as_str)
    };
    let method = field("method").context("invalid_request: missing method")?;
    let target = field("target").context("invalid_request: missing target")?;
    let (status, body) = runtime.block_on(trellis::http::cli_api(
        registry.clone(),
        method,
        target,
        field("body"),
    ))?;
    Ok(serde_json::json!({"status": status, "body": body}))
}

fn result_reply<T: Serialize>(id: u64, result: Result<T>) -> protocol::Reply {
    protocol::Reply {
        id,
        payload: match result {
            Ok(value) => serde_json::json!({"result": value}),
            Err(error) => serde_json::json!({"error": format!("{error:#}")}),
        },
    }
}

/// What the phases of a [`Route::Read`] request share.
#[derive(Clone, Copy)]
struct ReadCtx<'a> {
    request: &'a protocol::Request,
    operation: Operation,
    kind: ReadKind,
    policy: ReadPolicy,
    session: u64,
    registry: &'a Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
    browser: &'a Arc<tokio::sync::Mutex<daemon::BrowserProvisioner>>,
    browser_shutdown: &'a tokio::sync::watch::Sender<bool>,
    runtime: &'a tokio::runtime::Handle,
}

/// Phases F to H: budgets the request, runs it, detaches the session and
/// replies. A registered serve then holds the connection.
fn read_connection(mut stream: std::os::unix::net::UnixStream, ctx: &ReadCtx) -> Result<()> {
    let ReadCtx {
        request,
        kind,
        policy,
        session,
        registry,
        browser_shutdown,
        runtime,
        ..
    } = *ctx;
    let serve = kind == ReadKind::Serve;
    let _serve_registration =
        serve.then(|| ServeRegistration::new(registry.clone(), runtime.clone(), session));
    let read_budget = match policy.budget {
        ReadBudget::MetricsWork => metrics_read_budget(request),
        ReadBudget::Fixed(wait) => wait,
    };
    // The read's deadline, as `run_read` finally set it: a wait for the first head
    // (#842) starts a fresh read budget after it.
    let mut answered_deadline = Instant::now() + read_budget;
    // Progress of that wait goes to the client as frames before its reply.
    let mut progress_stream = stream.try_clone().ok();
    let mut progress = |message: String| {
        if let Some(progress) = progress_stream.as_mut() {
            let _ = protocol::write_frame(
                progress,
                &protocol::Reply {
                    id: request.id,
                    payload: serde_json::json!({ "progress": message }),
                },
            );
        }
    };
    let result = run_read(ctx, read_budget, &mut answered_deadline, &mut progress);
    let request_deadline = answered_deadline;
    if !serve {
        runtime.block_on(async { registry.lock().await.disconnect(session) });
    }
    let serve_registered = serve && result.is_ok();
    reply_to_read(&mut stream, ctx, result, request_deadline)?;
    if serve_registered {
        // The scoped registration holds the daemon alive until this control
        // connection closes or the daemon stops, signalled by
        // the shutdown channel instead of a polled flag (#138 item 21).
        wait_for_serve_disconnect(stream, browser_shutdown.subscribe(), runtime)?;
    }
    Ok(())
}

/// The work budget of a metrics read, from its request.
fn metrics_read_budget(request: &protocol::Request) -> Duration {
    let metrics_work = metrics_work_budget(request);
    #[cfg(feature = "test-support")]
    let metrics_work =
        metrics_test_duration("TRELLIS_TEST_METRICS_WORK_DEADLINE_MS").unwrap_or(metrics_work);
    #[cfg(feature = "test-support")]
    let metrics_work = if matches!(
        request.payload["args"]["scope"].as_str(),
        Some("file" | "declaration")
    ) {
        metrics_test_duration_file("TRELLIS_TEST_METRICS_FILE_WORK_DEADLINE_MS_FILE")
            .unwrap_or(metrics_work)
    } else {
        metrics_work
    };
    metrics_work
}

/// Phase G: checks the workspace, then registers a serve or runs the
/// operation on its activated checkout.
fn run_read(
    ctx: &ReadCtx,
    read_budget: Duration,
    answered_deadline: &mut Instant,
    progress: &mut dyn FnMut(String),
) -> Result<serde_json::Value> {
    let ReadCtx {
        request,
        operation,
        kind,
        policy,
        ..
    } = *ctx;
    let request_deadline = *answered_deadline;
    let root: PathBuf = serde_json::from_value(
        request
            .payload
            .get("workspace")
            .cloned()
            .context("missing workspace")?,
    )?;
    let identity = WorkspaceIdentity::discover_unattached(Some(&root), &root)?;
    TopologyRoots::production()?.reject_root_overlap(&identity)?;
    if kind == ReadKind::Serve {
        return register_serve(ctx, identity);
    }
    ensure!(kind == ReadKind::Checkout, "unknown daemon operation");
    let metrics_options = if operation == Operation::Metrics {
        Some(metrics_options_from_request(
            request
                .payload
                .get("args")
                .context("invalid_argument: missing args")?,
        )?)
    } else {
        None
    };
    let identity = match policy.marker {
        Marker::ExistingReadOnly => identity.attach_existing_marker_readonly()?,
        Marker::Publish => identity.attach_marker()?,
    };
    // Virgin queue publication can contend with a short-lived index-use
    // reader before any request ID has been accepted. Retry only that
    // typed pre-acceptance admission; never replay an enqueue after COMMIT.
    let activation_deadline = request_deadline;
    let checkout = in_read_scope(policy.read_scope, activation_deadline, || {
        activate_checkout(ctx, &identity, activation_deadline)
    })?;
    // The activation above is the one MCP and serve use: it starts startup
    // reconciliation, the first index and the watcher. A read with no head
    // to answer from waits for that first head, then gets its own budget.
    let request_deadline =
        if policy.awaits_first_head && await_first_head(&checkout, CLI_READ_WAIT, progress)? {
            Instant::now() + read_budget
        } else {
            request_deadline
        };
    *answered_deadline = request_deadline;
    let args = request.payload.get("args").context("missing args")?;
    // Pauses a read-only request after activation, so a fixture can take
    // index-use contention before the selected read (#687).
    #[cfg(all(feature = "test-support", unix))]
    let mut contention_fixture =
        std::env::var_os("TRELLIS_TEST_CLI_READ_CONTENTION_SOCKET").filter(|_| policy.replayable);
    #[cfg(all(feature = "test-support", unix))]
    if let Some(path) = &contention_fixture {
        use std::io::{Read, Write};
        let mut fixture = std::os::unix::net::UnixStream::connect(path)
            .expect("connect CLI read contention fixture");
        fixture
            .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
            .unwrap();
        fixture
            .write_all(b"R")
            .expect("report CLI read before the selected read");
        fixture
            .read_exact(&mut [0])
            .expect("release CLI read contention fixture");
    }
    let read = SelectedRead {
        ctx,
        checkout: &checkout,
        identity: &identity,
        args,
        metrics_options: metrics_options.as_ref(),
        request_deadline,
    };
    in_read_scope(policy.read_scope, request_deadline, || {
        read.with_retry(
            #[cfg(all(feature = "test-support", unix))]
            &mut contention_fixture,
        )
    })
}

fn register_serve(ctx: &ReadCtx, identity: WorkspaceIdentity) -> Result<serde_json::Value> {
    let ReadCtx {
        request,
        registry,
        browser,
        browser_shutdown,
        runtime,
        ..
    } = *ctx;
    let options = checked_serve_options(
        request
            .payload
            .get("options")
            .cloned()
            .context("missing options")?,
    )?;
    let bind = serde_json::from_value(
        request
            .payload
            .get("bind")
            .cloned()
            .context("missing bind")?,
    )?;
    let token_file: PathBuf = serde_json::from_value(
        request
            .payload
            .get("tokenFile")
            .cloned()
            .context("missing token file")?,
    )?;
    let identity = identity.attach_marker()?;
    let address = runtime.block_on(async {
        let mut browser = browser.lock().await;
        let first = browser.address().is_none();
        let address = browser
            .register_serve(registry, &identity, options, bind, &token_file)
            .await?;
        if first {
            browser.start_owned(browser_shutdown.subscribe())?;
        }
        Ok::<_, anyhow::Error>(address)
    })?;
    Ok(serde_json::json!(address.to_string()))
}

/// Attaches the session and activates its checkout, waiting out typed
/// contention until `activation_deadline`.
fn activate_checkout(
    ctx: &ReadCtx,
    identity: &WorkspaceIdentity,
    activation_deadline: Instant,
) -> Result<Arc<registry::CheckoutRuntime>> {
    let ReadCtx {
        session,
        registry,
        runtime,
        ..
    } = *ctx;
    loop {
        let activated = runtime.block_on(async {
            let mut checked = registry.lock().await;
            checked
                .attach_launch(session, identity)
                .map_err(|e| anyhow::anyhow!("{}", e.reason()))?;
            checked.activate(&identity.root_key)
        });
        match activated {
            Ok(checkout) => break Ok(checkout),
            Err(error) if trellis::store::cli_read_retryable_contention(&error) => {
                if Instant::now() >= activation_deadline {
                    break Err(trellis::store::cli_read_busy_expired());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => break Err(error),
        }
    }
}

/// One selected read or write on an activated checkout.
struct SelectedRead<'a> {
    ctx: &'a ReadCtx<'a>,
    checkout: &'a registry::CheckoutRuntime,
    identity: &'a WorkspaceIdentity,
    args: &'a serde_json::Value,
    metrics_options: Option<&'a trellis::metrics_read::MetricsOptions>,
    request_deadline: Instant,
}

impl SelectedRead<'_> {
    /// Runs the operation, repeating a replayable one while it meets typed
    /// contention or `index_not_ready`, until the deadline.
    fn with_retry(
        &self,
        #[cfg(all(feature = "test-support", unix))] contention_fixture: &mut Option<
            std::ffi::OsString,
        >,
    ) -> Result<serde_json::Value> {
        let checkout = self.checkout;
        let deadline = self.request_deadline;
        let replayable_read = self.ctx.policy.replayable;
        loop {
            // Recorded before the read: a state change between the read and
            // the wait below is never missed.
            let state_version = checkout.state_version();
            let selected = self.once();
            // A peer's exclusive index-use lock (a restarted daemon's startup
            // probe or recovery stage, or another process) refuses the read
            // with typed contention (#687). It publishes no in-process signal,
            // so wait out this bounded external contention like activation
            // above, then report typed expiry. Only read-only calls repeat.
            if replayable_read
                && selected
                    .as_ref()
                    .err()
                    .is_some_and(trellis::store::cli_read_retryable_contention)
            {
                if Instant::now() >= deadline {
                    break Err(trellis::store::cli_read_busy_expired());
                }
                #[cfg(all(feature = "test-support", unix))]
                if let Some(path) = contention_fixture.take() {
                    use std::io::Write;
                    let mut signal = std::os::unix::net::UnixStream::connect(path)
                        .expect("connect CLI read contention fixture");
                    signal
                        .write_all(b"B")
                        .expect("report CLI read contention wait");
                }
                std::thread::sleep(
                    Duration::from_millis(20)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
                continue;
            }
            // Mandatory startup reconciliation may be running when a CLI read first attaches.
            // This only repeats read-only calls; index is never replayed.
            if replayable_read
                && selected
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.to_string().starts_with("index_not_ready:"))
                && Instant::now() < deadline
            {
                #[cfg(all(feature = "test-support", unix))]
                if let Ok(path) = std::env::var("TRELLIS_TEST_CLI_READ_WAIT_SOCKET") {
                    use std::io::Write;
                    let mut signal = std::os::unix::net::UnixStream::connect(path)
                        .expect("connect CLI startup read fixture");
                    signal
                        .write_all(b"W")
                        .expect("report CLI startup read wait");
                }
                // Wait for startup reconciliation to progress, not a poll (#138
                // item 16).
                checkout.wait_state_change(
                    state_version,
                    deadline.saturating_duration_since(Instant::now()),
                );
                continue;
            }
            break selected;
        }
    }

    fn once(&self) -> Result<serde_json::Value> {
        let Self {
            ctx,
            checkout,
            identity,
            args,
            metrics_options,
            request_deadline,
        } = *self;
        match ctx.operation {
            Operation::Status => checkout.cli_status(),
            #[cfg(feature = "test-support")]
            Operation::SettledForTests => {
                let at: Option<trellis::model::IndexPin> =
                    serde_json::from_value(args["at"].clone())
                        .context("invalid_argument: at is not a revision")?;
                let pin = checkout.await_settled_for_tests(at)?;
                Ok(serde_json::json!({ "revision": pin }))
            }
            Operation::ProducerPlan => checkout.cli_producer_plan(args),
            Operation::ProducerStatus => checkout.cli_producer_status(args),
            Operation::ProducerPickup => checkout.cli_producer_pickup(args),
            Operation::ProducerRun => checkout.cli_producer_run(args),
            Operation::ProducerCancel => checkout.cli_producer_cancel(args),
            Operation::Symbols => checkout.cli_symbols(
                args["search"].as_str().context("missing search")?,
                args["limit"].as_u64().context("missing limit")? as usize,
            ),
            Operation::Query => checkout.cli_query(&ViewQuery {
                seed: args["seed"].as_str().context("missing seed")?.into(),
                depth: args["depth"].as_u64().context("missing depth")? as usize,
                max_nodes: args["maxNodes"].as_u64().context("missing maxNodes")? as usize,
                max_calls: args["maxCalls"].as_u64().context("missing maxCalls")? as usize,
                include_callbacks: args["includeCallbacks"]
                    .as_bool()
                    .context("missing includeCallbacks")?,
                exclude_paths: serde_json::from_value(args["excludePaths"].clone())?,
                semantic_producer_id: serde_json::from_value(
                    args.get("semanticProducerId").cloned().unwrap_or_default(),
                )
                .context("invalid_argument: semanticProducerId must be a string")?,
            }),
            Operation::RulesCheck => {
                cli_architecture_read(checkout, "check", args, request_deadline)
            }
            Operation::References => {
                cli_architecture_read(checkout, "references", args, request_deadline)
            }
            Operation::Metrics => {
                let options = metrics_options.expect("metrics options validated");
                cli_metrics_read(checkout, options, request_deadline)
            }
            Operation::Index => cli_index(checkout, identity, args),
            Operation::SemanticImport => {
                let options: trellis::store::requests::SemanticImportOptions =
                    serde_json::from_value(args.clone())
                        .map_err(|_| anyhow::anyhow!("invalid_request: semantic import options"))?;
                checkout.cli_semantic_import(&options)
            }
            _ => anyhow::bail!("unknown daemon operation"),
        }
    }
}

fn cli_architecture_read(
    checkout: &registry::CheckoutRuntime,
    kind: &str,
    args: &serde_json::Value,
    request_deadline: Instant,
) -> Result<serde_json::Value> {
    let mut check = || {
        ensure!(Instant::now() < request_deadline, "deadline_exceeded");
        Ok(())
    };
    // The owner finishes the first read after a publication; leave time to
    // answer. The wait comes before read admission, and again if the head
    // moved meanwhile.
    let (response, catching_up, result) = loop {
        let wait = request_deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(Duration::from_secs(1));
        checkout.await_architecture_tool(kind, args, wait, &mut check);
        let (response, catching_up) = checkout.evidence_response()?;
        let result = checkout.architecture_tool(&response, kind, args, &mut check);
        let building = result.as_ref().err().is_some_and(|error| {
            error
                .downcast_ref::<trellis::http::ArchitectureBuilding>()
                .is_some()
        });
        if !building || wait.is_zero() {
            break (response, catching_up, result);
        }
        drop(response);
        std::thread::sleep(Duration::from_millis(1));
    };
    match result {
        Ok(payload) => {
            check()?;
            let value = serde_json::json!({"catchingUp":catching_up,"payload":payload});
            ensure!(
                serde_json::to_vec(&value)?.len() <= 64 * 1024 * 1024,
                "response_too_large"
            );
            check()?;
            response.finish(value)
        }
        Err(error) => {
            if let Some(diagnostic) = trellis::rules::diagnostic(&error) {
                response.finish(serde_json::json!({"catchingUp":catching_up,"error":diagnostic}))
            } else {
                Err(error)
            }
        }
    }
}

fn cli_metrics_read(
    checkout: &registry::CheckoutRuntime,
    options: &trellis::metrics_read::MetricsOptions,
    request_deadline: Instant,
) -> Result<serde_json::Value> {
    let mut before_work = || {
        #[cfg(feature = "test-support")]
        metrics_test_phase(b"W");
        #[cfg(feature = "test-support")]
        if let Ok(gate) = std::env::var("TRELLIS_TEST_METRICS_WORK_GATE") {
            use std::io::Write;
            let gate = PathBuf::from(gate);
            if let Ok(mut started) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(gate.join("started"))
            {
                let _ = started.write_all(b"x");
            }
            while !gate.join("release").exists() && Instant::now() < request_deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        #[cfg(feature = "test-support")]
        if let Ok(delay) = std::env::var("TRELLIS_TEST_METRICS_WORK_DELAY_MS")
            && let Ok(milliseconds) = delay.parse::<u64>()
        {
            std::thread::sleep(Duration::from_millis(milliseconds).min(
                request_deadline.saturating_duration_since(Instant::now())
                    + Duration::from_millis(100),
            ));
        }
    };
    #[cfg(feature = "test-support")]
    let mut delayed_parse = false;
    let mut on_progress = || {
        #[cfg(feature = "test-support")]
        if !delayed_parse
            && trellis::metrics::in_parser_progress_for_tests()
            && let Ok(delay) = std::env::var("TRELLIS_TEST_METRICS_PARSE_DELAY_MS")
            && let Ok(milliseconds) = delay.parse::<u64>()
        {
            delayed_parse = true;
            if let Ok(path) = std::env::var("TRELLIS_TEST_METRICS_PARSE_HIT_FILE") {
                let _ = std::fs::write(path, b"parse");
            }
            std::thread::sleep(Duration::from_millis(milliseconds).min(
                request_deadline.saturating_duration_since(Instant::now())
                    + Duration::from_millis(100),
            ));
        }
    };
    #[cfg(feature = "test-support")]
    let serialize_threshold = std::env::var("TRELLIS_TEST_METRICS_SERIALIZE_DELAY_AT_BYTES")
        .ok()
        .or_else(|| {
            std::env::var("TRELLIS_TEST_METRICS_SERIALIZE_DELAY_AT_BYTES_FILE")
                .ok()
                .and_then(|path| std::fs::read_to_string(path).ok())
        })
        .and_then(|value| value.trim().parse::<usize>().ok());
    #[cfg(feature = "test-support")]
    let serialize_delay = std::env::var("TRELLIS_TEST_METRICS_SERIALIZE_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    #[cfg(feature = "test-support")]
    let mut delayed_serialize = false;
    let mut on_write = |written: usize| {
        #[cfg(feature = "test-support")]
        if !delayed_serialize
            && serialize_threshold.is_some_and(|threshold| written >= threshold)
            && let Some(milliseconds) = serialize_delay
        {
            delayed_serialize = true;
            if let Ok(path) = std::env::var("TRELLIS_TEST_METRICS_SERIALIZE_HIT_FILE") {
                let _ = std::fs::write(path, b"serialize");
            }
            std::thread::sleep(Duration::from_millis(milliseconds).min(
                request_deadline.saturating_duration_since(Instant::now())
                    + Duration::from_millis(100),
            ));
        }
        let _ = written;
    };
    #[cfg(feature = "test-support")]
    let limit = std::env::var("TRELLIS_TEST_METRICS_LIMIT_FILE")
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(64 * 1024 * 1024)
        .min(64 * 1024 * 1024);
    #[cfg(not(feature = "test-support"))]
    let limit = 64 * 1024 * 1024;
    // Errors become `selected`, so typed contention reaches the
    // shared bounded retry below instead of returning (#687).
    (|| {
        let (body, _) = checkout.metrics_response(
            options,
            &mut || {
                if Instant::now() >= request_deadline {
                    anyhow::bail!("deadline_exceeded");
                }
                Ok(())
            },
            registry::MetricsResponseHooks {
                before_work: &mut before_work,
                before_finish: &mut || {},
                after_finish: &mut || {},
                on_write: &mut on_write,
                on_progress: &mut on_progress,
            },
            limit,
        )?;
        let value = serde_json::from_slice(&body)?;
        if Instant::now() >= request_deadline {
            anyhow::bail!("deadline_exceeded");
        }
        Ok(value)
    })()
}

fn cli_index(
    checkout: &registry::CheckoutRuntime,
    identity: &WorkspaceIdentity,
    args: &serde_json::Value,
) -> Result<serde_json::Value> {
    ensure!(
        args.as_object().is_some_and(|values| values
            .keys()
            .all(|key| matches!(key.as_str(), "workspace" | "maxFileBytes"))),
        "invalid_request: unknown index option"
    );
    let mut options = IndexOptions::new(identity.root.clone());
    if let Some(value) = args.get("maxFileBytes") {
        options.max_file_bytes = value.as_u64().context("invalid maxFileBytes")?;
    }
    ensure!(
        (1..=16_777_216).contains(&options.max_file_bytes),
        "max-file-bytes must be 1..16777216"
    );
    #[cfg(feature = "test-support")]
    if let Some(path) = std::env::var_os("TRELLIS_TEST_DAEMON_INDEX_CLAIM_SOCKET") {
        checkout.set_after_claim_hook_for_tests(move || {
            use std::io::{Read, Write};
            let mut hook = std::os::unix::net::UnixStream::connect(path)
                .expect("connect accepted index fixture");
            hook.set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
                .unwrap();
            hook.write_all(b"C").unwrap();
            hook.read_exact(&mut [0]).unwrap();
        });
    }
    checkout.cli_index(&options)
}

/// Phase H: answers the request.
fn reply_to_read(
    stream: &mut std::os::unix::net::UnixStream,
    ctx: &ReadCtx,
    result: Result<serde_json::Value>,
    request_deadline: Instant,
) -> Result<()> {
    let request = ctx.request;
    let bounded = ctx.policy.reply == ReplyTransfer::DeadlineBounded;
    #[cfg(feature = "test-support")]
    if bounded
        && result.as_ref().err().is_some_and(|error| {
            let message = error.to_string();
            message.contains("deadline_exceeded") || message.contains("cancelled")
        })
        && let Some(delay) = metrics_test_duration_file("TRELLIS_TEST_METRICS_ERROR_DELAY_MS_FILE")
            .or_else(|| metrics_test_duration("TRELLIS_TEST_METRICS_ERROR_DELAY_MS"))
    {
        std::thread::sleep(delay);
    }
    let expired_success = bounded && result.is_ok() && Instant::now() >= request_deadline;
    let reply = protocol::Reply {
        id: request.id,
        payload: match result {
            Ok(_) if expired_success => serde_json::json!({"error": "deadline_exceeded"}),
            Ok(value) => serde_json::json!({"result": value}),
            Err(error) => serde_json::json!({"error": format!("{error:#}")}),
        },
    };
    if bounded {
        let transfer_window = Duration::from_secs(30);
        #[cfg(feature = "test-support")]
        let transfer_window =
            metrics_test_duration_file("TRELLIS_TEST_METRICS_TRANSFER_DEADLINE_MS_FILE")
                .or_else(|| metrics_test_duration("TRELLIS_TEST_METRICS_TRANSFER_DEADLINE_MS"))
                .unwrap_or(transfer_window);
        let transfer_deadline = Instant::now() + transfer_window;
        if Instant::now() >= request_deadline + Duration::from_millis(500)
            && result_is_deadline(&reply.payload)
        {
            return Ok(());
        }
        write_daemon_reply_until(stream, &reply, transfer_deadline)?;
    } else {
        write_daemon_reply(stream, &reply)?;
    }
    Ok(())
}

/// Run `read` under one read deadline (#142) when `scoped`.
fn in_read_scope<T>(scoped: bool, deadline: Instant, read: impl FnOnce() -> T) -> T {
    if scoped {
        trellis::store::with_read_deadline(deadline, read)
    } else {
        read()
    }
}

/// A CLI read of a just-activated checkout with no admitted head waits for the
/// first validated head, on the runtime's state signal, up to
/// `cli_first_index_wait` (#842, owner ruling 2026-11-09). It reports progress
/// to the client meanwhile. A prior valid head answers at once, as MCP does,
/// and any failure other than `index_not_ready` is left to the read to report.
/// With no head, a blocked startup recovery ends the wait with its
/// `recovery_blocked` cause.
/// Returns whether it waited. On expiry, fails `index_not_ready`; the daemon
/// keeps indexing.
fn await_first_head(
    checkout: &registry::CheckoutRuntime,
    probe_budget: Duration,
    progress: &mut dyn FnMut(String),
) -> Result<bool> {
    let started = Instant::now();
    let deadline = started + cli_first_index_wait();
    let mut reported: Option<Instant> = None;
    loop {
        // Recorded before the probe, so a publication after it is never missed.
        let seen = checkout.state_version();
        // An eligible head answers first, whatever startup recovery says
        // (T03, Decision 0009 "Reconciling").
        let probe = trellis::store::with_read_deadline(Instant::now() + probe_budget, || {
            checkout.evidence_response().map(drop)
        });
        if !probe
            .as_ref()
            .is_err_and(|error| error.to_string().starts_with("index_not_ready:"))
        {
            return Ok(reported.is_some());
        }
        // With no head, a blocked startup recovery will publish none: end the
        // wait with its typed cause, the one `status` reports (#768). Checked
        // on every wake.
        if let Some(blocker) = checkout.recovery_blocker() {
            anyhow::bail!("{blocker}");
        }
        #[cfg(all(feature = "test-support", unix))]
        if reported.is_none()
            && let Ok(path) = std::env::var("TRELLIS_TEST_CLI_READ_WAIT_SOCKET")
        {
            use std::io::Write;
            let mut signal = std::os::unix::net::UnixStream::connect(path)
                .expect("connect CLI startup read fixture");
            signal
                .write_all(b"W")
                .expect("report CLI startup read wait");
        }
        let now = Instant::now();
        if now >= deadline {
            anyhow::bail!(
                "index_not_ready: the first index of this checkout is still running; \
                 indexing continues in the daemon, retry shortly"
            );
        }
        let last = match reported {
            Some(at) if now.duration_since(at) < CLI_FIRST_INDEX_PROGRESS => at,
            _ => {
                progress(format!(
                    "trellis: indexing this checkout ({}s elapsed, waiting up to {}s)",
                    now.duration_since(started).as_secs(),
                    deadline.duration_since(started).as_secs()
                ));
                reported = Some(now);
                now
            }
        };
        checkout.wait_state_change(
            seen,
            deadline
                .min(last + CLI_FIRST_INDEX_PROGRESS)
                .saturating_duration_since(now),
        );
    }
}
