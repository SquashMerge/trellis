//! Bounded streamed semantic batch import: resource policy and the high-water counters that prove it.
//! The admitted and joined batch types are storage's (`crate::store::batch_input`), re-exported
//! here; only this module tree can create the `AdmissionProof` an `AdmittedBatch` is built from.

pub(crate) mod admission_proof;
pub mod admit;
pub mod counters;
pub mod descriptor;
pub mod limits;
pub mod origin;
pub(crate) mod retained;
#[cfg(feature = "test-support")]
pub mod testing;

pub use crate::store::batch_input::{AdmittedBatch, JoinedBatch};
pub use admit::{StageInput, StageOwner, stage_batch};
pub use counters::{BatchCounters, LookupBudget};
pub use limits::BatchLimits;
pub(crate) use retained::RetainedStageContinuation;

#[cfg(test)]
mod descriptor_tests;
#[cfg(test)]
mod limits_tests;
#[cfg(test)]
mod staging_tests;
