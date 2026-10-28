//! Bounded streamed semantic batch import: resource policy and the high-water counters that prove it.
pub mod limits;

pub use limits::BatchLimits;

#[cfg(test)]
mod limits_tests;

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
