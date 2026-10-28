//! Bounded streamed semantic batch import: resource policy and the high-water counters that prove it.
use crate::{model::IndexPin, scip_admission::PinnedProducer};
use descriptor::Descriptor;
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
    pub document_cache_high_water_bytes: u64,
    pub document_cache_high_water_entries: u64,
    pub active_document_groups_high_water: u64,
    pub candidate_group_high_water: u64,
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
}
