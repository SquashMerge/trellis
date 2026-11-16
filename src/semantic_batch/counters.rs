//! The high-water counters that prove the batch resource policy in `limits`, and the lookup
//! budget of the staged join.
use super::limits::BatchLimits;
use crate::scip_raw::ScipCaptureError;

/// Counters threaded through batch staging and decoding. The `*_high_water*` fields are the
/// AC5 test seam; `wire_fields` and `candidates` are the batch-shared decoder walk budget.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchCounters {
    /// Work per resumable retained-admission unit; no producer-sized decode is hidden here.
    pub retained_admission_units: u64,
    pub retained_admission_rows_high_water: u64,
    pub retained_admission_bytes_high_water: u64,
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

#[cfg(test)]
mod tests {
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
