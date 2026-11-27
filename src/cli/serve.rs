//! `trellis serve` through the daemon, and stopping an existing daemon.
use super::transport::{checked_reply, daemon_request, socket_path};
use crate::{CLI_REPLY_BOUND, ServeArgs, discover_rust_library, resolve_browse_root, start_daemon};
use anyhow::{Context, Result, ensure};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use trellis::{
    daemon::{client, protocol, registry},
    indexer::IndexOptions,
};

pub(crate) async fn serve_via_daemon(args: &ServeArgs) -> Result<()> {
    // Arm SIGTERM before any readiness banner: the serve control select is
    // polled later, but callers may signal as soon as they see the banner.
    #[cfg(unix)]
    let mut termination = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("register serve SIGTERM handler")?;
    #[cfg(unix)]
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("register serve SIGINT handler")?;
    ensure!(
        args.bind.ip().is_loopback(),
        "only loopback bind addresses are supported"
    );
    let token_file = args
        .token_file
        .as_ref()
        .context("serve requires explicit --token-file")?;
    let (roots, identity) = args.index.workspace.resolve_unattached()?;
    roots.validate_external(&identity, std::slice::from_ref(token_file))?;
    let token_parent = token_file
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let token_file = token_parent.canonicalize()?.join(
        token_file
            .file_name()
            .context("serve token file needs a name")?,
    );
    let mut index_options = IndexOptions::new(identity.root.clone());
    index_options.max_file_bytes = args.index.max_file_bytes.unwrap_or(2_097_152);
    ensure!(
        (1..=16_777_216).contains(&index_options.max_file_bytes),
        "max-file-bytes must be 1..16777216"
    );
    let rust_library = if let Some(path) = args
        .rust_library
        .clone()
        .or_else(|| std::env::var_os("RUST_SRC_PATH").map(PathBuf::from))
    {
        Some(path)
    } else if let Some(rustc) = &args.trusted_rustc {
        Some(discover_rust_library(rustc, &identity.root).await?)
    } else {
        None
    };
    let options = registry::BrowserOptions {
        browse_root: Some(resolve_browse_root(
            &index_options,
            args.browse_root.clone(),
        )),
        max_file_bytes: index_options.max_file_bytes,
        rust_source_roots: args.rust_source_roots.clone(),
        rust_library,
        trusted_rustc: args.trusted_rustc.clone(),
        cargo_home: args
            .cargo_home
            .clone()
            .or_else(|| std::env::var_os("CARGO_HOME").map(PathBuf::from))
            .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().join(".cargo"))),
    };
    let mut options_json = serde_json::to_value(options)?;
    let fields = options_json
        .as_object_mut()
        .expect("browser options object");
    // Only index options actually supplied by this caller can supersede a
    // durable option. Other BrowserOptions fields retain their normal shape.
    if args.index.max_file_bytes.is_none() {
        fields.remove("maxFileBytes");
    }
    let socket = socket_path()?;
    let registration = daemon_request(
        "serve",
        serde_json::json!({
            "workspace": identity.root, "bind": args.bind, "tokenFile": token_file,
            "options": options_json,
        }),
    );
    // A stopping daemon answers `daemon_stopping` before admission: wait it
    // out, bounded, then register with its successor (#1293).
    let mut stop_wait: Option<client::StopWait> = None;
    let (mut stream, address) = loop {
        // Inside the wait, the connect and the reply get only what is left of it.
        // Inside a stop wait the reconnect runs on the wait's owning deadline.
        let connected = match stop_wait.as_ref() {
            Some(wait) => client::connect_or_start_until(
                &socket,
                start_daemon,
                wait.deadline().min(Instant::now() + Duration::from_secs(5)),
            ),
            None => client::connect_or_start(&socket, start_daemon, Duration::from_secs(5)),
        };
        let mut stream = match connected {
            Ok(stream) => stream,
            Err(_)
                if stop_wait
                    .as_mut()
                    .is_some_and(|wait| wait.pause(&mut client::stderr_progress)) =>
            {
                continue;
            }
            Err(_) => anyhow::bail!("daemon_unavailable"),
        };
        // Serve registers in place and never waits for a checkout's index work
        // (Decision 0009 §6). The request and its reply share one absolute
        // deadline, inside a stop wait no later than the wait's own (#1293).
        let exchange = Instant::now() + Duration::from_secs(10);
        let deadline = stop_wait
            .as_ref()
            .map_or(exchange, |wait| wait.deadline().min(exchange));
        protocol::write_frame_until(&mut stream, &registration, deadline)?;
        let reply: protocol::Reply = protocol::read_frame_until(&mut stream, deadline)
            .map_err(|_| anyhow::anyhow!("outcome_unknown: inspect daemon listener status"))?;
        if protocol::is_stopping(&reply.payload)
            && stop_wait
                .get_or_insert_with(|| client::StopWait::new(client::STOP_WAIT))
                .pause(&mut client::stderr_progress)
        {
            continue;
        }
        break (stream, checked_reply(reply)?);
    };
    stream
        .set_read_timeout(None)
        .context("daemon_unavailable: prepare serve control read")?;
    let signal_stream = stream
        .try_clone()
        .context("daemon_unavailable: clone serve control connection")?;
    eprintln!(
        "Trellis: http://{}/\nState: {}\nToken file: {}\nIndexing starts when the browser, CLI or MCP first uses this checkout; the watcher then refreshes it after file changes.\nNo repository commands run.",
        address.as_str().unwrap_or("unknown"),
        roots.cache.display(),
        token_file.display()
    );
    // The test hook exposes the banner-to-select window. Both signals must
    // already be armed before a caller sees the banner.
    #[cfg(feature = "test-support")]
    if let Some(path) = std::env::var_os("TRELLIS_TEST_SERVE_AFTER_BANNER_SOCKET") {
        use std::io::{Read, Write};
        let mut hook = std::os::unix::net::UnixStream::connect(path)?;
        hook.set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))?;
        hook.write_all(b"B")?;
        hook.read_exact(&mut [0])?;
    }
    // The registration observes the daemon's lifetime, not a child wait status.
    // Even a normal EOF is unexpected while serve is running; never respawn here.
    let mut monitor = tokio::task::spawn_blocking(move || {
        let mut one = [0u8; 1];
        use std::io::Read;
        serve_control_failure(stream.read(&mut one))
    });
    let shutdown = async {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = termination.recv() => "SIGTERM",
                _ = interrupt.recv() => "SIGINT",
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            "SIGINT"
        }
    };
    tokio::select! {
        result = &mut monitor => {
            let error = match result {
                Ok(error) => error,
                Err(error) => anyhow::anyhow!("daemon_unavailable: serve monitor failed: {error}"),
            };
            use std::io::Write;
            let _ = writeln!(std::io::stderr().lock(), "Trellis serve stopping: {error}");
            return Err(error);
        }
        signal = shutdown => {
            use std::io::Write;
            let _ = writeln!(std::io::stderr().lock(), "Trellis serve stopping: received {signal}");
            signal_stream.shutdown(std::net::Shutdown::Both)?;
            let _ = monitor.await?;
        }
    }
    Ok(())
}

pub(crate) fn serve_control_failure(result: std::io::Result<usize>) -> anyhow::Error {
    match result {
        Ok(0) => anyhow::anyhow!(
            "daemon_unavailable: daemon control connection closed (EOF); daemon exit status unavailable"
        ),
        Ok(_) => anyhow::anyhow!("daemon_unavailable: unexpected data on serve control connection"),
        Err(error) => anyhow::anyhow!("daemon_unavailable: serve control read failed: {error}"),
    }
}

pub(crate) fn stop_existing_daemon() -> Result<serde_json::Value> {
    use std::io::ErrorKind;
    use std::os::unix::net::UnixStream;
    let mut stream = match UnixStream::connect(socket_path()?) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(serde_json::json!({"stopped": false}));
        }
        Err(error) => return Err(error).context("daemon_unavailable"),
    };
    // Accepted work may outlive ordinary CLI reply bounds. Success means the
    // elected owner has actually retired, not merely accepted the stop request.
    stream.set_write_timeout(Some(CLI_REPLY_BOUND))?;
    let request = daemon_request("daemon_stop", serde_json::json!({}));
    protocol::write_frame(&mut stream, &request)?;
    let reply: protocol::Reply = protocol::read_frame(&mut stream)
        .context("daemon_unavailable: stop completion not received")?;
    ensure!(reply.id == request.id, "invalid daemon stop reply");
    if protocol::is_stopping(&reply.payload) {
        // Another stop is under way: wait, on one owning deadline, until a
        // release is observed (#1293).
        drop(stream);
        let mut stop_wait = client::StopWait::new(client::STOP_WAIT);
        client::wait_for_stopping_owner(
            &socket_path()?,
            &mut stop_wait,
            &mut client::stderr_progress,
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        return Ok(serde_json::json!({"stopped": false}));
    }
    checked_reply(reply)
}
