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
    about = "Local, read-only Java, Python, JavaScript, TypeScript and Rust code index and inspection daemon"
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
    /// Wait until a checkout's watcher, FIFO and semantic catch-up are idle (#720).
    #[cfg(feature = "test-support")]
    #[command(hide = true)]
    TestSettled(TestSettledArgs),
    /// Build and atomically publish a complete index. Does not execute repository code.
    Index(IndexArgs),
    /// Explicitly import pinned SCIP evidence into the selected native index.
    Semantic(SemanticCommand),
    /// Plan and inspect headless semantic producers. Planning executes nothing.
    Producer(ProducerCommand),
    /// Run the user-level socket service, or stop the existing daemon.
    Daemon(DaemonArgs),
    /// Serve one read-only MCP connection on line-framed stdin/stdout.
    Mcp(WorkspaceArgs),
    /// Serve the authenticated loopback API and browser inspector. Indexing starts on first use; the watcher then keeps the index current.
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
    /// Explicitly install, inspect or remove pinned local search models.
    Model(ModelArgs),
    /// Inspect existing derived indexes and the vector cache without changing them.
    Gc(GcArgs),
    /// Read one allowlisted browser API route through the local daemon, as the Herdr mod does.
    Api(ApiArgs),
}
/// `trellis api` (#817). Prints the daemon's JSON answer on stdout. Exit 0 for a 2xx answer,
/// 1 for a daemon error answer (printed unchanged), 2 for a typed failure with no answer
/// (`{"error":{"code","message"}}`): a refused route, a bad body or the daemon unreachable.
#[derive(Args)]
struct ApiArgs {
    /// GET, or POST with --body.
    #[arg(value_parser = ["GET", "POST"])]
    method: String,
    /// The route and query, for example /api/checkouts.
    target: String,
    /// The POST body: JSON, at most 64 KiB, or `-` to read it from stdin.
    #[arg(long, allow_hyphen_values = true)]
    body: Option<String>,
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
struct TestSettledArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    /// The published head to settle at, as `trellis status` prints `revision`.
    #[arg(long)]
    at: Option<String>,
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
struct DaemonArgs {
    #[command(subcommand)]
    command: Option<DaemonCommand>,
}
#[derive(Subcommand)]
enum DaemonCommand {
    /// Stop the per-user daemon across all checkouts. Does not start one.
    Stop,
}
#[derive(Args)]
struct ModelArgs {
    #[command(subcommand)]
    command: ModelCommand,
}
#[derive(Subcommand)]
enum ModelCommand {
    /// Download and verify the reviewed pinned model. Nothing downloads implicitly.
    Install {
        name: Option<String>,
        #[arg(long)]
        revision: Option<String>,
    },
    /// List managed snapshots and effective settings without starting a daemon.
    List,
    /// Delete one managed snapshot. This does not restart a running daemon.
    Remove {
        name: String,
        #[arg(long)]
        revision: String,
    },
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
/// A rules check or references read can wait for the revision's architecture core (#556).
const CLI_ARCHITECTURE_WAIT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    // Before any SQLite connection opens (#673).
    trellis::sqlite_config::disable_memory_statistics();
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
    if let Command::Daemon(args) = &command {
        return match args.command {
            None => run_daemon().await,
            Some(DaemonCommand::Stop) => print_json(&stop_existing_daemon()?),
        };
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
        Command::Daemon(_) | Command::Serve(_) | Command::GitRead(_) => unreachable!(),
        #[cfg(feature = "test-support")]
        command @ Command::TestSettled(_) => dispatch_cli(&command)?,
        Command::Mcp(args) => {
            let (_, identity) = args.resolve_unattached()?;
            let socket = socket_path()?;
            let stream = client::connect_or_start(&socket, start_daemon, Duration::from_secs(5))
                .map_err(|_| anyhow::anyhow!("daemon_unavailable"))?;
            client::relay_stdio(stream, &identity, || {
                client::connect_or_start(&socket, start_daemon, Duration::from_secs(5))
            })?;
        }
        Command::Model(args) => model_command(args).await?,
        Command::Api(args) => std::process::exit(api_command(&args)),
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

async fn model_command(args: ModelArgs) -> Result<()> {
    let roots = TopologyRoots::production()?;
    let resolved = trellis::search_config::resolve(&roots)?;
    match args.command {
        ModelCommand::Install { name, revision } => {
            let name = name.unwrap_or_else(|| resolved.diagnostics.model_name.clone());
            let revision = revision.unwrap_or_else(|| resolved.diagnostics.model_revision.clone());
            let report = trellis::model_management::install(
                &resolved.diagnostics.models_dir,
                &name,
                &revision,
            )
            .await?;
            print_json(&report)?;
        }
        ModelCommand::List => {
            let installed = trellis::model_management::list(&resolved.diagnostics.models_dir)?;
            print_json(&serde_json::json!({
                "configuration": resolved.diagnostics,
                "installed": installed,
                "appliesTo": "next_daemon_start",
            }))?;
        }
        ModelCommand::Remove { name, revision } => {
            let report = trellis::model_management::remove(
                &resolved.diagnostics.models_dir,
                &name,
                &revision,
            )?;
            print_json(&report)?;
        }
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

/// `trellis api`: refuses before reaching the daemon, so a refused route starts nothing.
fn api_command(args: &ApiArgs) -> i32 {
    use std::io::{Read, Write};
    let outcome = (|| -> Result<(u16, serde_json::Value)> {
        let body = match args.body.as_deref() {
            None => None,
            Some(text) if text != "-" => {
                let limit = trellis::http::CLI_API_BODY_LIMIT;
                ensure!(
                    text.len() <= limit,
                    "body_too_large: the request body exceeds {limit} bytes"
                );
                Some(text.to_owned())
            }
            Some(_) => {
                let limit = trellis::http::CLI_API_BODY_LIMIT;
                let mut bytes = Vec::new();
                std::io::stdin()
                    .lock()
                    .take(limit as u64 + 1)
                    .read_to_end(&mut bytes)
                    .context("invalid_request: cannot read the body from stdin")?;
                ensure!(
                    bytes.len() <= limit,
                    "body_too_large: the request body exceeds {limit} bytes"
                );
                Some(String::from_utf8(bytes).context("invalid_request: the body is not UTF-8")?)
            }
        };
        trellis::http::cli_api_route(&args.method, &args.target, body.is_some())?;
        let reply = send_daemon(
            &daemon_request(
                "api",
                serde_json::json!({"method": args.method, "target": args.target, "body": body}),
            ),
            true,
        )
        .map_err(|error| {
            // A daemon started before `api` existed reads it as a checkout operation.
            if error.to_string() == "missing workspace" {
                anyhow::anyhow!(
                    "daemon_outdated: the running daemon predates trellis api; restart it (trellis daemon stop, then trellis serve)"
                )
            } else {
                error
            }
        })?;
        let status = reply["status"]
            .as_u64()
            .filter(|status| (100..600).contains(status))
            .context("invalid_response: the daemon's answer has no status")?;
        Ok((status as u16, reply["body"].clone()))
    })();
    let (value, code) = match outcome {
        Ok((status, body)) => (body, i32::from(!(200..300).contains(&status))),
        Err(error) => {
            let text = format!("{error:#}");
            let (code, message) = match text.split_once(": ") {
                Some((code, message))
                    if !code.is_empty()
                        && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
                {
                    (code.to_owned(), message.to_owned())
                }
                _ if text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') => {
                    (text.clone(), format!("The daemon call failed ({text})."))
                }
                _ => ("api_failed".to_owned(), text),
            };
            (
                serde_json::json!({"error": {"code": code, "message": message}}),
                2,
            )
        }
    };
    let mut stdout = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut stdout, &value);
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
    code
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

/// How long `trellis api` waits on a silent daemon, per socket read: below the 30 s a Herdr
/// `exec` waits by default, so a slow answer is the CLI's typed failure, not a killed command.
const CLI_API_REPLY_BOUND: Duration = Duration::from_secs(25);

/// How long other non-metrics CLI calls wait on a silent daemon, per socket
/// read. Daemon stop waits for ownership release without this completion bound.
/// Tests lower it with `TRELLIS_TEST_CLI_REPLY_BOUND_MS` to prove
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
        "index" | "semantic_import" | "producer_pickup"
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
        #[cfg(feature = "test-support")]
        Command::TestSettled(args) => {
            let at = args
                .at
                .as_deref()
                .map(serde_json::from_str::<serde_json::Value>)
                .transpose()
                .context("invalid_argument: --at is not a revision")?;
            (
                "settled_for_tests",
                &args.workspace,
                serde_json::json!({ "at": at }),
                true,
            )
        }
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
                    "java" | "python" | "javascript" | "typescript" | "rust"
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

fn serve_control_failure(result: std::io::Result<usize>) -> anyhow::Error {
    match result {
        Ok(0) => anyhow::anyhow!(
            "daemon_unavailable: daemon control connection closed (EOF); daemon exit status unavailable"
        ),
        Ok(_) => anyhow::anyhow!("daemon_unavailable: unexpected data on serve control connection"),
        Err(error) => anyhow::anyhow!("daemon_unavailable: serve control read failed: {error}"),
    }
}

fn stop_existing_daemon() -> Result<serde_json::Value> {
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
    checked_reply(reply)
}

struct DaemonStop {
    session: u64,
    request: u64,
    stream: std::os::unix::net::UnixStream,
}

struct DaemonControl {
    browser_shutdown: tokio::sync::watch::Sender<bool>,
    stop_sender: tokio::sync::mpsc::UnboundedSender<DaemonStop>,
}

async fn run_daemon() -> Result<()> {
    let paths = daemon::SocketPaths::new(&TopologyRoots::production()?.data);
    let Some(owner) = daemon::SocketOwner::acquire(&paths)? else {
        return Ok(());
    };
    // Test-only ownership marker: only the elected daemon records its PID.
    #[cfg(feature = "test-support")]
    if let Some(path) = std::env::var_os("TRELLIS_TEST_DAEMON_PID_FILE") {
        trellis::testing::write_daemon_marker(std::path::Path::new(&path))?;
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
    // A private fixture socket moves the existing virtual lifecycle clock;
    // no production delay or daemon protocol is changed.
    #[cfg(feature = "test-support")]
    if let Some(path) = std::env::var_os("TRELLIS_TEST_DAEMON_CLOCK_SOCKET") {
        let registry = registry.clone();
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || -> Result<()> {
            use std::io::{Read, Write};
            let mut hook = std::os::unix::net::UnixStream::connect(path)?;
            hook.set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))?;
            let mut now = Instant::now();
            let mut command = [0];
            while hook.read_exact(&mut command).is_ok() {
                if command == *b"T" {
                    now += registry::DAEMON_IDLE_DELAY + Duration::from_secs(1);
                }
                let exit = runtime
                    .block_on(async {
                        let mut registry = registry.lock().await;
                        registry.set_clock_override_for_tests(now);
                        registry.advance(now).map(|tick| tick.exit)
                    })
                    .map_err(|error| anyhow::anyhow!("{}", error.reason()))?;
                hook.write_all(&[u8::from(exit)])?;
            }
            Ok(())
        });
    }
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
    let mut connections = tokio::task::JoinSet::new();
    let mut sockets = std::collections::HashMap::new();
    let mut task_sessions = std::collections::HashMap::new();
    let (stop_sender, mut stop_receiver) = tokio::sync::mpsc::unbounded_channel::<DaemonStop>();
    let mut idle = Box::pin(daemon::run_idle_lifecycle(registry.clone()));
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
                    if let Some(path) = std::env::var_os("TRELLIS_TEST_DAEMON_OWNER_GONE_SOCKET") {
                        let _observed = std::os::unix::net::UnixStream::connect(path)
                            .expect("connect owner-exit observation fixture");
                    }
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
    let stop = loop {
        tokio::select! {
            stop = stop_receiver.recv() => break stop,
            result = &mut idle => { result.map_err(|e| anyhow::anyhow!("{}", e.reason()))?; break None; }
            () = &mut owner_gone => break None,
            completed = connections.join_next_with_id(), if !connections.is_empty() => {
                if let Some(completed) = completed {
                    let task = match completed {
                        Ok((task, _)) => task,
                        Err(error) => error.id(),
                    };
                    if let Some(session) = task_sessions.remove(&task) {
                        sockets.remove(&session);
                    }
                }
            }
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
                let control = DaemonControl {
                    browser_shutdown: browser_shutdown.clone(),
                    stop_sender: stop_sender.clone(),
                };
                sockets.insert(id, stream.try_clone()?);
                let task = connections.spawn_blocking(move || {
                    let _ = dispatch_connection(stream, id, registry, browser, search, control, accepted_at);
                });
                task_sessions.insert(task.id(), id);
            }
        }
    };
    drop(idle);
    if let Some(stop) = &stop {
        sockets.remove(&stop.session);
    }
    // Every connection read (request ingress, idle MCP readers and the serve
    // control wait) waits on this signal together with its socket's
    // readability, so publishing it ends them; aborting a spawn_blocking task
    // would leave those readers running. Darwin's shutdown(2) on a cloned
    // descriptor can miss a thread entering read(2) (#739, #741), so no read
    // depends on the socket shutdown below. It still makes a stopping daemon's
    // remaining writes fail fast instead of answering after the stop, and it
    // gives each peer end of input before the daemon exits.
    browser_shutdown.send_replace(true);
    for stream in sockets.values() {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    if stop.is_some() {
        registry::CheckoutRegistry::cancel_producers(registry.clone()).await?;
    }
    stop_daemon_connections(&browser_shutdown, &mut connections).await;
    browser.lock().await.finish_owned().await?;
    if stop.is_some() {
        registry::CheckoutRegistry::shutdown(registry.clone()).await?;
    }
    drop(browser);
    drop(registry);
    drop(listener);
    drop(owner);
    if let Some(mut stop) = stop {
        // Acknowledgment proves this elected owner released its socket/lock.
        // Another client may already be starting a successor.
        stop.stream.set_write_timeout(Some(CLI_REPLY_BOUND))?;
        protocol::write_frame(
            &mut stop.stream,
            &protocol::Reply {
                id: stop.request,
                payload: serde_json::json!({"result": {"stopped": true}}),
            },
        )?;
    }
    Ok(())
}

async fn stop_daemon_connections<T: 'static>(
    shutdown: &tokio::sync::watch::Sender<bool>,
    connections: &mut tokio::task::JoinSet<T>,
) {
    shutdown.send_replace(true);
    while connections.join_next().await.is_some() {}
}

#[cfg(test)]
mod daemon_shutdown_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct ControlPeer(std::os::unix::net::UnixStream);
    impl Drop for ControlPeer {
        fn drop(&mut self) {
            let _ = self.0.shutdown(std::net::Shutdown::Both);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_is_retained_for_a_late_serve_subscriber() {
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        drop(receiver);
        let (control, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let _peer = ControlPeer(peer);
        let mut connections = tokio::task::JoinSet::new();
        let (release, admitted) = std::sync::mpsc::channel();
        let (observed, observation) = tokio::sync::oneshot::channel();
        let runtime = tokio::runtime::Handle::current();
        let late_shutdown = shutdown.clone();
        connections.spawn_blocking(move || {
            admitted.recv().unwrap();
            let stopped = late_shutdown.subscribe();
            let retained = *stopped.borrow();
            if retained {
                wait_for_serve_disconnect(control, stopped, &runtime).unwrap();
            }
            let _ = observed.send(retained);
        });
        // This current-thread task runs only after shutdown publishes its value
        // and yields while draining the accepted dispatch.
        tokio::spawn(async move { release.send(()).unwrap() });
        stop_daemon_connections(&shutdown, &mut connections).await;
        assert!(
            observation.await.unwrap(),
            "daemon shutdown was lost before a serve control watcher subscribed"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retained_shutdown_cannot_race_serve_socket_configuration() {
        let (control, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let _peer = ControlPeer(peer);
        // The daemon's stop sweep can shut down an accepted socket before the
        // serve wait configures it (#625).
        control.shutdown(std::net::Shutdown::Both).unwrap();
        let (shutdown, stopped) = tokio::sync::watch::channel(true);
        let runtime = tokio::runtime::Handle::current();
        let result = tokio::task::spawn_blocking(move || {
            wait_for_serve_disconnect(control, stopped, &runtime)
        })
        .await
        .unwrap();
        assert!(
            result.is_ok(),
            "retained shutdown raced socket setup: {result:?}"
        );
        assert!(*shutdown.borrow());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serve_wait_ends_when_the_peer_disconnects() {
        let (control, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let (shutdown, stopped) = tokio::sync::watch::channel(false);
        let runtime = tokio::runtime::Handle::current();
        let (waiting, parked) = tokio::sync::oneshot::channel();
        let wait = tokio::task::spawn_blocking(move || {
            wait_for_serve_disconnect_hooked(control, stopped, &runtime, Some(waiting))
        });
        parked.await.unwrap();
        use std::io::Write;
        (&peer).write_all(b"ignored").unwrap();
        drop(peer);
        let result = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, wait)
            .await
            .expect("peer disconnect did not end the serve control wait")
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert!(!*shutdown.borrow());
    }

    /// #739: Darwin's `shutdown(2)` on a cloned descriptor can miss a thread
    /// entering a blocking `read(2)`, so the published shutdown signal must
    /// end the serve wait by itself. The peer stays open, and the control
    /// socket must still be usable afterwards, which proves the signal, not a
    /// socket shutdown, ended the wait.
    #[tokio::test(flavor = "current_thread")]
    async fn published_shutdown_ends_the_serve_wait_without_a_socket_shutdown() {
        let (control, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let probe = control.try_clone().unwrap();
        let (shutdown, stopped) = tokio::sync::watch::channel(false);
        let mut connections = tokio::task::JoinSet::new();
        let runtime = tokio::runtime::Handle::current();
        let (waiting, parked) = tokio::sync::oneshot::channel();
        connections.spawn_blocking(move || {
            wait_for_serve_disconnect_hooked(control, stopped, &runtime, Some(waiting)).unwrap();
        });
        tokio::time::timeout(trellis::testing::HANG_BACKSTOP, parked)
            .await
            .expect("serve wait never reached its read")
            .unwrap();
        tokio::time::timeout(
            trellis::testing::HANG_BACKSTOP,
            stop_daemon_connections(&shutdown, &mut connections),
        )
        .await
        .expect("published shutdown did not end the serve control wait");
        use std::io::{Read, Write};
        (&peer).write_all(&[7]).unwrap();
        let mut byte = [0u8; 1];
        probe.set_nonblocking(false).unwrap();
        (&probe).read_exact(&mut byte).unwrap();
        assert_eq!(
            byte,
            [7],
            "the serve wait shut its socket down instead of observing the signal"
        );
    }

    /// #741: the stop sweep ends an incomplete request by publishing shutdown,
    /// not by a cross-thread socket shutdown. The peer has sent part of a
    /// plain or a prefaced frame and stays open; the socket still carries data
    /// after the read ends, so no socket shutdown ended it.
    #[tokio::test(flavor = "current_thread")]
    async fn published_shutdown_ends_an_incomplete_request_without_a_socket_shutdown() {
        for partial in [&[0u8, 0, 0, 9, b'{'][..], &[0xff, 0, 0, 0, 9, b'{'][..]] {
            let (connection, peer) = std::os::unix::net::UnixStream::pair().unwrap();
            let probe = connection.try_clone().unwrap();
            use std::io::{Read, Write};
            (&peer).write_all(partial).unwrap();
            let (shutdown, stopped) = tokio::sync::watch::channel(false);
            let mut connections = tokio::task::JoinSet::new();
            let (parked, waiting) = std::sync::mpsc::channel();
            let (read, result) = tokio::sync::oneshot::channel();
            connections.spawn_blocking(move || {
                let request = read_daemon_request(
                    &connection,
                    stopped,
                    Instant::now() + trellis::testing::HANG_BACKSTOP,
                    Some(Box::new(move || {
                        let _ = parked.send(());
                    })),
                );
                let _ = read.send(request.map(|_| ()));
            });
            // The read parks only once the buffered partial frame is consumed.
            tokio::task::spawn_blocking(move || {
                waiting.recv_timeout(trellis::testing::HANG_BACKSTOP)
            })
            .await
            .unwrap()
            .expect("request read never parked");
            tokio::time::timeout(
                trellis::testing::HANG_BACKSTOP,
                stop_daemon_connections(&shutdown, &mut connections),
            )
            .await
            .expect("published shutdown did not end the incomplete request read");
            let error = result.await.unwrap().unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .map(std::io::Error::kind),
                Some(std::io::ErrorKind::UnexpectedEof),
                "{error:#}"
            );
            (&peer).write_all(&[7]).unwrap();
            let mut byte = [0u8; 1];
            (&probe).read_exact(&mut byte).unwrap();
            assert_eq!(byte, [7], "the request read was ended by a socket shutdown");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_finishes_the_serve_control_read_before_returning() {
        let (control, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let _peer = ControlPeer(peer);
        let (shutdown, stopped) = tokio::sync::watch::channel(false);
        let mut connections = tokio::task::JoinSet::new();
        let completed = Arc::new(AtomicBool::new(false));
        let finished = completed.clone();
        let runtime = tokio::runtime::Handle::current();
        let (started, ready) = tokio::sync::oneshot::channel();
        connections.spawn_blocking(move || {
            let _ = started.send(());
            wait_for_serve_disconnect(control, stopped, &runtime).unwrap();
            finished.store(true, Ordering::Release);
        });
        ready.await.unwrap();
        stop_daemon_connections(&shutdown, &mut connections).await;
        assert!(
            completed.load(Ordering::Acquire),
            "daemon shutdown returned before its serve control read finished"
        );
    }
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
    fn serve_control_failures_are_typed_and_preserve_read_errors() {
        for result in [Ok(0), Ok(1)] {
            assert!(
                serve_control_failure(result)
                    .to_string()
                    .starts_with("daemon_unavailable:")
            );
        }
        let message = serve_control_failure(Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "fixture connection reset",
        )))
        .to_string();
        assert!(message.starts_with("daemon_unavailable:"));
        assert!(message.contains("serve control read failed: fixture connection reset"));
    }

    #[tokio::test]
    async fn serve_registration_releases_on_handler_error_and_disconnect() {
        for failed in [false, true] {
            let registry = Arc::new(tokio::sync::Mutex::new(registry::CheckoutRegistry::new()));
            let shared = registry.clone();
            let runtime = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                let handler = || -> Result<()> {
                    let _registration = ServeRegistration::new(shared.clone(), runtime.clone(), 1);
                    assert!(
                        runtime
                            .block_on(async { shared.lock().await.idle_exit_deadline() })
                            .is_none()
                    );
                    ensure!(!failed, "fixture registration or reply failure");
                    Ok(())
                };
                assert_eq!(handler().is_err(), failed);
            })
            .await
            .unwrap();
            assert!(registry.lock().await.idle_exit_deadline().is_some());
        }
    }

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

/// Scoped to one blocking control handler, including registration/reply errors.
struct ServeRegistration {
    registry: Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
    runtime: tokio::runtime::Handle,
    session: u64,
}

impl ServeRegistration {
    fn new(
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

fn dispatch_connection(
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
    let ingress_deadline = accepted_at + Duration::from_secs(20);
    #[cfg(feature = "test-support")]
    let ingress_deadline = std::env::var("TRELLIS_TEST_METRICS_INGRESS_DEADLINE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|milliseconds| accepted_at + Duration::from_millis(milliseconds))
        .unwrap_or(ingress_deadline);
    let (request, prefaced) = read_daemon_request(
        &stream,
        browser_shutdown.subscribe(),
        ingress_deadline,
        #[cfg(test)]
        None,
    )?;
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
    if request.operation == "daemon_stop" {
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
        return Ok(());
    }
    if request.operation == "mcp" {
        let relayed = request
            .payload
            .get("relay")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let result = (|| -> Result<mcp::OpenedWorkspace> {
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
        return served;
    }
    if request.operation == "api" {
        // `trellis api` (#817): one allowlisted browser route, answered by its own handler.
        let result = (|| -> Result<serde_json::Value> {
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
        })();
        let reply = protocol::Reply {
            id: request.id,
            payload: match result {
                Ok(value) => serde_json::json!({"result": value}),
                Err(error) => serde_json::json!({"error": format!("{error:#}")}),
            },
        };
        return write_daemon_reply(&mut stream, &reply);
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
    let _serve_registration =
        serve.then(|| ServeRegistration::new(registry.clone(), runtime.clone(), session));
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
    let architecture = matches!(request.operation.as_str(), "rules_check" | "references");
    let request_deadline = Instant::now()
        + if metrics {
            metrics_work
        } else if architecture {
            CLI_ARCHITECTURE_WAIT
        } else if request.operation == "settled_for_tests" {
            // Only activation contention shares this; the wait has its own bound.
            trellis::testing::HANG_BACKSTOP
        } else {
            CLI_READ_WAIT
        };
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
                    browser.start_owned(browser_shutdown.subscribe())?;
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
                    | "settled_for_tests"
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
            "status" | "producer_plan" | "producer_status" | "settled_for_tests"
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
        let replayable_read = !matches!(
            request.operation.as_str(),
            "index" | "semantic_import" | "producer_pickup" | "producer_run" | "producer_cancel"
        );
        // Pauses a read-only request after activation, so a fixture can take
        // index-use contention before the selected read (#687).
        #[cfg(all(feature = "test-support", unix))]
        let mut contention_fixture =
            std::env::var_os("TRELLIS_TEST_CLI_READ_CONTENTION_SOCKET").filter(|_| replayable_read);
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
        loop {
            // Recorded before the read: a state change between the read and
            // the wait below is never missed.
            let state_version = checkout.state_version();
            let selected = match request.operation.as_str() {
                "status" => checkout.cli_status(),
                #[cfg(feature = "test-support")]
                "settled_for_tests" => (|| {
                    let at: Option<trellis::model::IndexPin> =
                        serde_json::from_value(args["at"].clone())
                            .context("invalid_argument: at is not a revision")?;
                    let pin = checkout.await_settled_for_tests(at)?;
                    Ok(serde_json::json!({ "revision": pin }))
                })(),
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
                    let kind = if request.operation == "references" {
                        "references"
                    } else {
                        "check"
                    };
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
                "semantic_import" => {
                    let options: trellis::store::requests::SemanticImportOptions =
                        serde_json::from_value(args.clone()).map_err(|_| {
                            anyhow::anyhow!("invalid_request: semantic import options")
                        })?;
                    checkout.cli_semantic_import(&options)
                }
                _ => anyhow::bail!("unknown daemon operation"),
            };
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
    let serve_registered = serve && result.is_ok();
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
    if serve_registered {
        // The scoped registration holds the daemon alive until this control
        // connection closes or the daemon stops, signalled by
        // the shutdown channel instead of a polled flag (#138 item 21).
        wait_for_serve_disconnect(stream, browser_shutdown.subscribe(), &runtime)?;
    }
    Ok(())
}

/// Reads one request from an accepted connection, and whether it was
/// prefaced (metrics admission). Each read also ends, as at end of input, on
/// the daemon's published shutdown, so the stop sweep never relies on a
/// cross-thread socket shutdown to end an incomplete request (#741).
fn read_daemon_request(
    stream: &std::os::unix::net::UnixStream,
    shutdown: tokio::sync::watch::Receiver<bool>,
    ingress_deadline: Instant,
    #[cfg(test)] waiting: Option<trellis::daemon::shutdown_read::WaitHookForTests>,
) -> Result<(protocol::Request, bool)> {
    use std::io::{Cursor, Read};
    let mut reader =
        trellis::daemon::shutdown_read::ShutdownReader::new(stream.try_clone()?, shutdown)?;
    #[cfg(test)]
    if let Some(waiting) = waiting {
        reader = reader.on_wait_for_tests(waiting);
    }
    reader.set_idle_timeout(Some(Duration::from_secs(15)));
    let mut first = [0u8; 1];
    reader.read_exact(&mut first)?;
    let prefaced = first[0] == 0xff;
    let request = if prefaced {
        // The reserved preface is consumed. The frame that follows has one
        // absolute budget, including a peer that sends one byte at a time.
        reader.set_idle_timeout(None);
        reader.set_deadline(Some(ingress_deadline));
        protocol::read_frame(&mut reader)?
    } else {
        protocol::read_frame(&mut Cursor::new(first).chain(&mut reader))?
    };
    Ok((request, prefaced))
}

fn wait_for_serve_disconnect(
    stream: std::os::unix::net::UnixStream,
    shutdown: tokio::sync::watch::Receiver<bool>,
    runtime: &tokio::runtime::Handle,
) -> Result<()> {
    wait_for_serve_disconnect_hooked(
        stream,
        shutdown,
        runtime,
        #[cfg(test)]
        None,
    )
}

/// Holds the serve registration until the peer closes the control connection
/// or the daemon publishes shutdown. Both are awaited in one runtime task: on
/// Darwin, `shutdown(2)` on a cloned descriptor does not reliably wake a
/// thread entering a blocking `read(2)` on the same socket (#739), so the
/// shutdown signal must end this wait directly rather than through the socket.
fn wait_for_serve_disconnect_hooked(
    stream: std::os::unix::net::UnixStream,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    runtime: &tokio::runtime::Handle,
    #[cfg(test)] waiting: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<()> {
    // fcntl(O_NONBLOCK) also succeeds on a socket the daemon already shut down
    // (#625), unlike SO_RCVTIMEO on Darwin.
    stream.set_nonblocking(true)?;
    let waiter = runtime.spawn(async move {
        // Registration needs the runtime's reactor, so it happens on the task.
        let stream = tokio::net::UnixStream::from_std(stream)?;
        #[cfg(test)]
        if let Some(waiting) = waiting {
            let _ = waiting.send(());
        }
        let mut one = [0u8; 1];
        loop {
            tokio::select! {
                // A dropped sender also means the daemon is stopping.
                _ = shutdown.wait_for(|stopped| *stopped) => return Ok::<(), std::io::Error>(()),
                ready = stream.readable() => {
                    if ready.is_err() {
                        return Ok(());
                    }
                    match stream.try_read(&mut one) {
                        Ok(n) if n > 0 => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        // EOF or a read error ends the session, as before.
                        _ => return Ok(()),
                    }
                }
            }
        }
    });
    match runtime.block_on(waiter) {
        Ok(result) => Ok(result?),
        // The runtime is shutting down, which also ends the session.
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(anyhow::anyhow!("serve control wait failed: {error}")),
    }
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
        options.language.as_deref().is_none_or(|language| matches!(
            language,
            "java" | "python" | "javascript" | "typescript" | "rust"
        )),
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
