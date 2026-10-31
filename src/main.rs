use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use trellis::{
    daemon::{self, client, protocol, registry},
    indexer::IndexOptions,
    mcp,
    model::ViewQuery,
    store::topology::{TopologyRoots, WorkspaceIdentity},
};

#[cfg(feature = "test-support")]
mod native_navigation_fixture;

#[derive(Parser)]
#[command(
    name = "trellis",
    version = trellis::build_info::version(),
    about = "Local, read-only JavaScript, Rust, Java and Python code index and inspection daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    #[command(hide = true)]
    GitRead(GitReadArgs),
    /// Export fixture snapshots through the real native extractor (test-support build only).
    #[cfg(feature = "test-support")]
    NativeCapture(NativeCaptureArgs),
    #[cfg(feature = "test-support")]
    #[command(hide = true)]
    NavigationFixture(NavigationFixtureArgs),
    /// Build and atomically publish a complete index. Does not execute repository code.
    Index(IndexArgs),
    /// Explicitly import pinned SCIP evidence into the selected native index.
    Semantic(SemanticCommand),
    /// Plan and inspect headless semantic producers. Planning executes nothing.
    Producer(ProducerCommand),
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
    /// Compute pinned source complexity metrics without writing the index.
    Metrics(MetricsArgs),
    /// Check reviewed architecture rules against captured source.
    Rules(RulesArgs),
    /// Show captured structural file-module references, not resolved call edges.
    References(ReferencesArgs),
    /// Query a bounded static call view. This is not a runtime sequence.
    Query(QueryArgs),
    /// Inspect existing derived indexes and the vector cache without changing them.
    Gc(GcArgs),
}
#[cfg(feature = "test-support")]
#[derive(Args)]
struct NativeCaptureArgs {
    /// Print the compiled producer descriptor and extraction-input inventory.
    #[arg(long, conflicts_with_all = ["fixture", "output", "source_set", "revision"])]
    describe: bool,
    /// Fixture directory or fixture.json; only its listed snapshot sources are measured.
    #[arg(long, required_unless_present = "describe")]
    fixture: Option<PathBuf>,
    /// New output path. Existing files are never replaced.
    #[arg(long, required_unless_present = "describe")]
    output: Option<PathBuf>,
    #[arg(long)]
    source_set: Option<String>,
    #[arg(long)]
    revision: Option<String>,
}
#[cfg(feature = "test-support")]
#[derive(Args)]
struct NavigationFixtureArgs {
    #[arg(long)]
    workspace: PathBuf,
    #[arg(long, value_parser = [
        "zero", "sole", "multiple", "unresolved",
        "sequence-sole", "sequence-zero", "sequence-multiple", "sequence-unresolved",
        "sequence-ambiguous", "sequence-stale", "sequence-dispatch", "sequence-external",
        "sequence-missing", "sequence-constructor", "sequence-library", "sequence-expand"
    ])]
    mode: String,
}
#[derive(Args)]
struct RulesArgs {
    #[command(subcommand)]
    command: RulesCommand,
}
#[derive(Subcommand)]
enum RulesCommand {
    Check(RulesCheckArgs),
}
#[derive(Args)]
struct RulesCheckArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    #[arg(long, default_value = "json", value_parser = ["json"])]
    format: String,
    /// Full index generation:revision pair; omitted means the admitted head.
    #[arg(long)]
    pin: Option<String>,
    /// Source path glob; target evidence is not restricted.
    #[arg(long = "path")]
    paths: Vec<String>,
}
#[derive(Args)]
struct ReferencesArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    #[arg(long)]
    pin: Option<String>,
    #[arg(long = "path")]
    paths: Vec<String>,
    #[arg(long, default_value_t = 5)]
    limit: usize,
}
#[derive(Args)]
struct GitReadArgs {
    #[arg(long)]
    root: PathBuf,
    #[arg(long, value_parser = ["head", "summary", "status", "diff"])]
    mode: String,
    #[arg(long)]
    path: Option<String>,
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
    /// Larger files are skipped with a diagnostic; valid range 1..16777216.
    #[arg(long)]
    max_file_bytes: Option<u64>,
}
#[derive(Args)]
struct SemanticCommand {
    #[command(subcommand)]
    action: SemanticAction,
}
#[derive(Subcommand)]
enum SemanticAction {
    Import(SemanticImportArgs),
}
#[derive(Args)]
struct SemanticImportArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    /// Batch descriptor file inside the workspace.
    #[arg(long = "batch")]
    batch: PathBuf,
    /// Lowercase SHA-256 of the descriptor file.
    #[arg(long = "batch-sha256")]
    batch_sha256: String,
}
#[derive(Args)]
struct ProducerCommand {
    #[command(subcommand)]
    action: ProducerAction,
}
#[derive(Subcommand)]
enum ProducerAction {
    /// Print everything a producer run would do, with its planSha256. Executes nothing.
    Plan(ProducerPlanArgs),
    /// Run one producer after inspecting its plan; refused unless --plan-sha256 matches.
    Run(ProducerRunArgs),
    /// Cancel a run that has not yet enqueued its import.
    Cancel(ProducerCancelArgs),
    /// Report producer detection, trust-profile availability and run state.
    Status(ProducerStatusArgs),
    /// Import an owner-configured artifact without running a producer, or clear the entry.
    Pickup(ProducerPickupArgs),
}
#[derive(Args)]
struct ProducerPickupArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(trellis::producer::profiles::cli_producer_names()))]
    producer: String,
    /// Artifact to import now and whenever it changes.
    #[arg(long, conflicts_with = "clear", required_unless_present = "clear")]
    artifact: Option<PathBuf>,
    /// Remove the producer's pickup configuration.
    #[arg(long)]
    clear: bool,
}
#[derive(Args)]
struct ProducerPlanArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(trellis::producer::profiles::cli_producer_names()))]
    producer: String,
    /// `restricted` runs no repository code; `trusted` allows it.
    #[arg(long, default_value = "restricted", value_parser = ["restricted", "trusted"])]
    trust: String,
    /// Wall-clock bound in seconds, 1..14400.
    #[arg(long, default_value_t = 1800)]
    timeout_secs: u64,
    /// Sampled resident-set bound in MiB, 64..65536.
    #[arg(long, default_value_t = 8192)]
    memory_mib: u64,
    /// Launcher to use instead of resolving one on PATH.
    #[arg(long)]
    executable: Option<PathBuf>,
}
#[derive(Args)]
struct ProducerRunArgs {
    #[command(flatten)]
    plan: ProducerPlanArgs,
    /// The planSha256 printed by `producer plan` for these same arguments.
    #[arg(long)]
    plan_sha256: String,
}
#[derive(Args)]
struct ProducerCancelArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    /// The run to cancel.
    #[arg(long)]
    run: String,
}
#[derive(Args)]
struct ProducerStatusArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    /// Report one run.
    #[arg(long)]
    run: Option<String>,
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
    /// A published semantic producer for call bindings. Default: each language's sole one.
    #[arg(long)]
    semantic_producer_id: Option<String>,
}
#[derive(Args)]
struct MetricsArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    pin: Option<String>,
    #[arg(long, default_value = "revision")]
    scope: String,
    #[arg(long)]
    source_set_id: Option<String>,
    #[arg(long)]
    language: Option<String>,
    #[arg(long)]
    path: Option<String>,
    #[arg(long)]
    sid: Option<String>,
    #[arg(long, default_value = "sum")]
    aggregate: String,
    #[arg(long, default_value = "all")]
    role: String,
    #[arg(long)]
    metrics: Option<String>,
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
    if let Command::GitRead(args) = &command {
        trellis::git_status::write_child_response(&args.root, &args.mode, args.path.as_deref())?;
        return Ok(());
    }
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
        #[cfg(feature = "test-support")]
        Command::NativeCapture(args) => {
            if args.describe {
                print_json(&trellis::native_capture::describe())?;
            } else {
                trellis::native_capture::capture(
                    args.fixture.as_deref().context("missing fixture")?,
                    args.output.as_deref().context("missing output")?,
                    args.source_set.as_deref(),
                    args.revision.as_deref(),
                )?;
            }
        }
        #[cfg(feature = "test-support")]
        Command::NavigationFixture(args) => {
            print_json(&native_navigation_fixture::publish(
                &args.workspace,
                &args.mode,
            )?)?;
        }
        Command::Daemon | Command::Serve(_) | Command::GitRead(_) => unreachable!(),
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
        command @ (Command::Index(_)
        | Command::Semantic(_)
        | Command::Producer(_)
        | Command::Status(_)
        | Command::Symbols(_)
        | Command::Query(_)
        | Command::Metrics(_)
        | Command::Rules(_)
        | Command::References(_)) => dispatch_cli(&command)?,
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
    // A fixture may keep an auto-started daemon's diagnostics.
    #[cfg(feature = "test-support")]
    if let Some(path) = std::env::var_os("TRELLIS_TEST_DAEMON_STDERR") {
        command.stderr(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?,
        );
    }
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

/// How long `trellis index` waits on a silent daemon for its terminal reply
/// (#179). Like every CLI read bound it limits inactivity per socket read, not
/// the whole reply: a liveness backstop, not a deadline.
const INDEX_REPLY_BOUND: Duration = Duration::from_secs(10 * 60);

/// How long every other non-metrics CLI call waits on a silent daemon, per
/// socket read. Tests lower it with `TRELLIS_TEST_CLI_REPLY_BOUND_MS` to prove
/// a call outlives it without waiting a minute (#201).
const CLI_REPLY_BOUND: Duration = Duration::from_secs(60);

#[cfg(test)]
mod reply_bound_tests {
    use super::*;
    #[test]
    fn index_waits_longer_than_other_cli_calls() {
        // #179: an index reply may take longer than the generic CLI bound.
        assert!(INDEX_REPLY_BOUND > CLI_REPLY_BOUND);
    }
}

fn send_daemon(request: &protocol::Request, read_only: bool) -> Result<serde_json::Value> {
    let socket = socket_path()?;
    if request.operation == "metrics" {
        let started = Instant::now();
        let ingress_window = Duration::from_secs(20);
        #[cfg(feature = "test-support")]
        let ingress_window = metrics_test_duration("TRELLIS_TEST_METRICS_CLIENT_INGRESS_MS")
            .unwrap_or(ingress_window);
        let ingress = started + ingress_window;
        let work = if matches!(
            request.payload["args"]["scope"].as_str(),
            Some("revision" | "directory")
        ) {
            Duration::from_secs(300)
        } else {
            Duration::from_secs(30)
        };
        #[cfg(feature = "test-support")]
        let work = metrics_test_duration("TRELLIS_TEST_METRICS_CLIENT_WORK_MS").unwrap_or(work);
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
        return checked_reply(reply);
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
    let reply_bound = if request.operation == "producer_run" {
        // The run's own timeout, plus the termination grace and the import's wait.
        let timeout_secs = request.payload["args"]["limits"]["timeoutSecs"]
            .as_u64()
            .unwrap_or(1800)
            .min(14400);
        Duration::from_secs(timeout_secs + 630)
    } else if matches!(
        request.operation.as_str(),
        "index" | "semantic_import" | "producer_pickup"
    ) {
        index_bound
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
    checked_reply(reply)
}

/// The `producer_plan` and `producer_run` payload for `args`, with the caller's declared
/// environment.
fn producer_plan_payload(args: &ProducerPlanArgs) -> Result<serde_json::Value> {
    ensure!(
        trellis::producer::plan::TIMEOUT_SECS.contains(&args.timeout_secs)
            && trellis::producer::plan::MEMORY_MIB.contains(&args.memory_mib),
        "invalid_request: producer limits"
    );
    let executable = args
        .executable
        .as_ref()
        .map(|path| -> Result<String> {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                std::env::current_dir()?.join(path)
            };
            Ok(absolute
                .to_str()
                .context("invalid_request: non-UTF8 executable path")?
                .to_owned())
        })
        .transpose()?;
    Ok(serde_json::json!({
        "version": 1,
        "producer": args.producer,
        "trustProfile": args.trust,
        "limits": {"timeoutSecs": args.timeout_secs, "memoryMiB": args.memory_mib},
        "executable": executable,
        "environment": trellis::producer::detect::Environment::from_process()?,
    }))
}

fn dispatch_cli(command: &Command) -> Result<()> {
    let (operation, workspace, mut payload, read_only) = match command {
        Command::Status(args) => ("status", args, serde_json::json!({}), true),
        Command::Rules(RulesArgs {
            command: RulesCommand::Check(args),
        }) => (
            "rules_check",
            &args.workspace,
            architecture_cli_arguments(args.pin.as_deref(), &args.paths, None)?,
            true,
        ),
        Command::References(args) => (
            "references",
            &args.workspace,
            architecture_cli_arguments(args.pin.as_deref(), &args.paths, Some(args.limit))?,
            true,
        ),
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
                semantic_producer_id: args.semantic_producer_id.clone(),
            }
            .validate()?;
            (
                "query",
                &args.workspace,
                serde_json::json!({
                    "seed": args.seed, "depth": args.depth, "maxNodes": args.max_nodes,
                    "maxCalls": args.max_calls, "includeCallbacks": args.include_callbacks,
                    "excludePaths": args.exclude_path,
                    "semanticProducerId": args.semantic_producer_id
                }),
                true,
            )
        }
        Command::Metrics(args) => {
            let mut options = trellis::store::MetricsOptions {
                scope: args.scope.clone(),
                source_set_id: args.source_set_id.clone(),
                language: args.language.clone(),
                path: args.path.clone(),
                sid: args.sid.clone(),
                aggregate: args.aggregate.clone(),
                role: args.role.clone(),
                ..Default::default()
            };
            if let Some(names) = &args.metrics {
                ensure!(
                    !names.is_empty(),
                    "invalid_argument: empty metrics selection"
                );
                options.metrics = names.split(',').map(str::to_owned).collect();
            }
            if let Some(pin) = &args.pin {
                let (generation, revision) = pin
                    .rsplit_once(':')
                    .context("invalid_argument: pin requires generation:revision")?;
                ensure!(
                    !revision.is_empty() && revision.bytes().all(|byte| byte.is_ascii_digit()),
                    "invalid_argument: invalid revision"
                );
                let revision = revision
                    .parse::<u64>()
                    .map_err(|_| anyhow::anyhow!("invalid_argument: invalid revision"))?;
                options.pin = Some(
                    serde_json::from_value(serde_json::json!({
                        "indexGeneration": generation, "indexRevision": revision
                    }))
                    .map_err(|error| anyhow::anyhow!("invalid_argument: invalid pin: {error}"))?,
                );
            }
            options
                .validate()
                .map_err(|error| anyhow::anyhow!("invalid_argument: {error}"))?;
            ensure!(
                options.source_set_id.as_deref().is_none_or(|id| id
                    .starts_with("source-set:v1:path-")
                    && id.len() == 83
                    && id[19..]
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
                "invalid_argument: source set ID"
            );
            ensure!(
                options
                    .sid
                    .as_deref()
                    .is_none_or(|sid| sid.starts_with("sid:v1:")
                        && sid.len() == 39
                        && sid[7..]
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
                "invalid_argument: sid"
            );
            ensure!(
                options
                    .path
                    .as_deref()
                    .is_none_or(|path| !path.contains([':', '\\', '\0'])),
                "invalid_argument: path"
            );
            ensure!(
                options.language.as_deref().is_none_or(|language| matches!(
                    language,
                    "java" | "python" | "javascript" | "rust"
                )),
                "invalid_argument: language"
            );
            (
                "metrics",
                &args.workspace,
                serde_json::json!({
                    "scope": options.scope, "sourceSetId": options.source_set_id,
                    "language": options.language, "path": options.path,
                    "sid": options.sid, "aggregate": options.aggregate, "role": options.role,
                    "metrics": options.metrics, "pin": options.pin
                }),
                true,
            )
        }
        Command::Index(args) => (
            "index",
            &args.workspace,
            serde_json::json!({
                "maxFileBytes": args.max_file_bytes
            }),
            false,
        ),
        Command::Producer(ProducerCommand {
            action: ProducerAction::Plan(args),
        }) => (
            "producer_plan",
            &args.workspace,
            producer_plan_payload(args)?,
            true,
        ),
        Command::Producer(ProducerCommand {
            action: ProducerAction::Run(args),
        }) => {
            ensure!(
                args.plan_sha256.len() == 64
                    && args
                        .plan_sha256
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "invalid_request: plan digest"
            );
            let mut payload = producer_plan_payload(&args.plan)?;
            payload["planSha256"] = serde_json::json!(args.plan_sha256);
            ("producer_run", &args.plan.workspace, payload, false)
        }
        Command::Producer(ProducerCommand {
            action: ProducerAction::Cancel(args),
        }) => (
            "producer_cancel",
            &args.workspace,
            serde_json::json!({"version": 1, "runId": args.run}),
            false,
        ),
        Command::Producer(ProducerCommand {
            action: ProducerAction::Status(args),
        }) => (
            "producer_status",
            &args.workspace,
            serde_json::json!({
                "version": 1,
                "runId": args.run,
                "environment": trellis::producer::detect::Environment::from_process()?,
            }),
            true,
        ),
        Command::Producer(ProducerCommand {
            action: ProducerAction::Pickup(args),
        }) => {
            let artifact = args
                .artifact
                .as_ref()
                .map(|path| -> Result<String> {
                    let absolute = if path.is_absolute() {
                        path.clone()
                    } else {
                        std::env::current_dir()?.join(path)
                    };
                    Ok(absolute
                        .to_str()
                        .context("invalid_request: non-UTF8 artifact path")?
                        .to_owned())
                })
                .transpose()?;
            (
                "producer_pickup",
                &args.workspace,
                serde_json::json!({
                    "version": 1,
                    "producer": args.producer,
                    "artifact": artifact,
                    "clear": args.clear,
                }),
                false,
            )
        }
        Command::Semantic(SemanticCommand {
            action: SemanticAction::Import(args),
        }) => {
            let anchor = |path: &PathBuf| -> Result<String> {
                let absolute = if path.is_absolute() {
                    path.clone()
                } else {
                    std::env::current_dir()?.join(path)
                };
                Ok(absolute
                    .to_str()
                    .context("invalid_request: non-UTF8 input path")?
                    .to_owned())
            };
            ensure!(
                args.batch_sha256.len() == 64
                    && args
                        .batch_sha256
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "invalid_request: batch digest"
            );
            (
                "semantic_import",
                &args.workspace,
                serde_json::json!({
                    "version":2,"descriptor":anchor(&args.batch)?,"descriptorSha256":args.batch_sha256
                }),
                false,
            )
        }
        _ => unreachable!(),
    };
    let (_roots, identity) = workspace.resolve_unattached()?;
    if let Command::Metrics(args) = command
        && args.metrics.is_none()
    {
        payload
            .as_object_mut()
            .expect("metrics args object")
            .remove("metrics");
    }
    if let Command::Index(args) = command {
        ensure!(
            (1..=16_777_216).contains(&args.max_file_bytes.unwrap_or(2_097_152)),
            "max-file-bytes must be 1..16777216"
        );
        if args.max_file_bytes.is_none() {
            payload
                .as_object_mut()
                .expect("index args object")
                .remove("maxFileBytes");
        }
    }
    let request = daemon_request(
        operation,
        serde_json::json!({
            "workspace": identity.root, "args": payload,
        }),
    );
    let value = send_daemon(&request, read_only)?;
    if let Command::Metrics(args) = command {
        if args.json {
            print_json(&value)?;
        } else {
            let payload = &value["payload"];
            println!(
                "metrics {}:{} ({}, {}, role={}, catchingUp={})",
                payload["indexGeneration"].as_str().unwrap_or("?"),
                payload["indexRevision"],
                payload["scope"].as_str().unwrap_or("?"),
                payload["aggregate"].as_str().unwrap_or("?"),
                payload["role"].as_str().unwrap_or("?"),
                value["catchingUp"]
            );
            for kind in ["declarations", "files", "directories"] {
                if let Some(rows) = payload[kind].as_array() {
                    for row in rows {
                        println!(
                            "{} {} {} {} {}",
                            kind,
                            row["sourceSetId"].as_str().unwrap_or("?"),
                            row["path"].as_str().unwrap_or("?"),
                            row["sid"].as_str().unwrap_or(""),
                            row["metrics"]
                        );
                    }
                }
            }
        }
    } else {
        print_json(&value)?;
    }
    if let Command::Producer(ProducerCommand {
        action: ProducerAction::Pickup(args),
    }) = command
        && args.artifact.is_some()
        && value["state"] != "done"
    {
        anyhow::bail!(
            "{}: producer pickup {}",
            value["errorCode"].as_str().unwrap_or("index_failed"),
            value["state"].as_str().unwrap_or("failed")
        );
    }
    if let Command::Producer(ProducerCommand {
        action: ProducerAction::Run(_),
    }) = command
        && value["state"] != "done"
    {
        anyhow::bail!(
            "{}: producer run {}",
            value["errorCode"].as_str().unwrap_or("producer_failed"),
            value["state"].as_str().unwrap_or("failed")
        );
    }
    if matches!(command, Command::Rules(_) | Command::References(_)) && value.get("error").is_some()
    {
        anyhow::bail!("architecture evaluation failed");
    }
    if matches!(command, Command::Rules(_))
        && value["payload"]["counts"]["deviates"].as_u64().unwrap_or(0) > 0
    {
        anyhow::bail!("architecture deviations");
    }
    Ok(())
}

fn architecture_cli_arguments(
    pin: Option<&str>,
    paths: &[String],
    limit: Option<usize>,
) -> Result<serde_json::Value> {
    let mut arguments = serde_json::json!({"paths":paths});
    if let Some(limit) = limit {
        ensure!(
            (1..=50).contains(&limit),
            "invalid_argument: limit must be 1..50"
        );
        arguments["limit"] = serde_json::json!(limit);
    }
    if let Some(pin) = pin {
        let (generation, revision) = pin
            .rsplit_once(':')
            .context("invalid_argument: pin requires generation:revision")?;
        ensure!(
            !revision.is_empty() && revision.bytes().all(|b| b.is_ascii_digit()),
            "invalid_argument: revision"
        );
        let pin: trellis::model::IndexPin = serde_json::from_value(serde_json::json!({
            "indexGeneration":generation,
            "indexRevision":revision.parse::<u64>().context("invalid_argument: revision")?,
        }))
        .context("invalid_argument: pin")?;
        arguments["expectedBasis"] = serde_json::to_value(pin)?;
    }
    Ok(arguments)
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
    let search = Arc::new(daemon::search::SearchService::from_environment(
        TopologyRoots::production()?,
    )?);
    owner.listener().set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(owner.listener().try_clone()?)?;
    let search_fill = daemon::search_fill::SearchFill::start(search.clone())?;
    let startup_search = search.model_configured();
    #[cfg(feature = "test-support")]
    let startup_search =
        startup_search || std::env::var_os("TRELLIS_TEST_SEARCH_STARTUP_SOCKET").is_some();
    if startup_search {
        search_fill.queue_startup(TopologyRoots::production()?);
    }
    let mut checkouts = registry::CheckoutRegistry::new();
    checkouts.set_search_fill(search_fill);
    checkouts.set_search_service(search.clone());
    let registry = Arc::new(tokio::sync::Mutex::new(checkouts));
    #[cfg(feature = "test-support")]
    if let Some(path) = std::env::var_os("TRELLIS_TEST_DAEMON_STARTUP_SOCKET") {
        registry
            .lock()
            .await
            .set_next_startup_hook_for_tests(Arc::new(move || {
                use std::io::{Read, Write};
                let mut stream = std::os::unix::net::UnixStream::connect(&path)
                    .expect("connect daemon startup fixture");
                stream
                    .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
                    .unwrap();
                stream
                    .set_write_timeout(Some(trellis::testing::HANG_BACKSTOP))
                    .unwrap();
                stream
                    .write_all(&[1])
                    .expect("report daemon startup fixture arrival");
                stream
                    .read_exact(&mut [0])
                    .expect("release daemon startup fixture");
            }));
    }
    let browser = Arc::new(tokio::sync::Mutex::new(daemon::BrowserProvisioner::new()));
    let next_session = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let (browser_shutdown, _) = tokio::sync::watch::channel(false);
    let idle = daemon::run_idle_lifecycle(registry.clone());
    tokio::pin!(idle);
    // A test-elected daemon ends with the test process that owns it (#130),
    // even when the test never stops it or is killed before its guards run.
    let owner_gone = async {
        #[cfg(feature = "test-support")]
        if let Some(pid) = std::env::var(trellis::testing::OWNER_PID_ENV)
            .ok()
            .and_then(|pid| pid.parse::<libc::pid_t>().ok())
            .filter(|pid| *pid > 1)
        {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if !trellis::testing::process_running(pid) {
                    return;
                }
            }
        }
        std::future::pending::<()>().await
    };
    tokio::pin!(owner_gone);
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
            () = &mut owner_gone => break,
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
                let accepted_at = Instant::now();
                let stream = stream.into_std()?;
                stream.set_nonblocking(false)?;
                let registry = registry.clone();
                let browser = browser.clone();
                let search = search.clone();
                let id = next_session.fetch_add(1, Ordering::Relaxed);
                let browser_shutdown = browser_shutdown.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = dispatch_connection(stream, id, registry, browser, search, browser_shutdown, accepted_at);
                });
            }
        }
    }
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
        omitted.as_object_mut().unwrap().remove("maxFileBytes");
        let absent = checked_serve_options(omitted).unwrap();
        let explicit = checked_serve_options(options).unwrap();
        assert!(absent.0.get("maxFileBytes").is_none());
        assert_eq!(explicit.0["maxFileBytes"], 2_097_152);
        assert!(explicit.0.get("scip").is_none());
        assert!(explicit.0.get("manifest").is_none());
    }
}

fn dispatch_connection(
    mut stream: std::os::unix::net::UnixStream,
    session: u64,
    registry: Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
    browser: Arc<tokio::sync::Mutex<daemon::BrowserProvisioner>>,
    search: Arc<daemon::search::SearchService>,
    browser_shutdown: tokio::sync::watch::Sender<bool>,
    accepted_at: Instant,
) -> Result<()> {
    let runtime = tokio::runtime::Handle::current();
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    use std::io::{Cursor, Read};
    let mut first = [0u8; 1];
    stream.read_exact(&mut first)?;
    let prefaced = first[0] == 0xff;
    let ingress_deadline = accepted_at + Duration::from_secs(20);
    #[cfg(feature = "test-support")]
    let ingress_deadline = std::env::var("TRELLIS_TEST_METRICS_INGRESS_DEADLINE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|milliseconds| accepted_at + Duration::from_millis(milliseconds))
        .unwrap_or(ingress_deadline);
    let request: protocol::Request = if prefaced {
        protocol::read_prefaced_frame_until(&mut stream, ingress_deadline)?
    } else {
        protocol::read_frame(&mut Cursor::new(first).chain(&mut stream))?
    };
    if prefaced {
        ensure!(
            request.operation == "metrics",
            "invalid prefaced daemon operation"
        );
        #[cfg(feature = "test-support")]
        if let Some(delay) = metrics_test_duration("TRELLIS_TEST_METRICS_ACK_DELAY_MS") {
            std::thread::sleep(delay);
        }
        protocol::write_frame_until(
            &mut stream,
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
            request.operation != "metrics",
            "metrics admission preface required"
        );
    }
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
                    let captured = WorkspaceIdentity::discover_unattached(
                        Some(&attachment.root),
                        &attachment.root,
                    )
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
        let result = search.gc_report();
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
    // One deadline for the whole CLI read (#142): activation contention, the
    // wait for startup reconciliation and every SQLite busy wait share it, so
    // they cannot stack. When it passes the read fails typed.
    let metrics = request.operation == "metrics";
    let metrics_work = if matches!(
        request.payload["args"]["scope"].as_str(),
        Some("revision" | "directory")
    ) {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(30)
    };
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
    let request_deadline = Instant::now() + if metrics { metrics_work } else { CLI_READ_WAIT };
    let read_scope = matches!(
        request.operation.as_str(),
        "status"
            | "symbols"
            | "query"
            | "metrics"
            | "rules_check"
            | "references"
            | "producer_plan"
            | "producer_status"
    );
    let run = || -> Result<serde_json::Value> {
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
                "status"
                    | "symbols"
                    | "query"
                    | "index"
                    | "semantic_import"
                    | "metrics"
                    | "rules_check"
                    | "references"
                    | "producer_plan"
                    | "producer_status"
                    | "producer_pickup"
                    | "producer_run"
                    | "producer_cancel"
            ),
            "unknown daemon operation"
        );
        let metrics_options = if metrics {
            Some(metrics_options_from_request(
                request
                    .payload
                    .get("args")
                    .context("invalid_argument: missing args")?,
            )?)
        } else {
            None
        };
        let identity = if matches!(
            request.operation.as_str(),
            "status" | "producer_plan" | "producer_status"
        ) {
            identity.attach_existing_marker_readonly()?
        } else {
            identity.attach_marker()?
        };
        // Virgin queue publication can contend with a short-lived index-use
        // reader before any request ID has been accepted. Retry only that
        // typed pre-acceptance admission; never replay an enqueue after COMMIT.
        let activation_deadline = request_deadline;
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
        let deadline = request_deadline;
        loop {
            // Recorded before the read: a state change between the read and
            // the wait below is never missed.
            let state_version = checkout.state_version();
            let selected = match request.operation.as_str() {
                "status" => checkout.cli_status(),
                "producer_plan" => checkout.cli_producer_plan(args),
                "producer_status" => checkout.cli_producer_status(args),
                "producer_pickup" => checkout.cli_producer_pickup(args),
                "producer_run" => checkout.cli_producer_run(args),
                "producer_cancel" => checkout.cli_producer_cancel(args),
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
                    semantic_producer_id: serde_json::from_value(
                        args.get("semanticProducerId").cloned().unwrap_or_default(),
                    )
                    .context("invalid_argument: semanticProducerId must be a string")?,
                }),
                "rules_check" | "references" => (|| {
                    let (response, catching_up) = checkout.evidence_response()?;
                    let kind = if request.operation == "references" {
                        "references"
                    } else {
                        "check"
                    };
                    let mut check = || {
                        ensure!(Instant::now() < request_deadline, "deadline_exceeded");
                        Ok(())
                    };
                    let result = response.architecture_response(kind, args, &mut check);
                    match result {
                        Ok(payload) => {
                            check()?;
                            let value =
                                serde_json::json!({"catchingUp":catching_up,"payload":payload});
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
                })(),
                "metrics" => {
                    let options = metrics_options.as_ref().expect("metrics options validated");
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
                            while !gate.join("release").exists()
                                && Instant::now() < request_deadline
                            {
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
                    let serialize_threshold =
                        std::env::var("TRELLIS_TEST_METRICS_SERIALIZE_DELAY_AT_BYTES")
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
                            if let Ok(path) =
                                std::env::var("TRELLIS_TEST_METRICS_SERIALIZE_HIT_FILE")
                            {
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
                }
                "index" => {
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
                    checkout.cli_index(&options)
                }
                "semantic_import" => {
                    let options: trellis::store::requests::SemanticImportOptions =
                        serde_json::from_value(args.clone()).map_err(|_| {
                            anyhow::anyhow!("invalid_request: semantic import options")
                        })?;
                    checkout.cli_semantic_import(&options)
                }
                _ => anyhow::bail!("unknown daemon operation"),
            };
            // Mandatory startup reconciliation may be running when a CLI read first attaches.
            // This only repeats read-only calls; index is never replayed.
            if !matches!(
                request.operation.as_str(),
                "index"
                    | "semantic_import"
                    | "producer_pickup"
                    | "producer_run"
                    | "producer_cancel"
            ) && selected
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
    };
    let result = if read_scope {
        trellis::store::with_read_deadline(request_deadline, run)
    } else {
        run()
    };
    if !serve {
        runtime.block_on(async { registry.lock().await.disconnect(session) });
    }
    #[cfg(feature = "test-support")]
    if metrics
        && result.as_ref().err().is_some_and(|error| {
            let message = error.to_string();
            message.contains("deadline_exceeded") || message.contains("cancelled")
        })
        && let Some(delay) = metrics_test_duration_file("TRELLIS_TEST_METRICS_ERROR_DELAY_MS_FILE")
            .or_else(|| metrics_test_duration("TRELLIS_TEST_METRICS_ERROR_DELAY_MS"))
    {
        std::thread::sleep(delay);
    }
    let expired_success = metrics && result.is_ok() && Instant::now() >= request_deadline;
    let reply = protocol::Reply {
        id: request.id,
        payload: match result {
            Ok(_) if expired_success => serde_json::json!({"error": "deadline_exceeded"}),
            Ok(value) => serde_json::json!({"result": value}),
            Err(error) => serde_json::json!({"error": format!("{error:#}")}),
        },
    };
    if metrics {
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
        write_daemon_reply_until(&mut stream, &reply, transfer_deadline)?;
    } else {
        write_daemon_reply(&mut stream, &reply)?;
    }
    if serve {
        // This control connection only observes the daemon process lifetime:
        // it ends when the client closes it or the daemon stops, signalled by
        // the shutdown channel instead of a polled flag (#138 item 21).
        let closer = stream.try_clone()?;
        let mut shutdown = browser_shutdown.subscribe();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<()>();
        let watcher = runtime.spawn(async move {
            tokio::select! {
                _ = shutdown.wait_for(|stopped| *stopped) => {
                    let _ = closer.shutdown(std::net::Shutdown::Both);
                }
                _ = closed_rx => {}
            }
        });
        stream.set_read_timeout(None)?;
        let mut one = [0u8; 1];
        use std::io::Read;
        while matches!(stream.read(&mut one), Ok(n) if n > 0) {}
        let _ = closed_tx.send(());
        let _ = runtime.block_on(watcher);
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

#[cfg(feature = "test-support")]
fn metrics_test_duration(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
}

#[cfg(feature = "test-support")]
fn metrics_test_duration_file(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
}

#[cfg(feature = "test-support")]
fn metrics_test_phase(mark: &[u8]) {
    use std::io::Write;
    if let Ok(path) = std::env::var("TRELLIS_TEST_METRICS_PHASE_ORDER_FILE")
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = file.write_all(mark);
    }
}

fn result_is_deadline(payload: &serde_json::Value) -> bool {
    payload["error"]
        .as_str()
        .is_some_and(|value| value.contains("deadline_exceeded") || value.contains("cancelled"))
}

fn metrics_options_from_request(
    args: &serde_json::Value,
) -> Result<trellis::store::MetricsOptions> {
    fn field<T: serde::de::DeserializeOwned>(args: &serde_json::Value, name: &str) -> Result<T> {
        serde_json::from_value(args[name].clone())
            .map_err(|error| anyhow::anyhow!("invalid_argument: {name}: {error}"))
    }
    let fields = args
        .as_object()
        .context("invalid_argument: metrics args object")?;
    ensure!(
        fields.keys().all(|key| matches!(
            key.as_str(),
            "scope"
                | "aggregate"
                | "role"
                | "sourceSetId"
                | "language"
                | "path"
                | "sid"
                | "metrics"
                | "pin"
        )),
        "invalid_argument: unknown metrics option"
    );
    ensure!(
        fields
            .get("metrics")
            .is_none_or(|value| value.as_array().is_some_and(|names| !names.is_empty())),
        "invalid_argument: empty metrics selection"
    );
    let options = trellis::store::MetricsOptions {
        workbench: false,
        scope: args["scope"]
            .as_str()
            .context("invalid_argument: scope")?
            .to_owned(),
        aggregate: args["aggregate"]
            .as_str()
            .context("invalid_argument: aggregate")?
            .to_owned(),
        role: if fields.contains_key("role") {
            field(args, "role")?
        } else {
            "all".into()
        },
        source_set_id: field(args, "sourceSetId")?,
        language: field(args, "language")?,
        path: field(args, "path")?,
        sid: field(args, "sid")?,
        metrics: if args.get("metrics").is_some() {
            field(args, "metrics")?
        } else {
            vec![]
        },
        pin: field(args, "pin")?,
        top_cognitive: None,
    };
    options
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid_argument: {error}"))?;
    ensure!(
        options.source_set_id.as_deref().is_none_or(|id| {
            id.starts_with("source-set:v1:path-")
                && id.len() == 83
                && id[19..]
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        }),
        "invalid_argument: source set ID"
    );
    ensure!(
        options.sid.as_deref().is_none_or(|sid| {
            sid.starts_with("sid:v1:")
                && sid.len() == 39
                && sid[7..]
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        }),
        "invalid_argument: sid"
    );
    ensure!(
        options
            .path
            .as_deref()
            .is_none_or(|path| !path.contains([':', '\\', '\0'])),
        "invalid_argument: path"
    );
    ensure!(
        options
            .language
            .as_deref()
            .is_none_or(|language| matches!(language, "java" | "python" | "javascript" | "rust")),
        "invalid_argument: language"
    );
    Ok(options)
}

fn write_daemon_reply_until(
    stream: &mut std::os::unix::net::UnixStream,
    reply: &protocol::Reply,
    deadline: Instant,
) -> Result<()> {
    // The selected response was fully materialized and fenced before transfer.
    struct TransferBytes {
        bytes: Vec<u8>,
        deadline: Instant,
    }
    impl std::io::Write for TransferBytes {
        fn write(&mut self, part: &[u8]) -> std::io::Result<usize> {
            if Instant::now() >= self.deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "metrics transfer deadline exceeded",
                ));
            }
            if part.len() > (64 * 1024 * 1024 + 4096_usize).saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "metrics transfer too large",
                ));
            }
            self.bytes.extend_from_slice(part);
            if Instant::now() >= self.deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "metrics transfer deadline exceeded",
                ));
            }
            Ok(part.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut serialized = TransferBytes {
        bytes: Vec::new(),
        deadline,
    };
    serde_json::to_writer(&mut serialized, &reply.payload)?;
    let payload = serialized.bytes;
    if Instant::now() >= deadline {
        anyhow::bail!("deadline_exceeded");
    }
    const CHUNK_BYTES: usize = 450_000;
    let chunk_bytes = CHUNK_BYTES;
    #[cfg(feature = "test-support")]
    let forced_chunks = std::env::var("TRELLIS_TEST_METRICS_FORCE_CHUNK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=CHUNK_BYTES).contains(value));
    #[cfg(feature = "test-support")]
    let chunk_bytes = forced_chunks.unwrap_or(chunk_bytes);
    #[cfg(feature = "test-support")]
    let last_chunk_delay = metrics_test_duration("TRELLIS_TEST_METRICS_LAST_CHUNK_DELAY_MS");
    #[cfg(feature = "test-support")]
    let small_frame = forced_chunks.is_none() && payload.len() + 64 <= protocol::MAX_FRAME;
    #[cfg(not(feature = "test-support"))]
    let small_frame = payload.len() + 64 <= protocol::MAX_FRAME;
    if small_frame {
        protocol::write_frame_until(stream, reply, deadline)?;
    } else {
        for (index, chunk) in payload.chunks(chunk_bytes).enumerate() {
            let more = (index + 1) * chunk_bytes < payload.len();
            #[cfg(feature = "test-support")]
            if !more && let Some(delay) = last_chunk_delay {
                std::thread::sleep(delay.min(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(100),
                ));
            }
            protocol::write_frame_until(
                stream,
                &protocol::Reply {
                    id: reply.id,
                    payload: serde_json::json!({"chunk": hex::encode(chunk), "more": more}),
                },
                deadline,
            )?;
            #[cfg(feature = "test-support")]
            if let Ok(path) = std::env::var("TRELLIS_TEST_METRICS_CHUNK_COUNT_FILE") {
                use std::io::Write;
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = file.write_all(b"x");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod metrics_request_tests {
    use super::metrics_options_from_request;
    use serde_json::json;

    #[test]
    fn role_selection_defaults_and_strict_daemon_option_validation() {
        let args = json!({"scope": "revision", "aggregate": "sum"});
        assert_eq!(metrics_options_from_request(&args).unwrap().role, "all");
        for role in ["production", "all", "test"] {
            let mut selected = args.clone();
            selected["role"] = json!(role);
            assert_eq!(metrics_options_from_request(&selected).unwrap().role, role);
        }
        for role in [
            json!(""),
            json!("unknown"),
            json!("Production"),
            json!(null),
            json!(true),
            json!(["all"]),
        ] {
            let mut invalid = args.clone();
            invalid["role"] = role;
            let error = metrics_options_from_request(&invalid).unwrap_err();
            assert!(error.to_string().contains("invalid_argument"), "{error}");
        }
        let mut unknown = args;
        unknown["roles"] = json!("production");
        assert!(
            metrics_options_from_request(&unknown)
                .unwrap_err()
                .to_string()
                .contains("unknown metrics option")
        );
    }
}
