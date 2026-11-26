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
/// sent again, to the successor, within the stop wait (60 s; `trellis api`
/// keeps its own 25 s bound). Progress goes to stderr every 10 s. Only an
/// attempt that never wrote (`daemon_unavailable`) is retried inside the wait.
pub(crate) fn send_daemon(
    request: &protocol::Request,
    read_only: bool,
) -> Result<serde_json::Value> {
    let bound = if request.operation == "api" {
        CLI_API_REPLY_BOUND.min(client::STOP_WAIT)
    } else {
        client::STOP_WAIT
    };
    let mut stop_wait: Option<client::StopWait> = None;
    loop {
        let reply = send_daemon_once(request, read_only);
        let stopping = match &reply {
            Ok(reply) => protocol::is_stopping(&reply.payload),
            Err(error) => stop_wait.is_some() && error.to_string() == "daemon_unavailable",
        };
        if stopping
            && stop_wait
                .get_or_insert_with(|| client::StopWait::new(bound))
                .pause(&mut client::stderr_progress)
        {
            continue;
        }
        return checked_reply(reply?);
    }
}

fn send_daemon_once(request: &protocol::Request, read_only: bool) -> Result<protocol::Reply> {
    let socket = socket_path()?;
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
    let reply_bound = if request.operation == "producer_run" {
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
    };
    let mut first = Some(
        client::connect_or_start(&socket, start_daemon, Duration::from_secs(5))
            .map_err(|_| anyhow::anyhow!("daemon_unavailable"))?,
    );
    let reply = client::call(request, read_only, || {
        let stream = if let Some(stream) = first.take() {
            stream
        } else {
            client::connect_or_start(&socket, start_daemon, Duration::from_secs(5))?
        };
        stream.set_read_timeout(Some(reply_bound))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(stream)
    })
    .map_err(|error| anyhow::anyhow!("{}", error.code()))?;
    Ok(reply)
}
