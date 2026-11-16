//! The checkout runtime's search availability and head coverage.
use super::{CheckoutRuntime, CoverageMemo};

impl CheckoutRuntime {
    pub(crate) fn search_availability(&self) -> crate::search_model::SearchAvailability {
        self.search
            .as_ref()
            .map(|search| search.status_handle().availability())
            .unwrap_or_default()
    }

    /// This checkout's retrieval coverage at `pin`, read inside the caller's
    /// selected snapshot (#777), with search's precedence minus the per-query
    /// causes: model diagnostics, then the matrix the owner published for this
    /// pin, then owner work pending (counted from the cache), else
    /// `matrix_not_resident`. Like search, it first rearms an evicted head
    /// (#942), without touching its idle timer. `None` without a search
    /// service or matrix owner.
    /// Without a matrix for `pin`, the head is counted once per pin and the
    /// cache once per cache write (#806), not on every call.
    pub(crate) fn search_head_coverage(
        &self,
        response: &crate::store::EvidenceResponse,
        pin: crate::model::IndexPin,
        mut check: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<Option<crate::search_model::SearchHeadCoverage>> {
        use crate::search_model::{
            SearchCacheDiagnostic, SearchHeadCoverage, SearchIndexState, SearchUnavailableReason,
        };
        let Some(search) = self.search.as_ref() else {
            return Ok(None);
        };
        if let Some(reason) = search.status_handle().unavailable_reason() {
            return Ok(Some(SearchHeadCoverage {
                state: SearchIndexState::Unavailable(reason),
                declarations: self.search_head_declarations(response, pin, check)?,
                embedded: Some(0),
                cache_diagnostic: None,
            }));
        }
        let scheduler = self.browser_scheduler()?;
        let Some(head) = scheduler.search_head_slot(false) else {
            return Ok(None);
        };
        // Like search, rearm an evicted head before sampling it, so both report
        // the same state; unlike search, this is not use and extends no
        // residency (#942, owner ruling 2026-11-12).
        head.rearm_request();
        let model_id = crate::search_embed::model_id_for_card_format(
            crate::search_card::CardFormat::V1.as_str(),
        );
        let running = scheduler.search_fill_building(pin);
        if let Some((declarations, embedded)) = head.coverage(pin, &model_id, search.cache_epoch())
        {
            let declarations = declarations as u64;
            return Ok(Some(SearchHeadCoverage {
                state: if embedded == declarations {
                    SearchIndexState::Complete
                } else if running {
                    SearchIndexState::Building
                } else {
                    SearchIndexState::Partial
                },
                declarations,
                embedded: Some(embedded),
                cache_diagnostic: None,
            }));
        }
        let declarations = self.search_head_declarations(response, pin, check)?;
        if running || head.pending_for(pin, declarations) {
            // Sample the generation before the scan: a write that lands
            // during it leaves this memo keyed to a superseded generation.
            let generation = (search.cache_epoch(), search.vector_writes());
            if let Some(embedded) = self.memo_embedded(pin, generation) {
                return Ok(Some(SearchHeadCoverage {
                    state: SearchIndexState::Building,
                    declarations,
                    embedded: Some(embedded),
                    cache_diagnostic: None,
                }));
            }
            #[cfg(feature = "test-support")]
            self.search_coverage_scans
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let hashes = response.mcp_search_head_card_hashes(pin, &mut check)?;
            // Owner ruling 2026-11-07 (#777): the optional cache never fails
            // describe/status. A cache fault or cache contention leaves the
            // count unknown with a typed diagnostic, so describe never spends
            // its deadline retrying a cache-only read. The request deadline
            // and index reads stay typed errors.
            let (embedded, cache_diagnostic) = match search.cached_embedded(&hashes, None, check) {
                Ok(embedded) => {
                    self.remember_embedded(pin, generation, embedded);
                    (Some(embedded), None)
                }
                Err(error) if error.is::<crate::search_vectors::StoreUnavailable>() => {
                    (None, Some(SearchCacheDiagnostic::VectorCacheUnavailable))
                }
                Err(error) if crate::store::cli_read_retryable_contention(&error) => {
                    (None, Some(SearchCacheDiagnostic::VectorCacheBusy))
                }
                Err(error) => return Err(error),
            };
            return Ok(Some(SearchHeadCoverage {
                state: SearchIndexState::Building,
                declarations,
                embedded,
                cache_diagnostic,
            }));
        }
        Ok(Some(SearchHeadCoverage {
            state: SearchIndexState::Unavailable(SearchUnavailableReason::MatrixNotResident),
            declarations,
            embedded: Some(0),
            cache_diagnostic: None,
        }))
    }

    /// The head's declaration count at `pin`, read once per pin (#806).
    fn search_head_declarations(
        &self,
        response: &crate::store::EvidenceResponse,
        pin: crate::model::IndexPin,
        mut check: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<u64> {
        if let Some(memo) = self.search_coverage.lock().unwrap().as_ref()
            && memo.pin == pin
        {
            return Ok(memo.declarations);
        }
        let declarations = response.mcp_search_head_count(pin, &mut check)? as u64;
        let mut memo = self.search_coverage.lock().unwrap();
        if memo.as_ref().is_none_or(|memo| memo.pin != pin) {
            *memo = Some(CoverageMemo {
                pin,
                declarations,
                embedded: None,
            });
        }
        Ok(declarations)
    }

    fn memo_embedded(&self, pin: crate::model::IndexPin, generation: (u64, u64)) -> Option<u64> {
        let memo = self.search_coverage.lock().unwrap();
        let memo = memo.as_ref().filter(|memo| memo.pin == pin)?;
        memo.embedded
            .filter(|(epoch, writes, _)| (*epoch, *writes) == generation)
            .map(|(_, _, embedded)| embedded)
    }

    fn remember_embedded(
        &self,
        pin: crate::model::IndexPin,
        generation: (u64, u64),
        embedded: u64,
    ) {
        if let Some(memo) = self.search_coverage.lock().unwrap().as_mut()
            && memo.pin == pin
        {
            memo.embedded = Some((generation.0, generation.1, embedded));
        }
    }
}
