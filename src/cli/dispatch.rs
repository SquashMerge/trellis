//! CLI commands as daemon calls: request payloads and the reporting of results.
//! `dispatch_cli` stays in `main.rs`.
#[cfg(feature = "test-support")]
use crate::TestSettledArgs;
use crate::{
    Command, IndexArgs, MetricsArgs, ProducerAction, ProducerCommand, ProducerPickupArgs,
    ProducerPlanArgs, ProducerRunArgs, QueryArgs, RulesArgs, RulesCommand, SemanticAction,
    SemanticCommand, SemanticImportArgs, SymbolsArgs, WorkspaceArgs, print_json,
    validate_cli_metrics_options,
};
use anyhow::{Context, Result, ensure};
use trellis::model::ViewQuery;

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
        .map(|path| absolute_utf8(path, "invalid_request: non-UTF8 executable path"))
        .transpose()?;
    let mut settings = serde_json::Map::new();
    if let Some(features) = &args.cargo_features {
        let features: Vec<&str> = features
            .split(',')
            .map(str::trim)
            .filter(|feature| !feature.is_empty())
            .collect();
        settings.insert("cargoFeatures".into(), serde_json::json!(features));
    }
    if let Some(project) = &args.ts_project {
        settings.insert("typescriptProject".into(), serde_json::json!(project));
    }
    if let Some(name) = &args.python_project_name {
        settings.insert("pythonProjectName".into(), serde_json::json!(name));
    }
    if let Some(version) = &args.python_project_version {
        settings.insert("pythonProjectVersion".into(), serde_json::json!(version));
    }
    if let Some(tool) = &args.java_build_tool {
        settings.insert("javaBuildTool".into(), serde_json::json!(tool));
    }
    Ok(serde_json::json!({
        "version": 1,
        "producer": args.producer,
        "trustProfile": args.trust,
        "limits": {"timeoutSecs": args.timeout_secs, "memoryMiB": args.memory_mib},
        "executable": executable,
        "environment": trellis::producer::detect::Environment::from_process()?,
        "settings": settings,
    }))
}

/// One daemon call built from a CLI subcommand.
pub(crate) struct CliCall<'a> {
    pub(crate) operation: &'static str,
    pub(crate) workspace: &'a WorkspaceArgs,
    /// The request's `args`.
    pub(crate) payload: serde_json::Value,
    pub(crate) read_only: bool,
}
impl<'a> CliCall<'a> {
    fn new(
        operation: &'static str,
        workspace: &'a WorkspaceArgs,
        payload: serde_json::Value,
        read_only: bool,
    ) -> Self {
        Self {
            operation,
            workspace,
            payload,
            read_only,
        }
    }
}

/// Validates `command`'s arguments and builds its daemon call.
pub(crate) fn cli_call(command: &Command) -> Result<CliCall<'_>> {
    let call = CliCall::new;
    Ok(match command {
        Command::Status(args) => call("status", args, serde_json::json!({}), true),
        #[cfg(feature = "test-support")]
        Command::TestSettled(args) => call(
            "settled_for_tests",
            &args.workspace,
            test_settled_cli_payload(args)?,
            true,
        ),
        Command::Rules(RulesArgs {
            command: RulesCommand::Check(args),
        }) => call(
            "rules_check",
            &args.workspace,
            architecture_cli_arguments(args.pin.as_deref(), &args.paths, None)?,
            true,
        ),
        Command::References(args) => call(
            "references",
            &args.workspace,
            architecture_cli_arguments(args.pin.as_deref(), &args.paths, Some(args.limit))?,
            true,
        ),
        Command::Symbols(args) => {
            call("symbols", &args.workspace, symbols_cli_payload(args)?, true)
        }
        Command::Query(args) => call("query", &args.workspace, query_cli_payload(args)?, true),
        Command::Metrics(args) => {
            call("metrics", &args.workspace, metrics_cli_payload(args)?, true)
        }
        Command::Index(args) => call("index", &args.workspace, index_cli_payload(args), false),
        Command::Producer(ProducerCommand { action }) => match action {
            ProducerAction::Plan(args) => call(
                "producer_plan",
                &args.workspace,
                producer_plan_payload(args)?,
                true,
            ),
            ProducerAction::Run(args) => call(
                "producer_run",
                &args.plan.workspace,
                producer_run_cli_payload(args)?,
                false,
            ),
            ProducerAction::Cancel(args) => call(
                "producer_cancel",
                &args.workspace,
                serde_json::json!({"version": 1, "runId": args.run}),
                false,
            ),
            ProducerAction::Status(args) => call(
                "producer_status",
                &args.workspace,
                serde_json::json!({
                    "version": 1,
                    "runId": args.run,
                    "environment": trellis::producer::detect::Environment::from_process()?,
                }),
                true,
            ),
            ProducerAction::Pickup(args) => call(
                "producer_pickup",
                &args.workspace,
                producer_pickup_cli_payload(args)?,
                false,
            ),
            _ => unreachable!(),
        },
        Command::Semantic(SemanticCommand {
            action: SemanticAction::Import(args),
        }) => call(
            "semantic_import",
            &args.workspace,
            semantic_import_cli_payload(args)?,
            false,
        ),
        _ => unreachable!(),
    })
}

#[cfg(feature = "test-support")]
fn test_settled_cli_payload(args: &TestSettledArgs) -> Result<serde_json::Value> {
    let at = args
        .at
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
        .context("invalid_argument: --at is not a revision")?;
    Ok(serde_json::json!({ "at": at }))
}

fn symbols_cli_payload(args: &SymbolsArgs) -> Result<serde_json::Value> {
    ensure!((1..=150).contains(&args.limit), "limit must be 1..150");
    Ok(serde_json::json!({"search": args.search, "limit": args.limit}))
}

fn query_cli_payload(args: &QueryArgs) -> Result<serde_json::Value> {
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
    Ok(serde_json::json!({
        "seed": args.seed, "depth": args.depth, "maxNodes": args.max_nodes,
        "maxCalls": args.max_calls, "includeCallbacks": args.include_callbacks,
        "excludePaths": args.exclude_path,
        "semanticProducerId": args.semantic_producer_id
    }))
}

fn metrics_cli_payload(args: &MetricsArgs) -> Result<serde_json::Value> {
    let mut options = trellis::metrics_read::MetricsOptions {
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
    validate_cli_metrics_options(&options)?;
    let mut payload = serde_json::json!({
        "scope": options.scope, "sourceSetId": options.source_set_id,
        "language": options.language, "path": options.path,
        "sid": options.sid, "aggregate": options.aggregate, "role": options.role,
        "pin": options.pin
    });
    // No `--metrics` means every metric: the request leaves the option out.
    if args.metrics.is_some() {
        payload["metrics"] = serde_json::json!(options.metrics);
    }
    Ok(payload)
}

/// Its range is checked in `dispatch_cli`, after the workspace resolves.
fn index_cli_payload(args: &IndexArgs) -> serde_json::Value {
    match args.max_file_bytes {
        Some(bytes) => serde_json::json!({ "maxFileBytes": bytes }),
        None => serde_json::json!({}),
    }
}

fn producer_run_cli_payload(args: &ProducerRunArgs) -> Result<serde_json::Value> {
    ensure!(
        is_lower_hex64(&args.plan_sha256),
        "invalid_request: plan digest"
    );
    let mut payload = producer_plan_payload(&args.plan)?;
    payload["planSha256"] = serde_json::json!(args.plan_sha256);
    Ok(payload)
}

fn producer_pickup_cli_payload(args: &ProducerPickupArgs) -> Result<serde_json::Value> {
    let artifact = args
        .artifact
        .as_ref()
        .map(|path| absolute_utf8(path, "invalid_request: non-UTF8 artifact path"))
        .transpose()?;
    Ok(serde_json::json!({
        "version": 1,
        "producer": args.producer,
        "artifact": artifact,
        "clear": args.clear,
    }))
}

fn semantic_import_cli_payload(args: &SemanticImportArgs) -> Result<serde_json::Value> {
    ensure!(
        is_lower_hex64(&args.batch_sha256),
        "invalid_request: batch digest"
    );
    Ok(serde_json::json!({
        "version": 2,
        "descriptor": absolute_utf8(&args.batch, "invalid_request: non-UTF8 input path")?,
        "descriptorSha256": args.batch_sha256
    }))
}

fn is_lower_hex64(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `path` made absolute against the current directory, as UTF-8; `non_utf8`
/// is the error when it is not.
fn absolute_utf8(path: &std::path::Path, non_utf8: &'static str) -> Result<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(absolute.to_str().context(non_utf8)?.to_owned())
}

/// Prints the daemon's reply for `command` and turns a reported failure into
/// the command's error.
pub(crate) fn report_cli_result(command: &Command, value: &serde_json::Value) -> Result<()> {
    match command {
        Command::Metrics(args) if !args.json => print_metrics_text(value),
        _ => print_json(value)?,
    }
    match command {
        Command::Producer(ProducerCommand {
            action: ProducerAction::Pickup(args),
        }) => report_producer_pickup(args, value),
        Command::Producer(ProducerCommand {
            action: ProducerAction::Run(_),
        }) => report_producer_run(value),
        Command::Rules(_) => {
            report_architecture_error(value)?;
            ensure!(
                value["payload"]["counts"]["deviates"].as_u64().unwrap_or(0) == 0,
                "architecture deviations"
            );
            Ok(())
        }
        Command::References(_) => report_architecture_error(value),
        _ => Ok(()),
    }
}

fn print_metrics_text(value: &serde_json::Value) {
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

fn report_producer_pickup(args: &ProducerPickupArgs, value: &serde_json::Value) -> Result<()> {
    if args.artifact.is_some() && value["state"] != "done" {
        anyhow::bail!(
            "{}: producer pickup {}",
            value["errorCode"].as_str().unwrap_or("index_failed"),
            value["state"].as_str().unwrap_or("failed")
        );
    }
    Ok(())
}

fn report_producer_run(value: &serde_json::Value) -> Result<()> {
    if value["state"] == "done" {
        return Ok(());
    }
    // One line saying why (#967); the reply on stdout has the stderr lines.
    use std::io::Write;
    if let Ok(Some(failure)) = serde_json::from_value::<Option<trellis::producer::failure::Failure>>(
        value["failure"].clone(),
    ) {
        let code = value["errorCode"]
            .as_str()
            .or(value["outcome"].as_str())
            .unwrap_or("producer_failed");
        let _ = writeln!(
            std::io::stderr(),
            "{}",
            trellis::producer::failure::cli_line(code, &failure)
        );
    }
    anyhow::bail!(
        "{}: producer run {}",
        value["errorCode"].as_str().unwrap_or("producer_failed"),
        value["state"].as_str().unwrap_or("failed")
    );
}

fn report_architecture_error(value: &serde_json::Value) -> Result<()> {
    ensure!(
        value.get("error").is_none(),
        "architecture evaluation failed"
    );
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
