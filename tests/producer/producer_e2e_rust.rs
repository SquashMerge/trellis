//! Real rust-analyzer through the trusted profile. The default variant skips with
//! `SKIP producer_unavailable: <reason>` when rustup, toolchain 1.98.1 or its rust-analyzer
//! is not provisioned (CI). The strict `#[ignore]` variant fails instead and prints an
//! `E2E-EVIDENCE` line on pass. Build output goes to the run directory. Nothing is installed.
#![cfg(feature = "test-support")]

use crate::producer_auto::{Mcp, declaration_semantic_status};
use crate::real;
use crate::support::{self, start_checkout, state_dir};
use real::{Availability, Case};
use serde_json::{Value, json};
use std::fs;

const RUST: Case = Case {
    producer: "rust-analyzer",
    fixture: "rust",
    extension: "rs",
    trust: "trusted",
};

#[test]
fn rust_analyzer_trusted_end_to_end() {
    real::run_case(&RUST, Availability::SkipIfUnavailable);
}

#[test]
#[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
fn rust_analyzer_trusted_end_to_end_strict() {
    assert!(real::run_case(&RUST, Availability::Required).is_some());
}

/// Automatic re-run with real rust-analyzer (#660 AC14): after one approving manual run and an
/// opt-in, an edit to a Rust file is re-run with no command, the language reads `catchingUp`
/// while the run is in flight, and the edited file's evidence is fresh afterwards. The daemon
/// runs under the isolated `HOME`, with the declared producer environment computed from the
/// real one before it is replaced.
fn auto_rerun(availability: Availability) -> Option<String> {
    let (interval_name, interval_value) = support::auto_intervals(1_000, 0, 500);
    let mut env = real::real_producer_env();
    env.push((interval_name, interval_value));
    env.push(support::native_watcher());
    let checkout = start_checkout(
        |root| {
            real::copy_tree(&real::fixture_dir(RUST.fixture), root);
            fs::create_dir(root.join(".git")).unwrap();
        },
        &env,
    );
    let plan = availability.resolve(
        RUST.producer,
        real::plan(&checkout, RUST.producer, RUST.trust, &[]),
    )?;
    let hash = plan["planSha256"].as_str().unwrap().to_owned();
    let output = real::producer_command(
        &checkout,
        "run",
        RUST.producer,
        &["--trust", RUST.trust, "--plan-sha256", &hash],
    )
    .output()
    .unwrap();
    let manual: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{}",
            crate::child::failure(
                format_args!("manual run: no JSON on stdout ({error})"),
                &output,
                Some(&checkout.daemon_stderr()),
            )
        )
    });
    assert_eq!(manual["outcome"], "imported", "{manual}");

    let status = |checkout: &support::Checkout| {
        crate::child::run_cli_json(
            checkout
                .cli()
                .args(["status", "--workspace"])
                .arg(&checkout.root),
            "status",
        )
    };
    let rust_row = |checkout: &support::Checkout| {
        status(checkout)["semanticLanguages"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["language"] == "rust").cloned())
            .unwrap_or(Value::Null)
    };
    assert_eq!(rust_row(&checkout)["semanticState"], "fresh");

    support::enable_auto(&checkout.root, RUST.producer, "");
    let head = || support::producer_status(&checkout)["head"]["sourceRevision"].clone();
    let before = head();
    fs::write(
        checkout.root.join("src/util.rs"),
        "pub fn double(value: u32) -> u32 {\n    value * 3\n}\n",
    )
    .unwrap();
    trellis::testing::wait_until("the edit's head", || head() != before);

    // The language reads catching up while the automatic run is scheduled or running.
    let mut mcp = Mcp::handshake(Mcp::start_in(
        &checkout,
        &[],
        checkout.temp.path().join("mcp.stderr"),
    ));
    let catching_up =
        json!([{"language": "rust", "state": "catchingUp", "reason": "producer re-run scheduled"}]);
    trellis::testing::wait_until(
        "rust catchingUp while the automatic run is in flight",
        || declaration_semantic_status(&mut mcp, "double_checked") == catching_up,
    );

    // Then the run imports with no command, and the edited file's evidence is fresh.
    let mut run_id = Value::Null;
    trellis::testing::wait_until("the automatic run to finish", || {
        let auto = support::auto_run(&checkout, RUST.producer);
        run_id = auto["lastRunId"].clone();
        auto["state"] == "idle" && run_id.is_string()
    });
    let record: Value = serde_json::from_slice(
        &fs::read(
            state_dir(&checkout)
                .join("runs")
                .join(run_id.as_str().unwrap())
                .join("run.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(record["origin"], "auto", "{record}");
    assert_eq!(record["outcome"], "imported", "{record}");
    assert_ne!(record["runId"], manual["runId"]);
    trellis::testing::wait_until("the edited file's evidence to be fresh", || {
        let row = rust_row(&checkout);
        row["semanticState"] == "fresh" && row["reimport"].is_null()
    });
    assert_eq!(
        declaration_semantic_status(&mut mcp, "double_checked"),
        json!([])
    );

    let line = format!(
        "E2E-EVIDENCE producer={} version={} verification=verified trustProfile={} autoRun=imported runId={} sawCatchingUp=true freshAfter=true",
        RUST.producer,
        plan["producer"]["version"].as_str().unwrap(),
        RUST.trust,
        run_id.as_str().unwrap(),
    );
    println!("{line}");
    Some(line)
}

#[test]
fn rust_analyzer_auto_rerun_freshens_edited_file() {
    auto_rerun(Availability::SkipIfUnavailable);
}

#[test]
#[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
fn rust_analyzer_auto_rerun_freshens_edited_file_strict() {
    assert!(auto_rerun(Availability::Required).is_some());
}
