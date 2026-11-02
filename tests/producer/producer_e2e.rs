//! Real scip-typescript (JavaScript mode) and scip-python through the restricted profile.
//! Each producer has a default variant that skips with `SKIP producer_unavailable: <reason>`
//! when it is not provisioned (CI), and a strict `#[ignore]` variant that fails instead and
//! prints an `E2E-EVIDENCE` line on pass. Nothing is installed.
#![cfg(feature = "test-support")]

use crate::real;
use crate::support;
use real::{Availability, Case};

const TYPESCRIPT: Case = Case {
    producer: "scip-typescript",
    fixture: "javascript",
    extension: "js",
    trust: "restricted",
};
const PYTHON: Case = Case {
    producer: "scip-python",
    fixture: "python",
    extension: "py",
    trust: "restricted",
};

#[test]
fn scip_typescript_javascript_restricted_end_to_end() {
    real::run_case(&TYPESCRIPT, Availability::SkipIfUnavailable);
}

#[test]
#[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
fn scip_typescript_javascript_restricted_end_to_end_strict() {
    assert!(real::run_case(&TYPESCRIPT, Availability::Required).is_some());
}

#[test]
fn scip_python_restricted_end_to_end() {
    real::run_case(&PYTHON, Availability::SkipIfUnavailable);
}

#[test]
#[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
fn scip_python_restricted_end_to_end_strict() {
    assert!(real::run_case(&PYTHON, Availability::Required).is_some());
}

#[test]
fn unavailable_producer_is_typed_skip() {
    let checkout = support::start_checkout(
        |root| real::copy_tree(&real::fixture_dir("javascript"), root),
        &[],
    );
    let not_a_package = checkout.root.join("package.json");
    let forced = || {
        real::plan(
            &checkout,
            "scip-typescript",
            "restricted",
            &["--executable", not_a_package.to_str().unwrap()],
        )
    };
    let reason = forced().unwrap_err();
    assert!(
        reason.contains("not an npm package entry point"),
        "{reason}"
    );
    assert!(
        Availability::SkipIfUnavailable
            .resolve("scip-typescript", forced())
            .is_none()
    );
    let strict =
        std::panic::catch_unwind(|| Availability::Required.resolve("scip-typescript", forced()));
    assert!(strict.is_err(), "the strict variant must fail, not skip");
}

/// One line of 10,001 function declarations: the shape of a committed minified bundle, over
/// the 10,000-symbols-per-document cap under real scip-typescript.
fn minified_bundle() -> String {
    let mut bundle = String::new();
    for n in 0..10_001 {
        bundle.push_str(&format!("function b{n}(){{}}"));
    }
    bundle.push('\n');
    bundle
}

/// #634: a real scip-typescript artifact whose one generated bundle is over the per-document
/// symbol cap imports every other document; the bundle is reported partial with
/// `document_cap_exceeded` and no facts.
fn bundle_over_symbol_cap(availability: Availability) -> Option<()> {
    let checkout = support::start_checkout(
        |root| {
            real::copy_tree(&real::fixture_dir("javascript"), root);
            std::fs::create_dir(root.join("vendor")).unwrap();
            std::fs::write(root.join("vendor/bundle.js"), minified_bundle()).unwrap();
        },
        &[],
    );
    let plan = availability.resolve(
        "scip-typescript",
        real::plan(&checkout, "scip-typescript", "restricted", &[]),
    )?;
    let hash = plan["planSha256"].as_str().unwrap().to_owned();
    let output = real::producer_command(
        &checkout,
        "run",
        "scip-typescript",
        &["--trust", "restricted", "--plan-sha256", &hash],
    )
    .output()
    .unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "no JSON on stdout; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(reply["outcome"], "imported", "{reply}");
    let report = &reply["import"]["report"];
    let javascript = report["languages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|language| language["language"] == "javascript")
        .unwrap_or_else(|| panic!("no javascript report: {report}"));
    assert_eq!(
        (
            &javascript["complete"],
            &javascript["partial"],
            &javascript["failed"]
        ),
        (&2.into(), &1.into(), &0.into()),
        "{javascript}"
    );
    let reasons = &javascript["partialReasons"];
    assert_eq!(reasons["capped"], 1, "{reasons}");
    let listed = reasons["listed"].as_array().unwrap();
    let bundle: Vec<_> = listed
        .iter()
        .filter(|entry| entry["path"] == "vendor/bundle.js")
        .collect();
    assert_eq!(bundle.len(), 1, "{reasons}");
    assert_eq!(bundle[0]["diagnostic"], "document_cap_exceeded");
    assert_eq!(bundle[0]["reasons"]["capped"], true);
    assert!(
        listed
            .iter()
            .all(|entry| entry["path"] != "src/a.js" && entry["path"] != "src/b.js"),
        "the other documents import complete: {reasons}"
    );
    let claims: u64 = report["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|artifact| artifact["claims"].as_u64())
        .sum();
    assert!(claims > 0, "the other documents keep their facts: {report}");
    println!("E2E-EVIDENCE producer=scip-typescript bundle=document_cap_exceeded claims={claims}");
    Some(())
}

#[test]
fn scip_typescript_bundle_over_symbol_cap_imports_the_rest() {
    bundle_over_symbol_cap(Availability::SkipIfUnavailable);
}

#[test]
#[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
fn scip_typescript_bundle_over_symbol_cap_imports_the_rest_strict() {
    assert!(bundle_over_symbol_cap(Availability::Required).is_some());
}
