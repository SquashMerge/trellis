//! Real scip-java through the restricted profile. The default variant skips with
//! `SKIP producer_unavailable: <reason>` when scip-java or its offline JDK cache is not
//! provisioned (CI). The strict `#[ignore]` variant fails instead and prints an
//! `E2E-EVIDENCE` line on pass. Nothing is installed.
#![cfg(feature = "test-support")]

#[path = "support/real_producers.rs"]
mod real;
#[path = "support/producer.rs"]
mod support;
use real::{Availability, Case};

const JAVA: Case = Case {
    producer: "scip-java",
    fixture: "java",
    extension: "java",
    trust: "restricted",
};

#[test]
fn scip_java_restricted_end_to_end() {
    real::run_case(&JAVA, Availability::SkipIfUnavailable);
}

#[test]
#[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
fn scip_java_restricted_end_to_end_strict() {
    assert!(real::run_case(&JAVA, Availability::Required).is_some());
}
