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
    store::topology::{TopologyRoots, WorkspaceIdentity},
};

mod cli;
mod daemon_main;
#[cfg(feature = "test-support")]
mod native_navigation_fixture;

#[cfg(test)]
use cli::serve::serve_control_failure;
use cli::{
    api::api_command,
    dispatch::{cli_call, report_cli_result},
    serve::{serve_via_daemon, stop_existing_daemon},
    transport::{daemon_request, send_daemon, socket_path},
};
use daemon_main::connection::dispatch_connection;
#[cfg(test)]
use daemon_main::{
    connection::ServeRegistration,
    io::{read_daemon_request, wait_for_serve_disconnect, wait_for_serve_disconnect_hooked},
};

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
    /// Plan, inspect and check headless semantic producers. Planning executes nothing; only
    /// `producer install` installs, and only on request.
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
    /// Print the output gzip-compressed and base64-encoded, on one line; exit codes and the
    /// 16 MiB answer bound (on the decoded answer) are unchanged (#915).
    #[arg(long)]
    gzip: bool,
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
    #[arg(long, required_unless_present = "run", conflicts_with = "run")]
    report: bool,
    /// Run one GC pass now in the daemon, as the checkout's worker, and print what
    /// it collected and skipped. It uses the cwd checkout or `--workspace`.
    #[arg(long)]
    run: bool,
    #[command(flatten)]
    workspace: WorkspaceArgs,
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
    /// Report whether each producer is ready, with the pinned install command for each that
    /// isn't. Read-only: runs and installs nothing, and needs no daemon. Exits 0.
    Doctor(ProducerDoctorArgs),
    /// Run one producer's pinned install command, only on this explicit request. Prints the
    /// command, then asks on a terminal unless --yes. Never installs prerequisites.
    Install(ProducerInstallArgs),
}
#[derive(Args)]
struct ProducerDoctorArgs {
    #[command(flatten)]
    workspace: WorkspaceArgs,
    /// Print the report as JSON.
    #[arg(long)]
    json: bool,
}
#[derive(Args)]
struct ProducerInstallArgs {
    /// The producer to install, at its pinned version.
    #[arg(value_parser = clap::builder::PossibleValuesParser::new(trellis::producer::ProducerName::ALL.map(trellis::producer::ProducerName::as_str)))]
    producer: String,
    /// Run the pinned command without asking, for scripted use.
    #[arg(long)]
    yes: bool,
    /// Print the producer's doctor entry afterwards as JSON.
    #[arg(long)]
    json: bool,
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
    /// rust-analyzer: the Cargo features to enable, comma-separated, instead of the
    /// `required-features` the plan reads from the workspace manifests. Empty enables none.
    #[arg(long)]
    cargo_features: Option<String>,
    /// scip-typescript: the project config to use instead of the one the plan chooses.
    #[arg(long, value_parser = ["tsconfig.json", "jsconfig.json", "generated"])]
    ts_project: Option<String>,
    /// scip-python: the `--project-name` to pass instead of the `pyproject.toml` name.
    #[arg(long)]
    python_project_name: Option<String>,
    /// scip-python: the `--project-version` to pass instead of the `pyproject.toml` version.
    #[arg(long)]
    python_project_version: Option<String>,
    /// scip-java, trusted profile: the build tool to run instead of the one the plan detects.
    #[arg(long, value_parser = ["maven", "gradle", "scip"])]
    java_build_tool: Option<String>,
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
/// How long a CLI read on a checkout with no admitted head waits for the first
/// validated head its activation started (#842, owner ruling 2026-11-09). Below
/// the two minutes an agent's shell command commonly gets. On expiry the read
/// fails `index_not_ready` and indexing continues in the daemon.
const CLI_FIRST_INDEX_WAIT: Duration = Duration::from_secs(90);
/// How often that wait reports progress. Also keeps the client's per-read
/// inactivity bound (`CLI_REPLY_BOUND`) from expiring while it waits.
const CLI_FIRST_INDEX_PROGRESS: Duration = Duration::from_secs(10);

fn cli_first_index_wait() -> Duration {
    #[cfg(feature = "test-support")]
    if let Some(wait) = metrics_test_duration("TRELLIS_TEST_CLI_FIRST_INDEX_WAIT_MS") {
        return wait;
    }
    CLI_FIRST_INDEX_WAIT
}

#[tokio::main]
async fn main() -> Result<()> {
    // Before any SQLite connection opens (#673).
    trellis::sqlite_config::disable_memory_statistics();
    // Before any tree-sitter use (#1282).
    trellis::measured_parse::install();
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
            client::relay_stdio(stream, &identity, |timeout| {
                client::connect_or_start(&socket, start_daemon, timeout)
            })?;
        }
        Command::Model(args) => model_command(args).await?,
        Command::Producer(ProducerCommand {
            action: ProducerAction::Doctor(args),
        }) => producer_doctor(&args)?,
        Command::Producer(ProducerCommand {
            action: ProducerAction::Install(args),
        }) => producer_install(&args)?,
        Command::Api(args) => std::process::exit(api_command(&args)),
        Command::Gc(args) if !args.run => {
            ensure!(
                args.workspace.workspace.is_none(),
                "invalid_argument: --workspace applies to gc --run"
            );
            let request = daemon_request("gc_report", serde_json::json!({}));
            print_json(&send_daemon(&request, true)?)?;
        }
        command @ (Command::Index(_)
        | Command::Gc(_)
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

/// `producer doctor` (#880): runs in the CLI process against the environment the CLI would
/// declare to `producer plan`, so it needs no daemon or index.
fn producer_doctor(args: &ProducerDoctorArgs) -> Result<()> {
    use trellis::producer::{detect, doctor};
    // Operational failures become typed fields of the report, so doctor always reports and
    // exits 0; only invalid command-line usage (clap) fails.
    let declared = detect::Environment::from_process()
        .and_then(|environment| detect::Declared::new(&environment))
        .map_err(|error| format!("{error:#}"));
    let workspace = std::env::current_dir()
        .map_err(anyhow::Error::from)
        .and_then(|cwd| match &args.workspace.workspace {
            Some(explicit) => {
                WorkspaceIdentity::discover_unattached(Some(explicit), &cwd).map(|id| Some(id.root))
            }
            None => Ok(WorkspaceIdentity::discover_unattached(None, &cwd)
                .ok()
                .map(|identity| identity.root)),
        })
        .map_err(|error| format!("{error:#}"));
    let report = doctor::report(
        workspace
            .as_ref()
            .map(Option::as_deref)
            .map_err(Clone::clone),
        declared.as_ref().map_err(Clone::clone),
    );
    if args.json {
        print_json(&report)
    } else {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(doctor::render(&report).as_bytes())?;
        Ok(stdout.flush()?)
    }
}

/// `producer install` (#880): the only caller of the pinned install.
fn producer_install(args: &ProducerInstallArgs) -> Result<()> {
    use trellis::producer::{ProducerName, detect, doctor, install};
    let producer = ProducerName::parse(&args.producer)
        .ok_or_else(|| anyhow::anyhow!("invalid_request: producer"))?;
    let declared = detect::Declared::new(&detect::Environment::from_process()?)?;
    let confirmation = if args.yes {
        install::Confirmation::Assumed
    } else {
        install::Confirmation::Ask
    };
    // With --json, stdout holds only the JSON entry, so progress and the install command's
    // own output go to stderr.
    let after = if args.json {
        install::run_pinned_install(
            producer,
            &declared,
            confirmation,
            install::ChildOutput::Stderr,
            &mut std::io::stderr(),
        )?
    } else {
        install::run_pinned_install(
            producer,
            &declared,
            confirmation,
            install::ChildOutput::Inherit,
            &mut std::io::stdout(),
        )?
    };
    if args.json {
        print_json(&after)
    } else {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(b"Doctor:\n")?;
        stdout.write_all(doctor::render_producer(&after).as_bytes())?;
        Ok(stdout.flush()?)
    }
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

fn dispatch_cli(command: &Command) -> Result<()> {
    let call = cli_call(command)?;
    let (_roots, identity) = call.workspace.resolve_unattached()?;
    // Checked only once the workspace has resolved.
    if let Command::Index(args) = command {
        ensure!(
            (1..=16_777_216).contains(&args.max_file_bytes.unwrap_or(2_097_152)),
            "max-file-bytes must be 1..16777216"
        );
    }
    let request = daemon_request(
        call.operation,
        serde_json::json!({
            "workspace": identity.root, "args": call.payload,
        }),
    );
    let value = send_daemon(&request, call.read_only)?;
    report_cli_result(command, &value)
}

/// The metrics option rules of the CLI and of the daemon request it sends:
/// the shared transport rules, as `invalid_argument`.
fn validate_cli_metrics_options(options: &trellis::metrics_read::MetricsOptions) -> Result<()> {
    options
        .validate_transport()
        .map_err(|error| anyhow::anyhow!("invalid_argument: {error}"))
}

/// The work budget of a metrics request, from its still unvalidated scope.
fn metrics_work_budget(request: &protocol::Request) -> Duration {
    trellis::metrics_read::MetricsOptions::work_budget(
        request.payload["args"]["scope"]
            .as_str()
            .unwrap_or_default(),
    )
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
        report_lost_election(&paths.socket);
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
    // A metrics test that times the metrics read's own work needs no published core.
    #[cfg(feature = "test-support")]
    if std::env::var_os("TRELLIS_TEST_ARCHITECTURE_ON_DEMAND").is_some() {
        checkouts.architecture_on_demand_for_tests();
    }
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
    // The listener now only answers `daemon_stopping`, until the owner
    // releases the socket and election lock below (#1293).
    let stopping = tokio::spawn(answer_stopping(listener));
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
        // An explicit stop cancels accepted work instead of draining it
        // (#1293). The stop command goes first, before anything that can wait
        // on in-flight work: a pickup or producer-run import wait ends on it,
        // and so does every connection wait.
        registry::CheckoutRegistry::request_stop(registry.clone()).await;
        #[cfg(feature = "test-support")]
        if let Some(path) = std::env::var_os("TRELLIS_TEST_DAEMON_STOP_SOCKET") {
            use std::io::Write;
            if let Ok(mut hook) = std::os::unix::net::UnixStream::connect(path) {
                let _ = hook.write_all(b"S");
            }
        }
        registry::CheckoutRegistry::cancel_producers(registry.clone()).await?;
    }
    stop_daemon_connections(&browser_shutdown, &mut connections).await;
    browser.lock().await.finish_owned().await?;
    if stop.is_some() {
        registry::CheckoutRegistry::shutdown(registry.clone()).await?;
    }
    drop(browser);
    drop(registry);
    stopping.abort();
    let _ = stopping.await;
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

/// A candidate that lost the election exits 0; say why on stderr (#1293).
/// An auto-started candidate's stderr is null, so only a bare `trellis
/// daemon` shows it. A no-op request tells a stopping owner from a running one.
fn report_lost_election(socket: &std::path::Path) {
    use std::io::Write;
    let answer = (|| -> std::io::Result<protocol::Reply> {
        let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        protocol::write_frame(
            &mut stream,
            &daemon_request("daemon_probe", serde_json::json!({})),
        )?;
        protocol::read_frame(&mut stream)
    })();
    let message = match answer {
        Ok(reply) if protocol::is_stopping(&reply.payload) => {
            "trellis daemon: another Trellis daemon holds the election lock and is stopping; \
             start this one again once it has exited (or let a client start one)"
        }
        Ok(_) => "trellis daemon: another Trellis daemon is already running; this one exits",
        Err(_) => "trellis daemon: another Trellis daemon holds the election lock; this one exits",
    };
    let _ = writeln!(std::io::stderr().lock(), "{message}");
}

/// While a daemon stops, every new connection is answered `daemon_stopping`
/// before admission (#1293): clients wait for the stop, then re-run the
/// election, instead of hanging in a listen backlog nobody serves.
async fn answer_stopping(listener: tokio::net::UnixListener) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Ok(stream) = stream.into_std() else {
            continue;
        };
        // Bounded by its own read and write deadlines.
        tokio::task::spawn_blocking(move || {
            let _ = reply_stopping(stream);
        });
    }
}

fn reply_stopping(mut stream: std::os::unix::net::UnixStream) -> std::io::Result<()> {
    // One request frame within one second, however the peer sends it.
    let deadline = Instant::now() + Duration::from_secs(1);
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let mut first = [0u8; 1];
    read_exact_by(&mut stream, &mut first, deadline)?;
    // A metrics request arrives behind its one-byte admission preface.
    let request: protocol::Request = if first[0] == 0xff {
        protocol::read_frame_until(&mut stream, deadline)?
    } else {
        let mut length = [first[0], 0, 0, 0];
        read_exact_by(&mut stream, &mut length[1..], deadline)?;
        let size = u32::from_be_bytes(length) as usize;
        if size > protocol::MAX_FRAME {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let mut bytes = vec![0; size];
        read_exact_by(&mut stream, &mut bytes, deadline)?;
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)?
    };
    protocol::write_frame(
        &mut stream,
        &protocol::Reply {
            id: request.id,
            payload: serde_json::json!({ "error": protocol::DAEMON_STOPPING_MESSAGE }),
        },
    )?;
    #[cfg(feature = "test-support")]
    if let Some(path) = std::env::var_os("TRELLIS_TEST_DAEMON_STOPPING_SOCKET") {
        use std::io::Write;
        if let Ok(mut hook) = std::os::unix::net::UnixStream::connect(path) {
            let _ = hook.write_all(b"A");
        }
    }
    Ok(())
}

fn read_exact_by(
    stream: &mut std::os::unix::net::UnixStream,
    mut buf: &mut [u8],
    deadline: Instant,
) -> std::io::Result<()> {
    use std::io::Read;
    while !buf.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        stream.set_read_timeout(Some(left))?;
        let read = stream.read(buf)?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[read..];
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

#[cfg(feature = "test-support")]
fn metrics_test_duration(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
}
