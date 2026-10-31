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
