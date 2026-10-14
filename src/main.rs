use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use trellis::{
    daemon::{self, client, protocol, registry},
    indexer::IndexOptions,
    mcp,
    model::ViewQuery,
    store::topology::{TopologyRoots, WorkspaceIdentity},
};

#[derive(Parser)]
#[command(
    name = "trellis",
    version,
    about = "Local, read-only JavaScript, Rust, Java and Python code index and inspection daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Build and atomically publish a complete index. Does not execute repository code.
    Index(IndexArgs),
    /// Run the elected user-level socket service.
    Daemon,
    /// Serve one read-only MCP connection on line-framed stdin/stdout.
    Mcp(WorkspaceArgs),
    /// Serve the authenticated loopback API and browser inspector. Refresh is explicit.
    Serve(Box<ServeArgs>),
    /// Print the current published index status.
    Status(WorkspaceArgs),
    /// Search indexed symbol names and IDs (literal substring).
    Symbols(SymbolsArgs),
    /// Query a bounded static call view. This is not a runtime sequence.
    Query(QueryArgs),
    /// Export the normalized snapshot graph as JSON, including indexed source.
    Export(ExportArgs),
    /// Inspect existing derived indexes and saved records without changing them.
    Gc(GcArgs),
    /// Permanently remove one saved workspace record by its exact ID.
    Forget(ForgetArgs),
}
#[derive(Args)]
struct ForgetArgs {
    record_id: String,
    /// Confirm without an interactive terminal.
    #[arg(long)]
    yes: bool,
}
#[derive(Args)]
struct GcArgs {
    /// Print the read-only cleanup inventory.
    #[arg(long, required = true)]
    report: bool,
}
#[derive(Args, Clone)]
struct WorkspaceArgs {
    /// Existing workspace directory; defaults to the nearest Git checkout from cwd.
    #[arg(long)]
    workspace: Option<PathBuf>,
}
#[derive(Args, Clone)]
struct IndexArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    /// Existing binary SCIP artifact. No semantic indexer is executed.
    #[arg(long)]
    scip: Option<PathBuf>,
    /// Paired flat JSON map of relative input paths to SHA-256 hashes.
    #[arg(long, requires = "scip")]
    manifest: Option<PathBuf>,
    /// Larger files are skipped with a diagnostic; valid range 1..16777216.
    #[arg(long)]
    max_file_bytes: Option<u64>,
}
#[derive(Args)]
struct ServeArgs {
    #[command(flatten)]
    index: IndexArgs,
    /// Loopback socket only. Port 0 selects an available port.
    #[arg(long, default_value = "127.0.0.1:8877")]
    bind: SocketAddr,
    /// Root of the read-only file tree. Defaults to the indexed workspace.
    #[arg(long)]
    browse_root: Option<PathBuf>,
    /// Optional external Rust source library LABEL=PATH (read-only candidates, not resolved targets).
    #[arg(long = "rust-source-root", value_parser = parse_rust_source_root)]
    rust_source_roots: Vec<(String, PathBuf)>,
    /// Rust library source directory; defaults to trusted RUST_SRC_PATH if set.
    #[arg(long)]
    rust_library: Option<PathBuf>,
    /// Trusted absolute rustc executable for startup-only sysroot discovery (never builds code).
    #[arg(long)]
    trusted_rustc: Option<PathBuf>,
    /// Cargo cache directory; defaults to CARGO_HOME or the user Cargo home.
    #[arg(long)]
    cargo_home: Option<PathBuf>,
    /// Existing private token file or a path at which to securely create one.
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// Explicit opt-in to source-bearing Jev requests; durable shared budget directory.
    #[arg(long, requires = "jev_budget_cents")]
    jev_budget_dir: Option<PathBuf>,
    /// Authorized reservation cap in cents. Existing ledger caps cannot be changed.
    #[arg(long, requires = "jev_budget_dir", value_parser = clap::value_parser!(u64).range(10..=500))]
    jev_budget_cents: Option<u64>,
    /// Explicit opt-in: trusted executable ACP runner (not a command from the inspected repository).
    #[arg(long, requires_all = ["acp_state_dir", "acp_max_attempts"])]
    acp_runner: Option<PathBuf>,
    /// Separate private ACP allowance/audit directory. Never use a Jev budget directory.
    #[arg(long, requires_all = ["acp_runner", "acp_max_attempts"])]
    acp_state_dir: Option<PathBuf>,
    /// Authorized ACP attempt allowance; immutable across restarts. Not a monetary cap.
    #[arg(long, requires_all = ["acp_runner", "acp_state_dir"], value_parser = clap::value_parser!(u64).range(1..=20))]
    acp_max_attempts: Option<u64>,
}
#[derive(Args)]
struct SymbolsArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    #[arg(long, default_value = "")]
    search: String,
    #[arg(long, default_value_t = 50)]
    limit: usize,
}
#[derive(Args)]
struct QueryArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    #[arg(long)]
    seed: String,
    #[arg(long, default_value_t = 1)]
    depth: usize,
    #[arg(long, default_value_t = 40)]
    max_nodes: usize,
    #[arg(long, default_value_t = 200)]
    max_calls: usize,
    /// Navigate callback definitions explicitly; never turns references into calls.
    #[arg(long)]
    include_callbacks: bool,
    #[arg(long)]
    exclude_path: Vec<String>,
}
#[derive(Args)]
struct ExportArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    /// Without this option, write JSON to stdout. Never overwrites an existing file.
    #[arg(long)]
    output: Option<PathBuf>,
}
impl WorkspaceArgs {
    fn resolve_unattached(&self) -> Result<(TopologyRoots, WorkspaceIdentity)> {
        let roots = TopologyRoots::production()?;
        let cwd = std::env::current_dir()?;
        let identity = WorkspaceIdentity::discover_unattached(self.workspace.as_deref(), &cwd)?;
        roots.reject_root_overlap(&identity)?;
        Ok((roots, identity))
    }
}
fn print_json(value: &impl Serialize) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

const CLI_READ_WAIT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<()> {
    let command = Cli::parse().command;
    if matches!(&command, Command::Index(_) | Command::Serve(_)) {
        trellis::capture::pin_running_executable()?;
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "trellis=info".into()),
        )
        .init();
    if matches!(&command, Command::Daemon) {
        return run_daemon().await;
    }
    if let Command::Serve(args) = &command {
        return serve_via_daemon(args).await;
    }
    match command {
        Command::Daemon | Command::Serve(_) => unreachable!(),
        Command::Mcp(args) => {
            let (_, identity) = args.resolve_unattached()?;
            let socket = socket_path()?;
            let stream = client::connect_or_start(&socket, start_daemon, Duration::from_secs(5))
                .map_err(|_| anyhow::anyhow!("daemon_unavailable"))?;
            client::relay_stdio(stream, &identity, || {
                client::connect_or_start(&socket, start_daemon, Duration::from_secs(5))
            })?;
        }
        Command::Gc(_) => {
            let request = daemon_request("gc_report", serde_json::json!({}));
            print_json(&send_daemon(&request, true)?)?;
        }
        Command::Forget(args) => {
            use std::io::{self, Write};
            let deleted = TopologyRoots::production()?.forget_with_confirmation(
                &args.record_id,
                |record, paths| {
                    eprintln!("Record ID: {}", record.id);
                    eprintln!(
                        "Saved views: {}; annotations: {}",
                        record.views, record.annotations
                    );
                    for path in paths {
                        eprintln!("Saved path: {path}");
                    }
                    eprintln!("WARNING: every checkout sharing this UUID loses these saved items.");
                    if args.yes {
                        return Ok(true);
                    }
                    ensure!(
                        unsafe { libc::isatty(libc::STDIN_FILENO) } == 1,
                        "confirmation requires a terminal or --yes"
                    );
                    eprint!("Type the exact record ID to forget: ");
                    io::stderr().flush()?;
                    let mut answer = String::new();
                    io::stdin().read_line(&mut answer)?;
                    Ok(answer.trim_end_matches(['\r', '\n']) == record.id)
                },
            )?;
            ensure!(
                deleted,
                "record ID confirmation did not match; no files removed"
            );
            eprintln!("Forgot record {}", args.record_id);
        }
        command @ (Command::Index(_)
        | Command::Status(_)
        | Command::Symbols(_)
        | Command::Query(_)
        | Command::Export(_)) => dispatch_cli(&command)?,
    }
    Ok(())
}

fn resolve_browse_root(options: &IndexOptions, explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| options.workspace_root.clone())
}

#[cfg(test)]
mod workspace_defaults {
    use super::*;

    #[test]
    fn serve_defaults_to_cwd_and_browses_the_index_workspace() {
        let Cli {
            command: Command::Serve(args),
        } = Cli::try_parse_from(["trellis", "serve"]).unwrap()
        else {
            panic!("expected serve");
        };
        assert_eq!(args.index.workspace.workspace, None);
        let options = IndexOptions::new(PathBuf::from("/chosen/project"));
        assert_eq!(
            resolve_browse_root(&options, args.browse_root),
            options.workspace_root
        );
        assert_eq!(
            resolve_browse_root(&options, Some(PathBuf::from("/explicit/tree"))),
            PathBuf::from("/explicit/tree")
        );
    }
}

fn parse_rust_source_root(value: &str) -> Result<(String, PathBuf), String> {
    let (label, path) = value.split_once('=').ok_or("expected LABEL=PATH")?;
    if label.is_empty()
        || label.len() > 48
        || !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || path.is_empty()
    {
        return Err("expected a short alphanumeric LABEL and a nonempty PATH".into());
    }
    Ok((label.into(), PathBuf::from(path)))
}

#[cfg(test)]
mod rust_source_arguments {
    use super::*;
    #[test]
    fn roots_are_explicit_named_paths_not_commands() {
        assert_eq!(
            parse_rust_source_root("std=/a/b=c").unwrap(),
            ("std".into(), PathBuf::from("/a/b=c"))
        );
        for value in [
            "/a/b",
            "=path",
            "std=",
            "../escape=/tmp",
            "label with spaces=/tmp",
        ] {
            assert!(parse_rust_source_root(value).is_err());
        }
        let Cli {
            command: Command::Serve(args),
        } = Cli::try_parse_from(["trellis", "serve"]).unwrap()
        else {
            panic!("serve")
        };
        assert!(args.rust_source_roots.is_empty());
    }
}

async fn discover_rust_library(
    compiler: &std::path::Path,
    workspace: &std::path::Path,
) -> Result<PathBuf> {
    use tokio::io::AsyncReadExt;
    ensure!(
        compiler.is_absolute(),
        "trusted rustc must be an absolute executable path"
    );
    let compiler = compiler.canonicalize().context("resolve trusted rustc")?;
    ensure!(
        !compiler.starts_with(workspace.canonicalize()?),
        "trusted rustc must be outside the inspected workspace"
    );
    let neutral = std::env::temp_dir()
        .canonicalize()
        .context("resolve neutral toolchain discovery directory")?;
    ensure!(
        !neutral.starts_with(workspace.canonicalize()?),
        "toolchain discovery directory must be outside the workspace"
    );
    let mut child = tokio::process::Command::new(&compiler)
        .args(["--print", "sysroot"])
        .env_clear()
        .current_dir(neutral)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start trusted rustc metadata probe")?;
    let stdout = child
        .stdout
        .take()
        .context("capture trusted rustc metadata")?;
    let mut bytes = Vec::new();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        stdout.take(4097).read_to_end(&mut bytes).await?;
        ensure!(bytes.len() <= 4096, "toolchain metadata exceeds limit");
        ensure!(
            child.wait().await?.success(),
            "trusted rustc metadata probe failed"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = child.kill().await;
            return Err(error);
        }
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!("trusted rustc metadata probe timed out");
        }
    }
    let sysroot = std::str::from_utf8(&bytes)
        .context("toolchain path is not UTF8")?
        .trim();
    ensure!(
        !sysroot.is_empty() && std::path::Path::new(sysroot).is_absolute(),
        "invalid toolchain sysroot path"
    );
    let library = PathBuf::from(sysroot).join("lib/rustlib/src/rust/library");
    ensure!(
        library.is_dir(),
        "Rust standard-library sources are missing from the trusted toolchain"
    );
    Ok(library)
}

#[cfg(all(test, unix))]
mod trusted_toolchain_probe_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[tokio::test]
    async fn rejects_workspace_compiler_without_execution() {
        let temp = tempfile::tempdir().unwrap();
        let compiler = temp.path().join("rustc");
        let marker = temp.path().join("executed");
        std::fs::write(&compiler, format!("#!/bin/sh\ntouch {:?}\n", marker)).unwrap();
        std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(discover_rust_library(&compiler, temp.path()).await.is_err());
        assert!(!marker.exists());
    }
    #[tokio::test]
    async fn trusted_probe_uses_fixed_metadata_arguments() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let sysroot = temp.path().join("toolchain");
        std::fs::create_dir_all(sysroot.join("lib/rustlib/src/rust/library")).unwrap();
        let compiler = temp.path().join("trusted-rustc");
        std::fs::write(&compiler,format!("#!/bin/sh\n[ \"$#\" = 2 ] || exit 1\n[ \"$1\" = --print ] || exit 1\n[ \"$2\" = sysroot ] || exit 1\n[ -z \"${{CARGO_MANIFEST_DIR+x}}\" ] || exit 1\nprintf '%s\\n' '{}'\n",sysroot.display())).unwrap();
        std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            discover_rust_library(&compiler, &workspace).await.unwrap(),
            sysroot.join("lib/rustlib/src/rust/library")
        );
    }
}

fn socket_path() -> Result<PathBuf> {
    Ok(daemon::SocketPaths::new(&TopologyRoots::production()?.data).socket)
}

fn start_daemon() -> std::io::Result<client::StartOutcome> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // The daemon must outlive the first CLI/MCP client's process group/session.
    // setsid is async-signal-safe: no allocation or lock is taken in the child.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    // Election losers exit quickly; a still-running winner must not block the caller.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(client::StartOutcome::Started)
}

fn daemon_request(operation: &str, payload: serde_json::Value) -> protocol::Request {
    protocol::Request {
        id: 1,
        operation: operation.to_owned(),
        payload,
    }
}

fn checked_reply(reply: protocol::Reply) -> Result<serde_json::Value> {
    if let Some(error) = reply.payload.get("error").and_then(|v| v.as_str()) {
        anyhow::bail!("{error}");
    }
    reply
        .payload
        .get("result")
        .cloned()
        .context("invalid daemon reply")
}

fn send_daemon(request: &protocol::Request, read_only: bool) -> Result<serde_json::Value> {
    let socket = socket_path()?;
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
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(stream)
    })
    .map_err(|error| anyhow::anyhow!("{}", error.code()))?;
    checked_reply(reply)
}

fn dispatch_cli(command: &Command) -> Result<()> {
    let (operation, workspace, mut payload, read_only) = match command {
        Command::Status(args) => ("status", args, serde_json::json!({}), true),
        Command::Symbols(args) => {
            ensure!((1..=150).contains(&args.limit), "limit must be 1..150");
            (
                "symbols",
                &args.workspace,
                serde_json::json!({"search": args.search, "limit": args.limit}),
                true,
            )
        }
        Command::Query(args) => {
            ViewQuery {
                seed: args.seed.clone(),
                depth: args.depth,
                max_nodes: args.max_nodes,
                max_calls: args.max_calls,
                include_callbacks: args.include_callbacks,
                exclude_paths: args.exclude_path.clone(),
            }
            .validate()?;
            (
                "query",
                &args.workspace,
                serde_json::json!({
                    "seed": args.seed, "depth": args.depth, "maxNodes": args.max_nodes,
                    "maxCalls": args.max_calls, "includeCallbacks": args.include_callbacks,
                    "excludePaths": args.exclude_path
                }),
                true,
            )
        }
        Command::Export(args) => ("export", &args.workspace, serde_json::json!({}), true),
        Command::Index(args) => (
            "index",
            &args.workspace,
            serde_json::json!({
                "scip": args.scip, "manifest": args.manifest,
                "maxFileBytes": args.max_file_bytes
            }),
            false,
        ),
        _ => unreachable!(),
    };
    let (roots, identity) = workspace.resolve_unattached()?;
    if let Command::Index(args) = command {
        ensure!(
            (1..=16_777_216).contains(&args.max_file_bytes.unwrap_or(2_097_152)),
            "max-file-bytes must be 1..16777216"
        );
        let mut options = IndexOptions::new(identity.root.clone());
        options.scip_path = args.scip.clone();
        options.manifest_path = args.manifest.clone();
        options.anchor_optional_inputs(&std::env::current_dir()?)?;
        payload["scip"] = serde_json::to_value(options.scip_path)?;
        payload["manifest"] = serde_json::to_value(options.manifest_path)?;
        if args.max_file_bytes.is_none() {
            payload
                .as_object_mut()
                .expect("index args object")
                .remove("maxFileBytes");
        }
    }
    if let Command::Export(args) = command
        && let Some(path) = &args.output
    {
        roots.validate_external(&identity, std::slice::from_ref(path))?;
    }
    let request = daemon_request(
        operation,
        serde_json::json!({
            "workspace": identity.root, "args": payload,
        }),
    );
    let value = send_daemon(&request, read_only)?;
    if let Command::Export(args) = command
        && let Some(path) = &args.output
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .context("create export (existing files are never overwritten)")?;
        serde_json::to_writer_pretty(&mut file, &value)?;
        file.write_all(b"\n")?;
        file.flush()?;
    } else {
        print_json(&value)?;
    }
    Ok(())
}

async fn serve_via_daemon(args: &ServeArgs) -> Result<()> {
    // Arm SIGTERM before any readiness banner: the serve control select is
    // polled later, but callers may signal as soon as they see the banner.
    #[cfg(unix)]
    let mut termination = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("register serve SIGTERM handler")?;
    ensure!(
        args.bind.ip().is_loopback(),
        "only loopback bind addresses are supported"
    );
    let token_file = args
        .token_file
        .as_ref()
        .context("serve requires explicit --token-file")?;
    let (roots, identity) = args.index.workspace.resolve_unattached()?;
    let mut destinations = vec![token_file.clone()];
    destinations.extend(args.jev_budget_dir.iter().cloned());
    destinations.extend(args.acp_state_dir.iter().cloned());
    roots.validate_external(&identity, &destinations)?;
    if args.jev_budget_dir.is_some() {
        let key = std::env::var("JEV_KEY")
            .map_err(|_| anyhow::anyhow!("JEV_KEY must be configured to enable live Jev"))?;
        ensure!(
            !key.trim().is_empty()
                && reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).is_ok(),
            "JEV_KEY must be configured to enable live Jev"
        );
    }
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
    index_options.scip_path = args.index.scip.clone();
    index_options.manifest_path = args.index.manifest.clone();
    index_options.max_file_bytes = args.index.max_file_bytes.unwrap_or(2_097_152);
    ensure!(
        (1..=16_777_216).contains(&index_options.max_file_bytes),
        "max-file-bytes must be 1..16777216"
    );
    index_options.anchor_optional_inputs(&std::env::current_dir()?)?;
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
        scip: index_options.scip_path,
        manifest: index_options.manifest_path,
        max_file_bytes: index_options.max_file_bytes,
        rust_source_roots: args.rust_source_roots.clone(),
        rust_library,
        trusted_rustc: args.trusted_rustc.clone(),
        cargo_home: args
            .cargo_home
            .clone()
            .or_else(|| std::env::var_os("CARGO_HOME").map(PathBuf::from))
            .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().join(".cargo"))),
        jev_budget_dir: args.jev_budget_dir.clone(),
        jev_budget_cents: args.jev_budget_cents,
        jev_key: if args.jev_budget_dir.is_some() {
            Some(std::env::var("JEV_KEY")?)
        } else {
            None
        },
        acp_runner: args.acp_runner.clone(),
        acp_state_dir: args.acp_state_dir.clone(),
        acp_max_attempts: args.acp_max_attempts,
    };
    let mut options_json = serde_json::to_value(options)?;
    let fields = options_json
        .as_object_mut()
        .expect("browser options object");
    // Only index options actually supplied by this caller can supersede a
    // durable option. Other BrowserOptions fields retain their normal shape.
    if args.index.scip.is_none() {
        fields.remove("scip");
    }
    if args.index.manifest.is_none() {
        fields.remove("manifest");
    }
    if args.index.max_file_bytes.is_none() {
        fields.remove("maxFileBytes");
    }
    let socket = socket_path()?;
    let mut stream = client::connect_or_start(&socket, start_daemon, Duration::from_secs(5))
        .map_err(|_| anyhow::anyhow!("daemon_unavailable"))?;
    // Serve registers in place and never waits for a checkout's index work
    // (Decision 0009 §6).
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    protocol::write_frame(
        &mut stream,
        &daemon_request(
            "serve",
            serde_json::json!({
                "workspace": identity.root, "bind": args.bind, "tokenFile": token_file,
                "options": options_json,
            }),
        ),
    )?;
    let reply: protocol::Reply = protocol::read_frame(&mut stream)
        .map_err(|_| anyhow::anyhow!("outcome_unknown: inspect daemon listener status"))?;
    let address = checked_reply(reply)?;
    eprintln!(
        "Trellis: http://{}/\nState: {}\nToken file: {}\nRefresh is explicit. No repository commands run.",
        address.as_str().unwrap_or("unknown"),
        roots.cache.display(),
        token_file.display()
    );
    // The opt-in test pause exposes the banner-to-select window. SIGTERM must
    // already be armed; normal serve executions never pause here.
    #[cfg(feature = "test-support")]
    if std::env::var("TRELLIS_TEST_SERVE_AFTER_BANNER_PAUSE").as_deref() == Ok("1") {
        std::thread::sleep(Duration::from_millis(750));
    }
    // The registration is a control connection, not a checkout attachment. Its
    // normal EOF is the daemon's intentional idle exit; never respawn here.
    stream.set_read_timeout(None)?;
    let signal_stream = stream.try_clone()?;
    let mut monitor = tokio::task::spawn_blocking(move || {
        let mut one = [0u8; 1];
        use std::io::Read;
        match stream.read(&mut one) {
            Ok(0) => Ok(()),
            _ => anyhow::bail!("daemon_unavailable: serve control interrupted"),
        }
    });
    let shutdown = async {
        #[cfg(unix)]
        tokio::select! {
            _ = termination.recv() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    };
    tokio::select! {
        result = &mut monitor => result??,
        _ = shutdown => {
            signal_stream.shutdown(std::net::Shutdown::Both)?;
            let _ = monitor.await?;
        }
    }
    Ok(())
}

async fn run_daemon() -> Result<()> {
    let paths = daemon::SocketPaths::new(&TopologyRoots::production()?.data);
    let Some(owner) = daemon::SocketOwner::acquire(&paths)? else {
        return Ok(());
    };
    // Test-only ownership marker: only the elected daemon records its PID.
    #[cfg(feature = "test-support")]
    if let Some(path) = std::env::var_os("TRELLIS_TEST_DAEMON_PID_FILE") {
        std::fs::write(path, std::process::id().to_string())?;
    }
    owner.listener().set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(owner.listener().try_clone()?)?;
    let registry = Arc::new(tokio::sync::Mutex::new(registry::CheckoutRegistry::new()));
    let browser = Arc::new(tokio::sync::Mutex::new(daemon::BrowserProvisioner::new()));
    let next_session = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let stopping = Arc::new(AtomicBool::new(false));
    let (browser_shutdown, _) = tokio::sync::watch::channel(false);
    let idle = daemon::run_idle_lifecycle(registry.clone());
    tokio::pin!(idle);
    // A single injected accept fault tests that the elected socket survives it.
    #[cfg(feature = "test-support")]
    let mut injected_accept_errno =
        match std::env::var("TRELLIS_TEST_DAEMON_ACCEPT_ERROR_ONCE").as_deref() {
            Ok("emfile") => Some(libc::EMFILE),
            Ok("econnaborted") => Some(libc::ECONNABORTED),
            _ => None,
        };
    loop {
        tokio::select! {
            result = &mut idle => { result.map_err(|e| anyhow::anyhow!("{}", e.reason()))?; break; }
            accepted = async {
                #[cfg(feature = "test-support")]
                if let Some(errno) = injected_accept_errno {
                    Err(std::io::Error::from_raw_os_error(errno))
                } else {
                    listener.accept().await
                }
                #[cfg(not(feature = "test-support"))]
                {
                    listener.accept().await
                }
            } => {
                #[cfg(feature = "test-support")]
                { injected_accept_errno = None; }
                let (stream, _) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        use std::io::Write;
                        let _ = writeln!(std::io::stderr().lock(), "daemon socket accept failed: {error}");
                        // Even a persistent EMFILE must not hot-spin or drop the elected socket.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let stream = stream.into_std()?;
                stream.set_nonblocking(false)?;
                let registry = registry.clone();
                let browser = browser.clone();
                let id = next_session.fetch_add(1, Ordering::Relaxed);
                let stopping = stopping.clone();
                let browser_shutdown = browser_shutdown.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = dispatch_connection(stream, id, registry, browser, stopping, browser_shutdown);
                });
            }
        }
    }
    stopping.store(true, Ordering::Release);
    let _ = browser_shutdown.send(true);
    drop(listener);
    drop(owner);
    Ok(())
}

fn checked_serve_options(raw: serde_json::Value) -> Result<registry::CheckoutOptions> {
    let _: registry::BrowserOptions = serde_json::from_value(raw.clone())?;
    // Validation must not fill in a missing optional key before lifecycle
    // compares the caller's explicitly supplied options.
    Ok(registry::CheckoutOptions(raw))
}

#[cfg(test)]
mod serve_option_transport_tests {
    use super::*;
    #[test]
    fn daemon_receipt_keeps_omitted_and_explicit_default_distinct() {
        let options = serde_json::to_value(registry::BrowserOptions::default()).unwrap();
        let mut omitted = options.clone();
        for key in ["scip", "manifest", "maxFileBytes"] {
            omitted.as_object_mut().unwrap().remove(key);
        }
        let absent = checked_serve_options(omitted).unwrap();
        let explicit = checked_serve_options(options).unwrap();
        for key in ["scip", "manifest", "maxFileBytes"] {
            assert!(absent.0.get(key).is_none(), "{}", absent.0);
        }
        assert_eq!(explicit.0["maxFileBytes"], 2_097_152);
        assert_eq!(explicit.0.get("scip"), Some(&serde_json::Value::Null));
        assert_eq!(explicit.0.get("manifest"), Some(&serde_json::Value::Null));
    }
}

fn dispatch_connection(
    mut stream: std::os::unix::net::UnixStream,
    session: u64,
    registry: Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
    browser: Arc<tokio::sync::Mutex<daemon::BrowserProvisioner>>,
    stopping: Arc<AtomicBool>,
    browser_shutdown: tokio::sync::watch::Sender<bool>,
) -> Result<()> {
    let runtime = tokio::runtime::Handle::current();
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let request: protocol::Request = protocol::read_frame(&mut stream)?;
    if request.operation == "mcp" {
        let relayed = request
            .payload
            .get("relay")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let result = (|| -> Result<mcp::OpenedWorkspace> {
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
            runtime.block_on(async {
                registry
                    .lock()
                    .await
                    .attach_launch(session, &identity)
                    .map_err(|error| anyhow::anyhow!("{}", error.reason()))
            })?;
            Ok(mcp::OpenedWorkspace::new(identity))
        })();
        let reply = protocol::Reply {
            id: request.id,
            payload: match &result {
                Ok(_) => serde_json::json!({"result": true}),
                Err(error) => serde_json::json!({"error": format!("{error:#}")}),
            },
        };
        let delivered = protocol::write_frame(&mut stream, &reply);
        let served = match (result, delivered) {
            (Ok(workspace), Ok(())) => {
                // The CLI request deadline must not end an idle MCP attachment.
                stream
                    .set_read_timeout(None)
                    .and_then(|()| {
                        // `trellis mcp`'s relay frames each line with its own
                        // relay ID (Decision 0009 §7).
                        if relayed {
                            mcp::run_relayed_socket(
                                workspace,
                                stream,
                                registry.clone(),
                                session,
                                runtime.clone(),
                            )
                        } else {
                            mcp::run_socket(
                                workspace,
                                stream,
                                registry.clone(),
                                session,
                                runtime.clone(),
                            )
                        }
                    })
                    .map_err(anyhow::Error::from)
            }
            (Err(error), _) => Err(error),
            (_, Err(error)) => Err(error.into()),
        };
        runtime.block_on(async { registry.lock().await.disconnect(session) });
        return served;
    }
    if request.operation == "gc_report" {
        let result = TopologyRoots::production().and_then(|roots| roots.gc_report());
        let reply = protocol::Reply {
            id: request.id,
            payload: match result {
                Ok(value) => serde_json::json!({"result": value}),
                Err(error) => serde_json::json!({"error": format!("{error:#}")}),
            },
        };
        return write_daemon_reply(&mut stream, &reply);
    }
    let serve = request.operation == "serve";
    let result = (|| -> Result<serde_json::Value> {
        let root: PathBuf = serde_json::from_value(
            request
                .payload
                .get("workspace")
                .cloned()
                .context("missing workspace")?,
        )?;
        let identity = WorkspaceIdentity::discover_unattached(Some(&root), &root)?;
        TopologyRoots::production()?.reject_root_overlap(&identity)?;
        if serve {
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
                    .register_serve(&registry, &identity, options, bind, &token_file)
                    .await?;
                if first {
                    browser.spawn_with_shutdown(browser_shutdown.subscribe())?;
                }
                Ok::<_, anyhow::Error>(address)
            })?;
            return Ok(serde_json::json!(address.to_string()));
        }
        ensure!(
            matches!(
                request.operation.as_str(),
                "status" | "symbols" | "query" | "export" | "index"
            ),
            "unknown daemon operation"
        );
        let identity = if request.operation == "status" {
            identity.attach_existing_marker_readonly()?
        } else {
            identity.attach_marker()?
        };
        // Virgin queue publication can contend with a short-lived index-use
        // reader before any request ID has been accepted. Retry only that
        // typed pre-acceptance admission; never replay an enqueue after COMMIT.
        let activation_deadline = Instant::now() + CLI_READ_WAIT;
        let checkout = loop {
            let activated = runtime.block_on(async {
                let mut checked = registry.lock().await;
                checked
                    .attach_launch(session, &identity)
                    .map_err(|e| anyhow::anyhow!("{}", e.reason()))?;
                checked.activate(&identity.root_key)
            });
            match activated {
                Ok(checkout) => break checkout,
                Err(error) if trellis::store::cli_read_retryable_contention(&error) => {
                    if Instant::now() >= activation_deadline {
                        return Err(trellis::store::cli_read_busy_expired());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error),
            }
        };
        let args = request.payload.get("args").context("missing args")?;
        let deadline = Instant::now() + CLI_READ_WAIT;
        loop {
            // Recorded before the read: a state change between the read and
            // the wait below is never missed.
            let state_version = checkout.state_version();
            let selected = match request.operation.as_str() {
                "status" => checkout.cli_status(),
                "symbols" => checkout.cli_symbols(
                    args["search"].as_str().context("missing search")?,
                    args["limit"].as_u64().context("missing limit")? as usize,
                ),
                "query" => checkout.cli_query(&ViewQuery {
                    seed: args["seed"].as_str().context("missing seed")?.into(),
                    depth: args["depth"].as_u64().context("missing depth")? as usize,
                    max_nodes: args["maxNodes"].as_u64().context("missing maxNodes")? as usize,
                    max_calls: args["maxCalls"].as_u64().context("missing maxCalls")? as usize,
                    include_callbacks: args["includeCallbacks"]
                        .as_bool()
                        .context("missing includeCallbacks")?,
                    exclude_paths: serde_json::from_value(args["excludePaths"].clone())?,
                }),
                "export" => checkout.cli_export(),
                "index" => {
                    let mut options = IndexOptions::new(identity.root.clone());
                    options.scip_path = serde_json::from_value(args["scip"].clone())?;
                    options.manifest_path = serde_json::from_value(args["manifest"].clone())?;
                    if let Some(value) = args.get("maxFileBytes") {
                        options.max_file_bytes = value.as_u64().context("invalid maxFileBytes")?;
                    }
                    ensure!(
                        (1..=16_777_216).contains(&options.max_file_bytes),
                        "max-file-bytes must be 1..16777216"
                    );
                    options.anchor_optional_inputs(&std::env::current_dir()?)?;
                    checkout.cli_index(&options)
                }
                _ => anyhow::bail!("unknown daemon operation"),
            };
            // Mandatory H may be running when a CLI read first attaches.
            // This only repeats read-only calls; index is never replayed.
            if request.operation != "index"
                && selected
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.to_string().starts_with("index_not_ready:"))
                && Instant::now() < deadline
            {
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
    })();
    if !serve {
        runtime.block_on(async { registry.lock().await.disconnect(session) });
    }
    let reply = protocol::Reply {
        id: request.id,
        payload: match result {
            Ok(value) => serde_json::json!({"result": value}),
            Err(error) => serde_json::json!({"error": format!("{error:#}")}),
        },
    };
    write_daemon_reply(&mut stream, &reply)?;
    if serve {
        // This control connection only observes the daemon process lifetime.
        stream.set_read_timeout(Some(Duration::from_millis(250)))?;
        let mut one = [0u8; 1];
        use std::io::Read;
        while !stopping.load(Ordering::Acquire) {
            match stream.read(&mut one) {
                Ok(0) => break,
                Ok(_) => (),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }
        }
    }
    Ok(())
}

fn write_daemon_reply(
    stream: &mut std::os::unix::net::UnixStream,
    reply: &protocol::Reply,
) -> Result<()> {
    let payload = serde_json::to_vec(&reply.payload)?;
    if payload.len() + 64 <= protocol::MAX_FRAME {
        protocol::write_frame(stream, reply)?;
    } else {
        const CHUNK_BYTES: usize = 450_000;
        for (index, chunk) in payload.chunks(CHUNK_BYTES).enumerate() {
            let more = (index + 1) * CHUNK_BYTES < payload.len();
            protocol::write_frame(
                stream,
                &protocol::Reply {
                    id: reply.id,
                    payload: serde_json::json!({"chunk": hex::encode(chunk), "more": more}),
                },
            )?;
        }
    }
    Ok(())
}
