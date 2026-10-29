//! Published per-checkout retrieval state. Only the search-fill driver mutates it.
#[cfg(any(test, feature = "test-support"))]
use crate::search_embed::QueryBudget;
use crate::{
    model::IndexPin,
    search_embed::EmbedError,
    search_rank::{SearchCandidate, SearchCandidateKey, SearchMatrix, SearchRanking},
};
use anyhow::Result;
#[cfg(any(test, feature = "test-support"))]
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex, Weak, atomic::Ordering};

pub enum HeadChange {
    Full(Arc<[SearchCandidate]>),
    Delta {
        removed_version_ids: Vec<i64>,
        added: Arc<[SearchCandidate]>,
    },
}
pub struct HeadUpdate {
    pub pin: IndexPin,
    pub declarations: usize,
    pub change: HeadChange,
}
struct Published {
    pin: IndexPin,
    model_id: String,
    epoch: u64,
    matrix: SearchMatrix,
    charge: Option<super::search_fill::MatrixCharge>,
    scratch: Option<Arc<super::search_fill::MatrixCharge>>,
}
#[derive(Default)]
struct State {
    published: Option<Published>,
    building: bool,
    stopped: bool,
    evicted: bool,
    failure: Option<HeadFailure>,
    declined: Option<(usize, String, u64)>,
    used_at: Option<std::time::Instant>,
}
enum HeadFailure {
    Store,
    Other(String),
}
impl HeadFailure {
    fn from_error(error: &anyhow::Error) -> Self {
        if error.is::<crate::search_vectors::StoreUnavailable>()
            || error.is::<crate::store::SelectedIntegrity>()
            || crate::search_vectors::is_corrupt_cache(error)
        {
            Self::Store
        } else {
            Self::Other(error.to_string())
        }
    }
    fn error(&self) -> anyhow::Error {
        match self {
            Self::Store => crate::search_vectors::StoreUnavailable.into(),
            Self::Other(message) => anyhow::anyhow!("{message}"),
        }
    }
}

pub struct HeadSlot {
    state: Mutex<State>,
    changed: Condvar,
    owner: Weak<super::search_fill::Core>,
    id: std::sync::atomic::AtomicU64,
    search: Weak<super::search::SearchService>,
    touched: std::sync::atomic::AtomicU64,
    #[cfg(any(test, feature = "test-support"))]
    before_rank: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}
impl HeadSlot {
    pub(super) fn new(
        owner: Weak<super::search_fill::Core>,
        id: u64,
        search: Weak<super::search::SearchService>,
    ) -> Self {
        Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            owner,
            id: std::sync::atomic::AtomicU64::new(id),
            search,
            touched: std::sync::atomic::AtomicU64::new(0),
            #[cfg(any(test, feature = "test-support"))]
            before_rank: Mutex::new(None),
        }
    }
    /// A hint, not a query-owned job. Dropping a waiter never withdraws it.
    pub fn request(&self) {
        self.touch();
        if let Some(owner) = self.owner.upgrade() {
            owner.request(self.id.load(Ordering::Acquire));
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn wait_latest(&self, cancel: &AtomicBool, budget: &QueryBudget) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(error) = &state.failure {
                return Err(error.error());
            }
            if state.stopped {
                return Ok(());
            }
            if cancel.load(Ordering::Acquire) {
                return Err(EmbedError::Cancelled.into());
            }
            budget.check()?;
            let usable = state.published.as_ref().is_some_and(|published| {
                self.search
                    .upgrade()
                    .is_none_or(|search| published.epoch == search.cache_epoch())
            });
            if usable || !state.building {
                return Ok(());
            }
            state = self
                .changed
                .wait_timeout(state, budget.remaining())
                .unwrap()
                .0;
        }
    }
    fn touch(&self) {
        let mut state = self.state.lock().unwrap();
        state.used_at = Some(
            self.search
                .upgrade()
                .map_or_else(std::time::Instant::now, |search| search.idle_now()),
        );
        if let Some(owner) = self.owner.upgrade() {
            self.touched.store(owner.touch(), Ordering::Relaxed);
        }
    }
    pub(super) fn expire_idle(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        let mut state = self.state.lock().unwrap();
        state.published.as_ref()?;
        let used_at = state.used_at?;
        let remaining = std::time::Duration::from_secs(600)
            .saturating_sub(now.saturating_duration_since(used_at));
        if remaining.is_zero() {
            state.published = None;
            state.evicted = true;
            self.changed.notify_all();
            None
        } else {
            Some(remaining)
        }
    }
    pub(super) fn last_touch(&self) -> u64 {
        self.touched.load(Ordering::Relaxed)
    }
    pub(super) fn startup_at_cap(&self) -> bool {
        self.owner
            .upgrade()
            .is_some_and(|owner| owner.startup_at_cap())
    }
    /// Readiness only; callers must still rank against their admitted exact pin.
    pub fn resident(&self) -> Result<bool> {
        self.touch();
        let search = self.search.upgrade();
        let identity = search.as_ref().map(|search| {
            (
                crate::search_embed::model_id_for_card_format(
                    crate::search_card::CardFormat::V1.as_str(),
                ),
                search.cache_epoch(),
            )
        });
        let state = self.state.lock().unwrap();
        if let Some(error) = &state.failure {
            return Err(error.error());
        }
        Ok(state.published.as_ref().is_some_and(|p| {
            identity
                .as_ref()
                .is_none_or(|(model, epoch)| p.model_id == *model && p.epoch == *epoch)
        }))
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_before_rank_hook_for_tests(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.before_rank.lock().unwrap() = Some(hook);
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn clear_for_tests(&self) {
        self.clear();
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_store_fault_for_tests(&self) {
        self.finish(&Err(crate::search_vectors::StoreUnavailable.into()));
    }
    pub fn fault(&self) -> Result<()> {
        let state = self.state.lock().unwrap();
        if let Some(error) = &state.failure {
            return Err(error.error());
        }
        Ok(())
    }
    pub fn declarations(&self, pin: IndexPin, model_id: &str, epoch: u64) -> Result<Option<usize>> {
        let state = self.state.lock().unwrap();
        if let Some(error) = &state.failure {
            return Err(error.error());
        }
        Ok(state
            .published
            .as_ref()
            .filter(|p| p.pin == pin && p.model_id == model_id && p.epoch == epoch)
            .map(|p| p.matrix.active_count()))
    }
    #[allow(clippy::too_many_arguments)] // Keep admitted pin/model/epoch and per-query policy explicit.
    pub fn rank(
        &self,
        pin: IndexPin,
        model_id: &str,
        epoch: u64,
        lexical: &[SearchCandidateKey],
        query: Option<&[i8]>,
        policy: &crate::search_rank::SearchPolicy,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<SearchRanking>> {
        self.touch();
        let search = self.search.upgrade();
        let _use = match search.as_ref() {
            Some(search) => match search.begin_head_use() {
                Some(usage) => Some(usage),
                None => return Ok(None),
            },
            None => None,
        };
        #[cfg(any(test, feature = "test-support"))]
        {
            let hook = self.before_rank.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let state = self.state.lock().unwrap();
        if let Some(error) = &state.failure {
            return Err(error.error());
        }
        let Some(published) = state
            .published
            .as_ref()
            .filter(|p| p.pin == pin && p.model_id == model_id && p.epoch == epoch)
        else {
            return Ok(None);
        };
        let ranked =
            crate::search_rank::rank_matrix(&published.matrix, lexical, query, policy, check)?;
        drop(state);
        Ok(Some(ranked))
    }
    pub(super) fn rebind(&self, id: u64) {
        self.id.store(id, Ordering::Release);
    }
    pub(super) fn begin(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.stopped {
            state.building = true;
        }
        self.changed.notify_all();
    }
    pub(super) fn finish(&self, result: &Result<()>) {
        let mut state = self.state.lock().unwrap();
        state.building = result.as_ref().is_err_and(|error| {
            error.is::<crate::store::topology::StorageBusy>()
                || error.is::<crate::store::SqliteContention>()
        });
        if let Err(error) = result
            && !error.is::<crate::store::topology::StorageBusy>()
            && !error.is::<crate::store::SqliteContention>()
            && !matches!(
                error.downcast_ref::<EmbedError>(),
                Some(
                    EmbedError::Cancelled
                        | EmbedError::ModelUnavailable(_)
                        | EmbedError::Runtime(_)
                )
            )
            && !error.is::<crate::store::SearchHeadChanged>()
            && !error.is::<super::search_fill::SearchDeferred>()
            && !error.is::<super::search_fill::MemoryDeclined>()
        {
            state.failure = Some(HeadFailure::from_error(error));
        }
        self.changed.notify_all();
    }
    pub(super) fn settle(&self) {
        self.state.lock().unwrap().building = false;
        self.changed.notify_all();
    }
    pub(super) fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        state.stopped = true;
        state.building = false;
        state.published = None;
        self.changed.notify_all();
    }
    pub(super) fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.published = None;
        state.evicted = true;
        self.changed.notify_all();
    }
    pub(super) fn rearm(&self) {
        self.state.lock().unwrap().evicted = false;
    }
    pub(super) fn fill_scratch(&self) -> Option<Arc<super::search_fill::MatrixCharge>> {
        self.state
            .lock()
            .unwrap()
            .published
            .as_ref()
            .and_then(|p| p.scratch.clone())
    }
    pub(super) fn can_retry_cold(&self) -> bool {
        let state = self.state.lock().unwrap();
        !state.stopped && state.declined.is_none()
    }
    pub(super) fn discard_for_cold(&self) {
        self.state.lock().unwrap().published = None;
    }
    pub(super) fn dormant(&self) -> bool {
        self.state.lock().unwrap().evicted
    }
    pub(super) fn needs_build(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.failure.is_some()
            || (state.declined.as_ref().is_none_or(|(_, _, epoch)| {
                self.search
                    .upgrade()
                    .is_some_and(|search| *epoch != search.cache_epoch())
            }) && state.published.as_ref().is_none_or(|published| {
                self.search
                    .upgrade()
                    .is_some_and(|search| published.epoch != search.cache_epoch())
            }))
    }
    pub(super) fn reserve_candidates(
        &self,
        added: usize,
        removed: usize,
        full: bool,
        model_id: &str,
        epoch: u64,
        startup: bool,
    ) -> Result<Option<super::search_fill::MatrixCharge>> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(None);
        };
        if full
            && self
                .state
                .lock()
                .unwrap()
                .declined
                .as_ref()
                .is_some_and(|(count, model, cache)| {
                    *count == added && model == model_id && *cache == epoch
                })
        {
            return Err(super::search_fill::MemoryDeclined.into());
        }
        // Stored syntax IDs decode to 39 ASCII bytes; card hashes have 64.
        // Vec -> Arc temporarily keeps both identity arrays, not string clones.
        let bytes = added
            .saturating_mul(2 * std::mem::size_of::<SearchCandidate>() + 39 + 64)
            .saturating_add(removed.saturating_mul(std::mem::size_of::<i64>()))
            .saturating_add(2 * std::mem::size_of::<usize>());
        if full
            && !owner.can_fit_alone(
                SearchMatrix::estimated_new_build_bytes_for_counts(
                    added,
                    added.saturating_mul(39),
                    added.saturating_mul(64),
                )
                .saturating_add(bytes)
                .saturating_add(super::search_fill::MATRIX_FILL_SCRATCH),
            )
        {
            let mut state = self.state.lock().unwrap();
            state.declined = Some((added, model_id.to_owned(), epoch));
            state.published = None;
            state.building = false;
            self.changed.notify_all();
            return Err(super::search_fill::MemoryDeclined.into());
        }
        let id = self.id.load(Ordering::Acquire);
        owner
            .reserve(id, bytes, startup)
            .or_else(|| {
                if full && !startup && owner.can_fit_alone(bytes) {
                    self.state.lock().unwrap().published = None;
                    owner.reserve(id, bytes, false)
                } else {
                    None
                }
            })
            .map(Some)
            .ok_or_else(|| super::search_fill::MemoryDeclined.into())
    }
    #[cfg(test)]
    pub(super) fn storage_bytes(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .published
            .as_ref()
            .map_or(0, |p| p.matrix.storage_bytes())
    }
    pub(super) fn previous(&self, model_id: &str, epoch: u64) -> Option<IndexPin> {
        self.state
            .lock()
            .unwrap()
            .published
            .as_ref()
            .filter(|p| p.model_id == model_id && p.epoch == epoch)
            .map(|p| p.pin)
    }
    #[cfg(test)]
    pub(super) fn build(
        &self,
        update: HeadUpdate,
        model_id: &str,
        epoch: u64,
        snapshot: Option<&crate::search_vectors::VectorSnapshot<'_>>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        self.build_mode(update, model_id, epoch, snapshot, false, check)
    }
    pub(super) fn build_mode(
        &self,
        update: HeadUpdate,
        model_id: &str,
        epoch: u64,
        snapshot: Option<&crate::search_vectors::VectorSnapshot<'_>>,
        startup: bool,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        check()?;
        let estimate = {
            let state = self.state.lock().unwrap();
            if state.stopped {
                return Err(EmbedError::Cancelled.into());
            }
            if state
                .declined
                .as_ref()
                .is_some_and(|(count, model, cache)| {
                    *count == update.declarations && model == model_id && *cache == epoch
                })
            {
                return Err(super::search_fill::MemoryDeclined.into());
            }
            match &update.change {
                HeadChange::Full(cards) => {
                    SearchMatrix::estimated_new_build_bytes_for_counts(cards.len(), 0, 0)
                        .saturating_add(super::search_fill::MATRIX_FILL_SCRATCH)
                }
                HeadChange::Delta {
                    removed_version_ids,
                    added,
                } => {
                    let old = state
                        .published
                        .as_ref()
                        .ok_or(crate::store::SearchHeadChanged)?;
                    old.matrix
                        .estimated_delta_candidate_bytes(removed_version_ids, added)
                }
            }
        };
        let mut charge = if let Some(owner) = self.owner.upgrade() {
            let id = self.id.load(Ordering::Acquire);
            let reservation = owner.reserve(id, estimate, startup).or_else(|| {
                if !startup
                    && owner.can_fit_alone(estimate)
                    && matches!(&update.change, HeadChange::Full(_))
                {
                    // A cold replacement may discard the old head before it
                    // allocates the successor, but never stops accounting it
                    // while both allocations physically exist.
                    self.state.lock().unwrap().published = None;
                    owner.reserve(id, estimate, false)
                } else {
                    None
                }
            });
            match reservation {
                Some(charge) => Some(charge),
                None => {
                    let mut state = self.state.lock().unwrap();
                    state.published = None;
                    if matches!(&update.change, HeadChange::Full(_))
                        && !owner.can_fit_alone(estimate)
                    {
                        state.declined = Some((update.declarations, model_id.to_owned(), epoch));
                    }
                    state.building = false;
                    self.changed.notify_all();
                    return Err(super::search_fill::MemoryDeclined.into());
                }
            }
        } else {
            None
        };
        let mut retained_scratch = None;
        let matrix = match update.change {
            HeadChange::Full(cards) => {
                SearchMatrix::from_snapshot(cards, snapshot, model_id, check)?
            }
            HeadChange::Delta {
                removed_version_ids,
                added,
            } => {
                let mut prepared = Vec::with_capacity(added.len());
                for card in added.iter() {
                    check()?;
                    let vector = snapshot
                        .map(|s| s.get(model_id, &card.card_hash))
                        .transpose()?
                        .flatten();
                    prepared.push((card.clone(), vector));
                }
                let mut published = self
                    .state
                    .lock()
                    .unwrap()
                    .published
                    .take()
                    .ok_or(crate::store::SearchHeadChanged)?;
                if let (Some(charge), Some(old)) = (&mut charge, published.charge.take()) {
                    charge.absorb(old);
                }
                retained_scratch = published.scratch.take();
                published
                    .matrix
                    .apply_delta(&removed_version_ids, prepared, check)?;
                published
                    .matrix
                    .refresh_missing(snapshot, model_id, check)?;
                published.matrix
            }
        };
        anyhow::ensure!(
            matrix.active_count() == update.declarations,
            crate::store::SelectedIntegrity("store_unavailable: matrix head count mismatch".into())
        );
        check()?;
        let mut state = self.state.lock().unwrap();
        if state.stopped {
            return Err(EmbedError::Cancelled.into());
        }
        let new_scratch = if retained_scratch.is_some() {
            0
        } else {
            super::search_fill::MATRIX_FILL_SCRATCH
        };
        if let Some(charge) = &mut charge {
            charge.shrink(matrix.storage_bytes().saturating_add(new_scratch))?;
        }
        let scratch = retained_scratch.or_else(|| {
            charge
                .as_mut()
                .map(|charge| Arc::new(charge.split(super::search_fill::MATRIX_FILL_SCRATCH)))
        });
        state.declined = None;
        state.used_at = Some(
            self.search
                .upgrade()
                .map_or_else(std::time::Instant::now, |search| search.idle_now()),
        );
        state.published = Some(Published {
            pin: update.pin,
            model_id: model_id.to_owned(),
            epoch,
            matrix,
            charge,
            scratch,
        });
        state.failure = None;
        // Matrix coverage is published independently from later document fill.
        state.building = false;
        self.changed.notify_all();
        Ok(())
    }
    pub(super) fn committed(
        &self,
        model_id: &str,
        epoch: u64,
        vectors: &[(String, Vec<i8>)],
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if let Some(published) = state.published.as_mut() {
            if published.model_id == model_id && published.epoch == epoch {
                published.matrix.update_vectors(vectors, &mut || Ok(()))?;
                if let Some(charge) = &mut published.charge {
                    charge.shrink(published.matrix.storage_bytes())?;
                }
            } else {
                state.published = None;
            }
        }
        self.changed.notify_all();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_rank::{SearchCandidate, SearchCandidateKey};
    use std::time::{Duration, Instant};
    fn pin(revision: u64) -> IndexPin {
        IndexPin {
            index_generation: uuid::Uuid::nil(),
            index_revision: revision,
        }
    }
    fn slot() -> HeadSlot {
        HeadSlot::new(Weak::new(), 1, Weak::new())
    }
    fn update(revision: u64) -> HeadUpdate {
        HeadUpdate {
            pin: pin(revision),
            declarations: 1,
            change: HeadChange::Full(
                vec![SearchCandidate {
                    key: SearchCandidateKey {
                        version_id: 1,
                        syntax_id: "sid:v1:00000000000000000000000000000001".into(),
                    },
                    card_hash: "a".repeat(64),
                    role: crate::native_evidence::DeclarationRole::Production,
                }]
                .into(),
            ),
        }
    }
    #[test]
    fn resident_idle_deadline_is_per_head_and_refreshes_on_use() {
        let first = slot();
        let second = slot();
        first
            .build(update(1), "model", 1, None, &mut || Ok(()))
            .unwrap();
        second
            .build(update(1), "model", 1, None, &mut || Ok(()))
            .unwrap();
        let now = std::time::Instant::now();
        first.state.lock().unwrap().used_at = Some(now - Duration::from_secs(600));
        second.state.lock().unwrap().used_at = Some(now - Duration::from_secs(599));
        assert_eq!(first.expire_idle(now), None);
        assert_eq!(first.declarations(pin(1), "model", 1).unwrap(), None);
        assert_eq!(second.expire_idle(now), Some(Duration::from_secs(1)));
        second.touch();
        assert!(second.expire_idle(now + Duration::from_secs(2)).is_some());
        assert_eq!(second.declarations(pin(1), "model", 1).unwrap(), Some(1));
    }
    #[test]
    fn budget_fallback_does_not_stop_background_publication() {
        let head = slot();
        head.begin();
        let now = Instant::now();
        let budget = QueryBudget::new(now, Arc::new(move || now));
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            head.wait_latest(&cancel, &budget)
                .unwrap_err()
                .downcast_ref::<EmbedError>(),
            Some(EmbedError::BudgetExceeded)
        ));
        assert!(!cancel.load(Ordering::Acquire));
        head.build(update(1), "model", 1, None, &mut || Ok(()))
            .unwrap();
        assert_eq!(head.declarations(pin(1), "model", 1).unwrap(), Some(1));
        let later = QueryBudget::new(now + Duration::from_secs(3), Arc::new(move || now));
        head.wait_latest(&cancel, &later).unwrap();
    }
    #[test]
    fn publication_requires_exact_pin_model_and_epoch() {
        let head = slot();
        head.build(update(1), "model", 2, None, &mut || Ok(()))
            .unwrap();
        assert_eq!(head.declarations(pin(2), "model", 2).unwrap(), None);
        assert_eq!(head.declarations(pin(1), "other", 2).unwrap(), None);
        assert_eq!(head.declarations(pin(1), "model", 3).unwrap(), None);
        head.clear();
        assert_eq!(head.declarations(pin(1), "model", 2).unwrap(), None);
    }
    #[test]
    fn observed_store_fault_wins_over_budget_and_old_matrix() {
        let head = slot();
        head.build(update(1), "model", 1, None, &mut || Ok(()))
            .unwrap();
        head.finish(&Err(crate::search_vectors::StoreUnavailable.into()));
        let now = Instant::now();
        let budget = QueryBudget::new(now, Arc::new(move || now));
        assert!(
            head.wait_latest(&AtomicBool::new(false), &budget)
                .unwrap_err()
                .is::<crate::search_vectors::StoreUnavailable>()
        );
        assert!(
            head.declarations(pin(1), "model", 1)
                .unwrap_err()
                .is::<crate::search_vectors::StoreUnavailable>()
        );
        head.build(update(2), "model", 1, None, &mut || Ok(()))
            .unwrap();
        assert_eq!(head.declarations(pin(2), "model", 1).unwrap(), Some(1));
    }
    #[test]
    fn stopped_slot_cannot_publish_or_retain_matrix() {
        let head = slot();
        head.stop();
        assert!(
            head.build(update(1), "model", 1, None, &mut || Ok(()))
                .is_err()
        );
        assert_eq!(head.declarations(pin(1), "model", 1).unwrap(), None);
    }
    #[test]
    fn delta_and_committed_fill_publish_one_head() {
        let head = slot();
        head.build(update(1), "model", 1, None, &mut || Ok(()))
            .unwrap();
        head.committed(
            "model",
            1,
            &[("a".repeat(64), vec![1; crate::search_embed::DIMENSIONS])],
        )
        .unwrap();
        let first = head
            .rank(
                pin(1),
                "model",
                1,
                &[],
                Some(&vec![1; crate::search_embed::DIMENSIONS]),
                &crate::search_rank::SearchPolicy::default(),
                &mut || Ok(()),
            )
            .unwrap()
            .unwrap();
        assert_eq!(first.embedded, 1);
        let added = SearchCandidate {
            key: SearchCandidateKey {
                version_id: 2,
                syntax_id: "sid:v1:00000000000000000000000000000002".into(),
            },
            card_hash: "b".repeat(64),
            role: crate::native_evidence::DeclarationRole::Production,
        };
        head.begin();
        head.build(
            HeadUpdate {
                pin: pin(2),
                declarations: 1,
                change: HeadChange::Delta {
                    removed_version_ids: vec![1],
                    added: vec![added].into(),
                },
            },
            "model",
            1,
            None,
            &mut || Ok(()),
        )
        .unwrap();
        assert!(
            head.rank(
                pin(1),
                "model",
                1,
                &[],
                None,
                &crate::search_rank::SearchPolicy::default(),
                &mut || Ok(())
            )
            .unwrap()
            .is_none()
        );
        // An old hash arriving after deletion cannot fill a reused slot.
        head.committed(
            "model",
            1,
            &[("a".repeat(64), vec![1; crate::search_embed::DIMENSIONS])],
        )
        .unwrap();
        assert_eq!(
            head.rank(
                pin(2),
                "model",
                1,
                &[],
                None,
                &crate::search_rank::SearchPolicy::default(),
                &mut || Ok(())
            )
            .unwrap()
            .unwrap()
            .embedded,
            0
        );
        head.committed(
            "model",
            1,
            &[("b".repeat(64), vec![1; crate::search_embed::DIMENSIONS])],
        )
        .unwrap();
        assert_eq!(
            head.rank(
                pin(2),
                "model",
                1,
                &[],
                None,
                &crate::search_rank::SearchPolicy::default(),
                &mut || Ok(())
            )
            .unwrap()
            .unwrap()
            .embedded,
            1
        );
    }
    #[test]
    fn cancelled_waiter_does_not_cancel_another_waiter_or_build() {
        let head = slot();
        head.begin();
        let now = Instant::now();
        let budget = QueryBudget::new(now + Duration::from_secs(3), Arc::new(move || now));
        assert!(
            head.wait_latest(&AtomicBool::new(true), &budget)
                .unwrap_err()
                .is::<EmbedError>()
        );
        head.build(update(1), "model", 1, None, &mut || Ok(()))
            .unwrap();
        head.wait_latest(&AtomicBool::new(false), &budget).unwrap();
    }
}
