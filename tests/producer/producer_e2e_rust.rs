//! Real rust-analyzer through the trusted profile. The default variant skips with
//! `SKIP producer_unavailable: <reason>` when rustup, toolchain 1.98.1 or its rust-analyzer
//! is not provisioned (CI). The strict `#[ignore]` variant fails instead and prints an
//! `E2E-EVIDENCE` line on pass. Build output goes to the run directory. Nothing is installed.
#![cfg(feature = "test-support")]

use crate::real;
use real::{Availability, Case};

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
