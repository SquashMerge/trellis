//! The CLI's connection to the daemon: the socket path, requests and `send_daemon`.
//! `start_daemon` and the reply bounds stay in `main.rs`.
#[cfg(feature = "test-support")]
use crate::metrics_test_duration;
use crate::{
    CLI_API_REPLY_BOUND, CLI_REPLY_BOUND, INDEX_REPLY_BOUND, cli_first_index_wait,
    metrics_work_budget, start_daemon,
};
use anyhow::{Context, Result};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use trellis::{
    daemon::{self, client, protocol},
    store::topology::TopologyRoots,
};

pub(crate) fn socket_path() -> Result<PathBuf> {
    Ok(daemon::SocketPaths::new(&TopologyRoots::production()?.data).socket)
}

pub(crate) fn daemon_request(operation: &str, payload: serde_json::Value) -> protocol::Request {
    protocol::Request {
        id: 1,
        operation: operation.to_owned(),
        payload,
    }
}

pub(super) fn checked_reply(reply: protocol::Reply) -> Result<serde_json::Value> {
    if let Some(error) = reply.payload.get("error").and_then(|v| v.as_str()) {
        anyhow::bail!("{error}");
    }
    reply
        .payload
        .get("result")
        .cloned()
        .context("invalid daemon reply")
}

/// Send `request`, waiting out an explicit daemon stop (#1293): a
/// `daemon_stopping` answer was given before admission, so even a mutation is
/// sent again, to the successor. One deadline covers the whole wait (60 s;
/// `trellis api` keeps its own 25 s bound for the whole call), and every
/// attempt inside it gets only what is left for its connect and its reply.
/// Progress goes to stderr every 10 s. Only an attempt that never wrote
/// (`daemon_unavailable`) is retried, so an ambiguous mutation is never replayed.
pub(crate) fn send_daemon(
    request: &protocol::Request,
    read_only: bool,
) -> Result<serde_json::Value> {
    let started = Instant::now();
    let api_deadline = (request.operation == "api").then(|| started + reply_bound(request));
    let mut stop_wait: Option<client::StopWait> = None;
    loop {
        let reply = send_daemon_once(request, read_only, stop_wait.as_ref());
        match (&reply, stop_wait.as_ref()) {
            (Ok(reply), _) if protocol::is_stopping(&reply.payload) => {}
            (Err(error), Some(_)) if error.to_string() == "daemon_unavailable" => {}
            // An attempt the wait's own deadline cut off ends the wait as the stop
            // it was waiting out. An ambiguous mutation keeps `outcome_unknown`.
            (Err(error), Some(wait))
                if wait.expired() && error.to_string() != "outcome_unknown" =>
            {
                anyhow::bail!("{}", protocol::DAEMON_STOPPING_MESSAGE);
            }
            _ => return checked_reply(reply?),
        }
        let wait = stop_wait.get_or_insert_with(|| {
            client::StopWait::until(
                api_deadline.unwrap_or_else(|| Instant::now() + stop_wait_bound()),
            )
        });
        if !wait.pause(&mut client::stderr_progress) {
            return checked_reply(reply?);
        }
    }
}

/// How long a CLI command waits out a stop; a fixture may shorten it.
fn stop_wait_bound() -> Duration {
    #[cfg(feature = "test-support")]
    if let Some(bound) = metrics_test_duration("TRELLIS_TEST_CLI_STOP_WAIT_MS") {
        return bound;
    }
    client::STOP_WAIT
}

/// How long one attempt waits on a silent daemon per socket read.
fn reply_bound(request: &protocol::Request) -> Duration {
    // Index replies only once its request is terminal, which takes as long as
    // the checkout needs (#179). Its bound is a liveness backstop: a checkout
    // Trellis cannot index within it is a performance defect to fix. On
    // expiry the requests.db row stays the answer.
    let index_bound = INDEX_REPLY_BOUND;
    #[cfg(feature = "test-support")]
    let index_bound =
        metrics_test_duration("TRELLIS_TEST_INDEX_REPLY_BOUND_MS").unwrap_or(index_bound);
    let cli_bound = CLI_REPLY_BOUND;
    #[cfg(feature = "test-support")]
    let cli_bound = metrics_test_duration("TRELLIS_TEST_CLI_REPLY_BOUND_MS").unwrap_or(cli_bound);
    // The settled wait's own bound is the hang backstop.
    #[cfg(feature = "test-support")]
    let cli_bound = if request.operation == "settled_for_tests" {
        trellis::testing::HANG_BACKSTOP + Duration::from_secs(30)
    } else {
        cli_bound
    };
    if request.operation == "producer_run" {
        // The run's own timeout, plus the termination grace and the import's wait.
        let timeout_secs = request.payload["args"]["limits"]["timeoutSecs"]
            .as_u64()
            .unwrap_or(1800)
            .min(14400);
        Duration::from_secs(timeout_secs + 630)
    } else if matches!(
        request.operation.as_str(),
        "index" | "semantic_import" | "producer_pickup" | "gc_run"
    ) {
        index_bound
    } else if request.operation == "api" {
        cli_bound.min(CLI_API_REPLY_BOUND)
    } else {
        cli_bound
    }
}

/// One attempt. Inside a stop wait its connect and every reply read are
/// bounded by what is left of the wait.
fn send_daemon_once(
    request: &protocol::Request,
    read_only: bool,
    stop_wait: Option<&client::StopWait>,
) -> Result<protocol::Reply> {
    let socket = socket_path()?;
    let budget = stop_wait.map(client::StopWait::remaining);
    // Read afresh at every connect, so a retry inside the attempt gets only what is left.
    let within = |bound: Duration| stop_wait.map_or(bound, |wait| bound.min(wait.remaining()));
    if request.operation == "metrics" {
        let started = Instant::now();
        let ingress_window = Duration::from_secs(20);
        #[cfg(feature = "test-support")]
        let ingress_window = metrics_test_duration("TRELLIS_TEST_METRICS_CLIENT_INGRESS_MS")
            .unwrap_or(ingress_window);
        let ingress = started + ingress_window;
        let work = metrics_work_budget(request);
        #[cfg(feature = "test-support")]
        let work = metrics_test_duration("TRELLIS_TEST_METRICS_CLIENT_WORK_MS").unwrap_or(work);
        // The daemon may first wait for the checkout's first head (#842).
        let work = work + cli_first_index_wait();
        let error_window = Duration::from_millis(500);
        #[cfg(feature = "test-support")]
        let error_window =
            metrics_test_duration("TRELLIS_TEST_METRICS_CLIENT_ERROR_MS").unwrap_or(error_window);
        let transfer_window = Duration::from_secs(30);
        #[cfg(feature = "test-support")]
        let transfer_window = metrics_test_duration("TRELLIS_TEST_METRICS_CLIENT_TRANSFER_MS")
            .unwrap_or(transfer_window);
        let end = ingress + work + error_window + transfer_window;
        let (ingress, end) = match budget {
            Some(left) => (ingress.min(started + left), end.min(started + left)),
            None => (ingress, end),
        };
        let reply = client::call_metrics(request, ingress, end, |timeout| {
            client::connect_or_start_metrics(
                || std::os::unix::net::UnixStream::connect(&socket),
                start_daemon,
                timeout,
            )
        })
        .map_err(|error| anyhow::anyhow!("{}", error.code()))?;
        return Ok(reply);
    }
    let reply_bound = reply_bound(request);
    let connect_bound = || within(Duration::from_secs(5));
    let mut first = Some(
        client::connect_or_start(&socket, start_daemon, connect_bound())
            .map_err(|_| anyhow::anyhow!("daemon_unavailable"))?,
    );
    let reply = client::call(request, read_only, || {
        let stream = if let Some(stream) = first.take() {
            stream
        } else {
            client::connect_or_start(&socket, start_daemon, connect_bound())?
        };
        stream.set_read_timeout(Some(within(reply_bound)))?;
        stream.set_write_timeout(Some(connect_bound()))?;
        Ok(stream)
    })
    .map_err(|error| anyhow::anyhow!("{}", error.code()))?;
    Ok(reply)
}
