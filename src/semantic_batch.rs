//! Bounded streamed semantic batch import: resource policy and the high-water counters that prove it.
use crate::{model::IndexPin, scip_admission::PinnedProducer, scip_raw::ScipCaptureError};
use anyhow::Result;
use descriptor::Descriptor;
use rusqlite::Connection;
use staging::Stage;

pub mod admit;
pub mod descriptor;
pub mod limits;
pub mod staging;
#[cfg(feature = "test-support")]
pub mod testing;

pub use admit::{StageOwner, stage_batch};
pub use limits::BatchLimits;

#[cfg(test)]
mod descriptor_tests;
#[cfg(test)]
mod limits_tests;
#[cfg(test)]
mod staging_tests;

/// Counters threaded through batch staging and decoding. The `*_high_water*` fields are the
/// AC5 test seam; `wire_fields` and `candidates` are the batch-shared decoder walk budget.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchCounters {
    pub wire_fields: u64,
    pub candidates: u64,
    pub page_buffer_high_water_bytes: u64,
    pub live_page_buffers: u64,
    pub live_page_buffers_high_water: u64,
    pub unit_buffer_high_water_bytes: u64,
    pub live_unit_buffers: u64,
    pub live_unit_buffers_high_water: u64,
    pub unit_reservation_high_water_bytes: u64,
    /// Logical join expansion, separate from admission's decode-buffer reservation.
    pub join_reservation_high_water_bytes: u64,
    pub document_cache_high_water_bytes: u64,
    pub document_cache_high_water_entries: u64,
    pub active_document_groups_high_water: u64,
    pub candidate_group_high_water: u64,
    /// Charged staged-row attempts of the latest join attempt, by owning table, including the
    /// retained native seed under `seed:retained-native`. Their sum is the attempt's row meter.
    pub staged_row_attempts: std::collections::BTreeMap<String, u64>,
}

impl BatchCounters {
    pub(crate) fn page_buffer_begin(&mut self, len: u64) {
        self.live_page_buffers += 1;
        self.live_page_buffers_high_water = self
            .live_page_buffers_high_water
            .max(self.live_page_buffers);
        self.page_buffer_high_water_bytes = self.page_buffer_high_water_bytes.max(len);
    }

    pub(crate) fn page_buffer_end(&mut self) {
        self.live_page_buffers = self.live_page_buffers.saturating_sub(1);
    }

    /// Folds a join's high-water marks into the batch counters.
    pub(crate) fn absorb_join(&mut self, join: &BatchCounters) {
        self.join_reservation_high_water_bytes = self
            .join_reservation_high_water_bytes
            .max(join.join_reservation_high_water_bytes);
        self.document_cache_high_water_bytes = self
            .document_cache_high_water_bytes
            .max(join.document_cache_high_water_bytes);
        self.document_cache_high_water_entries = self
            .document_cache_high_water_entries
            .max(join.document_cache_high_water_entries);
        self.active_document_groups_high_water = self
            .active_document_groups_high_water
            .max(join.active_document_groups_high_water);
        self.candidate_group_high_water = self
            .candidate_group_high_water
            .max(join.candidate_group_high_water);
        self.staged_row_attempts = join.staged_row_attempts.clone();
    }
}

/// Lookup work charged by the staged join. One budget serves every attempt of a request, so a
/// revision-conflict retry cannot spend the limit twice over.
#[derive(Debug, Clone)]
pub struct LookupBudget {
    limit: u64,
    used: u64,
}

impl LookupBudget {
    pub fn new(limits: &BatchLimits) -> Self {
        Self {
            limit: limits.lookup_steps,
            used: 0,
        }
    }

    /// Accounts `units` of work before it is done.
    pub fn charge(&mut self, units: u64) -> std::result::Result<(), ScipCaptureError> {
        self.used = self.used.saturating_add(units);
        if self.used > self.limit {
            return Err(ScipCaptureError::LimitExceeded("lookup steps".into()));
        }
        Ok(())
    }

    pub fn used(&self) -> u64 {
        self.used
    }
}

/// A batch whose staged join emitted only validated rows. Only the staged join builds one, and
/// publication takes it by reference, so nothing unvalidated can reach the index.
#[derive(Debug)]
pub struct JoinedBatch {
    captured: IndexPin,
    destination: IndexPin,
    destination_revision_id: String,
    descriptor_sha256: String,
    attempt: i64,
    report: serde_json::Value,
}

impl JoinedBatch {
    pub(crate) fn seal(
        captured: IndexPin,
        destination: IndexPin,
        destination_revision_id: String,
        descriptor_sha256: String,
        attempt: i64,
        report: serde_json::Value,
    ) -> Self {
        Self {
            captured,
            destination,
            destination_revision_id,
            descriptor_sha256,
            attempt,
            report,
        }
    }

    pub fn captured_pin(&self) -> IndexPin {
        self.captured
    }

    pub fn destination_pin(&self) -> IndexPin {
        self.destination
    }

    pub fn destination_revision_id(&self) -> &str {
        &self.destination_revision_id
    }

    pub fn descriptor_sha256(&self) -> &str {
        &self.descriptor_sha256
    }

    /// The stage join attempt this batch certified. Publication refuses it once a later
    /// attempt has replaced the stage's output.
    pub fn attempt(&self) -> i64 {
        self.attempt
    }

    pub fn report(&self) -> &serde_json::Value {
        &self.report
    }
}

/// A batch whose descriptor, pages and artifacts passed admission and whose claims are staged.
/// It owns the stage; dropping it removes the stage directory. Only `stage_batch` builds one.
pub struct AdmittedBatch {
    stage: Stage,
    descriptor: Descriptor,
    descriptor_sha256: String,
    producer: PinnedProducer,
    captured: IndexPin,
}

impl AdmittedBatch {
    fn new(
        stage: Stage,
        descriptor: Descriptor,
        descriptor_sha256: String,
        producer: PinnedProducer,
        captured: IndexPin,
    ) -> Self {
        Self {
            stage,
            descriptor,
            descriptor_sha256,
            producer,
            captured,
        }
    }

    pub fn stage(&self) -> &Stage {
        &self.stage
    }

    pub fn stage_mut(&mut self) -> &mut Stage {
        &mut self.stage
    }

    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    pub fn descriptor_sha256(&self) -> &str {
        &self.descriptor_sha256
    }

    pub fn producer(&self) -> &PinnedProducer {
        &self.producer
    }

    pub fn captured_pin(&self) -> IndexPin {
        self.captured
    }

    /// The stage connection together with the admitted batch's read-only parts.
    pub(crate) fn join_parts(&mut self) -> Result<JoinParts<'_>> {
        let conn = self.stage.conn()?;
        Ok(JoinParts {
            conn,
            descriptor: &self.descriptor,
            descriptor_sha256: &self.descriptor_sha256,
            producer: &self.producer,
        })
    }
}

pub(crate) struct JoinParts<'a> {
    pub conn: &'a Connection,
    pub descriptor: &'a Descriptor,
    pub descriptor_sha256: &'a str,
    pub producer: &'a PinnedProducer,
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn lookup_budget_fails_typed_once_the_limit_is_passed() {
        let mut limits = BatchLimits::PRODUCTION;
        limits.lookup_steps = 3;
        let mut budget = LookupBudget::new(&limits);
        budget.charge(2).unwrap();
        budget.charge(1).unwrap();
        assert!(matches!(
            budget.charge(1),
            Err(ScipCaptureError::LimitExceeded(name)) if name == "lookup steps"
        ));
        assert_eq!(budget.used(), 4);
    }
}
