//! One daemon-wide owner for disposable, post-publication vector filling.
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use super::{
    search::SearchService,
    search_head::{HeadSlot, HeadUpdate},
};
use crate::{
    http::DaemonState,
    model::IndexPin,
    search_card::CardFormat,
    search_embed::{
        DIMENSIONS, EmbedError, EmbeddingControl, InputKind, SearchEmbedder,
        model_id_for_card_format,
    },
    search_vectors::VectorCacheReader,
    store::{HeadSearchCards, SEARCH_CARD_BATCH_SIZE, SearchHeadChanged, topology::StorageBusy},
};
use anyhow::Result;

const BATCH: usize = SEARCH_CARD_BATCH_SIZE;
const FAULT_RETRY: Duration = Duration::from_millis(250);
const MAX_FAULT_RETRIES: usize = 3;
const MATRIX_RAM_LIMIT: usize = 2 * 1024 * 1024 * 1024;
// One existing driver reuses this resident reservation for bounded fill batches.
pub(super) const MATRIX_FILL_SCRATCH: usize = BATCH * (3 * DIMENSIONS + 256);

/// Accounted bytes include unpublished builds, not just query-visible matrices.
pub(super) struct MatrixBudget(Mutex<BudgetState>);
struct BudgetState {
    limit: usize,
    used: usize,
}
impl Default for MatrixBudget {
    fn default() -> Self {
        Self(Mutex::new(BudgetState {
            limit: MATRIX_RAM_LIMIT,
            used: 0,
        }))
    }
}
pub(super) struct MatrixCharge {
    budget: Arc<MatrixBudget>,
    bytes: usize,
}
impl MatrixBudget {
    fn reserve(self: &Arc<Self>, bytes: usize) -> Option<MatrixCharge> {
        let mut state = self.0.lock().unwrap();
        if bytes > state.limit.saturating_sub(state.used) {
            return None;
        }
        state.used += bytes;
        Some(MatrixCharge {
            budget: self.clone(),
            bytes,
        })
    }
}
impl MatrixCharge {
    pub(super) fn split(&mut self, bytes: usize) -> Self {
        assert!(bytes <= self.bytes);
        self.bytes -= bytes;
        Self {
            budget: self.budget.clone(),
            bytes,
        }
    }
    pub(super) fn absorb(&mut self, mut old: Self) {
        self.bytes += old.bytes;
        old.bytes = 0;
    }
    pub(super) fn shrink(&mut self, bytes: usize) -> Result<()> {
        if bytes > self.bytes {
            return Err(MatrixEstimateExceeded {
                reserved: self.bytes,
                actual: bytes,
            }
            .into());
        }
        self.budget.0.lock().unwrap().used -= self.bytes - bytes;
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for MatrixCharge {
    fn drop(&mut self) {
        self.budget.0.lock().unwrap().used -= self.bytes;
    }
}
#[derive(Debug)]
pub(super) struct MatrixEstimateExceeded {
    pub(super) reserved: usize,
    pub(super) actual: usize,
}
impl std::fmt::Display for MatrixEstimateExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "search matrix allocation exceeded reservation ({} > {} bytes)",
            self.actual, self.reserved
        )
    }
}
impl std::error::Error for MatrixEstimateExceeded {}

#[derive(Debug)]
pub(super) struct MemoryDeclined;
impl std::fmt::Display for MemoryDeclined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("search matrix is not resident")
    }
}
impl std::error::Error for MemoryDeclined {}

/// A live checkout cannot supply background work during a transient owner phase.
/// It is not a storage fault and only a later explicit hint retries it.
#[derive(Debug)]
pub(crate) struct SearchDeferred;
impl std::fmt::Display for SearchDeferred {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("search background work deferred")
    }
}
impl std::error::Error for SearchDeferred {}

type Source = Weak<dyn FillSource>;
pub(crate) trait FillSource: Send + Sync {
    fn is_startup(&self) -> bool {
        false
    }
    fn head(
        &self,
        _previous: Option<IndexPin>,
        _admit: &mut dyn FnMut(usize, usize, bool) -> Result<()>,
        _check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Option<HeadUpdate>> {
        Ok(None)
    }
    fn root_key(&self) -> Option<String> {
        None
    }
    fn page(&self, pin: Option<IndexPin>, after: Option<&str>) -> Result<HeadSearchCards>;
}
impl FillSource for DaemonState {
    fn root_key(&self) -> Option<String> {
        Some(self.search_root_key())
    }
    fn head(
        &self,
        previous: Option<IndexPin>,
        admit: &mut dyn FnMut(usize, usize, bool) -> Result<()>,
        check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Option<HeadUpdate>> {
        self.search_head_update(previous, admit, check)
    }
    fn page(&self, pin: Option<IndexPin>, after: Option<&str>) -> Result<HeadSearchCards> {
        self.search_fill_page(pin, after, BATCH)
    }
}

#[derive(Clone, Copy)]
struct Completed {
    pin: IndexPin,
    cache_epoch: u64,
}

struct Outcome {
    pin: Option<IndexPin>,
    cache_epoch: u64,
    result: Result<()>,
}
trait FillBackend: Send + Sync {
    fn search(&self) -> Weak<SearchService> {
        Weak::new()
    }
    fn prepare(
        &self,
        _source: &dyn FillSource,
        _cancel: &AtomicBool,
        _head: &HeadSlot,
    ) -> Result<()> {
        Ok(())
    }
    fn idle_now(&self) -> Instant {
        Instant::now()
    }
    fn idle_remaining(&self) -> Option<Duration> {
        None
    }
    fn begin_idle_expiry(&self) -> bool {
        false
    }
    fn finish_idle_expiry(&self) {}

    fn run(
        &self,
        source: &dyn FillSource,
        cancel: &AtomicBool,
        completed: Option<Completed>,
        publish: &dyn Fn(IndexPin),
    ) -> Outcome;
}
struct LocalBackend(Arc<SearchService>);
impl FillBackend for LocalBackend {
    fn search(&self) -> Weak<SearchService> {
        Arc::downgrade(&self.0)
    }
    fn prepare(&self, source: &dyn FillSource, cancel: &AtomicBool, head: &HeadSlot) -> Result<()> {
        let model_id = model_id_for_card_format(CardFormat::V1.as_str());
        if source.is_startup() && head.startup_at_cap() {
            return Err(MemoryDeclined.into());
        }
        if head.dormant() {
            return Err(MemoryDeclined.into());
        }
        if !self.0.model_configured() {
            return Ok(());
        }
        self.0.with_vector_writer(|_| Ok(()))?;
        let previous = head.previous(&model_id, self.0.cache_epoch());
        let prepare = |previous, startup| {
            let mut candidates_charge = None;
            let update = source.head(
                previous,
                &mut |added, removed, full| {
                    candidates_charge = head.reserve_candidates(
                        added,
                        removed,
                        full,
                        &model_id,
                        self.0.cache_epoch(),
                        startup,
                    )?;
                    Ok(())
                },
                &mut || check_cancel(cancel),
            )?;
            let Some(update) = update else {
                return Err(SearchDeferred.into());
            };
            check_cancel(cancel)?;
            if previous == Some(update.pin) && !head.needs_build() {
                head.settle();
                return Ok(());
            }
            // No checkout snapshot or owner inbox survives model loading.
            self.0.with_model_job(cancel, |_| Ok(()))?;
            let epoch = self.0.cache_epoch();
            let result = self.0.with_vector_snapshot(|snapshot| {
                head.build_mode(update, &model_id, epoch, snapshot, startup, &mut || {
                    check_cancel(cancel)
                })
            });
            drop(candidates_charge);
            result
        };
        let built = prepare(previous, source.is_startup());
        if built
            .as_ref()
            .is_err_and(|error| error.is::<MemoryDeclined>())
            && previous.is_some()
            && !source.is_startup()
            && head.can_retry_cold()
        {
            // Admission can fail before candidate allocation or during patch
            // staging. Release the old matrix before one same-job cold attempt.
            head.discard_for_cold();
            return prepare(None, false);
        }
        built
    }
    fn idle_now(&self) -> Instant {
        self.0.idle_now()
    }
    fn idle_remaining(&self) -> Option<Duration> {
        self.0.idle_remaining()
    }
    fn begin_idle_expiry(&self) -> bool {
        self.0.begin_idle_expiry()
    }
    fn finish_idle_expiry(&self) {
        self.0.finish_idle_expiry();
    }

    fn run(
        &self,
        source: &dyn FillSource,
        cancel: &AtomicBool,
        completed: Option<Completed>,
        publish: &dyn Fn(IndexPin),
    ) -> Outcome {
        let mut pin = None;
        let mut cache_epoch = self.0.cache_epoch();
        // One reader/connection per head job, with a fresh transaction per page.
        // Transactions end before inference/writes, so own inserts are visible.
        let mut reader = None;
        let result = (|| {
            check_cancel(cancel)?;
            let mut page = source.page(None, None)?;
            pin = Some(page.pin);
            if completed.is_some_and(|done| {
                done.pin == page.pin && done.cache_epoch == self.0.cache_epoch()
            }) {
                return Ok(());
            }
            publish(page.pin);
            if !self.0.model_configured() {
                return Err(EmbedError::ModelUnavailable(
                    "no local search model is configured".into(),
                )
                .into());
            }
            let model_id = model_id_for_card_format(CardFormat::V1.as_str());
            self.0.with_vector_writer(|_| Ok(()))?;
            cache_epoch = self.0.cache_epoch();
            reader = self.0.fill_reader()?;
            loop {
                self.0.expire_idle_matrices();
                check_cancel(cancel)?;
                let missing = Self::missing(reader.as_mut(), &model_id, &page)?;
                if missing.iter().any(|value| *value) {
                    return self.0.with_model_job(cancel, |model| {
                        Ok(self.fill_loaded(
                            source,
                            cancel,
                            model,
                            &model_id,
                            (page, missing),
                            reader.as_mut(),
                        ))
                    })?;
                }
                let Some(after) = next_cursor(&page) else {
                    return Ok(());
                };
                page = source.page(pin, Some(&after))?;
            }
        })();
        drop(reader);
        self.0.invalidate_corrupt_cache(&result);
        Outcome {
            pin,
            cache_epoch,
            result,
        }
    }
}
impl LocalBackend {
    fn missing(
        reader: Option<&mut VectorCacheReader>,
        model_id: &str,
        page: &HeadSearchCards,
    ) -> Result<Vec<bool>> {
        let Some(reader) = reader else {
            return Ok(vec![true; page.cards.len()]);
        };
        let snapshot = reader.snapshot()?;
        page.cards
            .iter()
            .map(|card| {
                snapshot
                    .get(model_id, &card.card_hash)
                    .map(|vector| vector.is_none_or(|vector| vector.len() != DIMENSIONS))
            })
            .collect()
    }

    fn fill_loaded(
        &self,
        source: &dyn FillSource,
        cancel: &AtomicBool,
        model: &dyn SearchEmbedder,
        model_id: &str,
        first_page: (HeadSearchCards, Vec<bool>),
        mut reader: Option<&mut VectorCacheReader>,
    ) -> Result<()> {
        let (mut page, mut missing) = first_page;
        loop {
            self.0.expire_idle_matrices();
            self.0.serve_queries(model);
            check_cancel(cancel)?;
            let mut hashes = std::collections::BTreeSet::new();
            let cards: Vec<_> = page
                .cards
                .iter()
                .zip(&missing)
                .filter(|(card, absent)| **absent && hashes.insert(card.card_hash.as_str()))
                .map(|(card, _)| card)
                .collect();
            if !cards.is_empty() {
                // The resident head reserves this bounded batch's staging.
                // Only this driver fills matrices, so batches cannot overlap.
                let texts = cards
                    .iter()
                    .map(|card| card.card_text.clone())
                    .collect::<Vec<_>>();
                let vectors =
                    model.embed(&texts, InputKind::Document, &EmbeddingControl::new(cancel))?;
                if vectors.len() != cards.len() {
                    return Err(EmbedError::Runtime(
                        "document embedding returned the wrong batch size".into(),
                    )
                    .into());
                }
                check_cancel(cancel)?;
                self.0.with_vector_writer(|writer| {
                    let entries = cards
                        .iter()
                        .zip(&vectors)
                        .map(|(card, vector)| {
                            (model_id, card.card_hash.as_str(), vector.values.as_slice())
                        })
                        .collect::<Vec<_>>();
                    writer.put_many(&entries)
                })?;
                self.0.publish_head_vectors(
                    model_id,
                    &cards
                        .iter()
                        .zip(&vectors)
                        .map(|(card, vector)| (card.card_hash.clone(), vector.values.clone()))
                        .collect::<Vec<_>>(),
                )?;
            }
            self.0.serve_queries(model);
            let Some(after) = next_cursor(&page) else {
                return Ok(());
            };
            check_cancel(cancel)?;
            page = source.page(Some(page.pin), Some(&after))?;
            missing = Self::missing(reader.as_deref_mut(), model_id, &page)?;
        }
    }
}
fn next_cursor(page: &HeadSearchCards) -> Option<String> {
    (page.cards.len() == BATCH).then(|| page.cards.last().unwrap().syntax_id.clone())
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(EmbedError::Cancelled.into())
    } else {
        Ok(())
    }
}

struct Slot {
    source: Source,
    head: Arc<HeadSlot>,
    root_key: Option<String>,
    startup: bool,
    cancel: Arc<AtomicBool>,
    queued: bool,
    dirty: bool,
    running: bool,
    running_pin: Option<IndexPin>,
    completed: Option<Completed>,
    retries: usize,
    retry_at: Option<Instant>,
    fault_cooldown: Option<Instant>,
}
#[derive(Default)]
struct Inbox {
    next_id: u64,
    slots: BTreeMap<u64, Slot>,
    ready: VecDeque<u64>,
    roots: BTreeMap<String, u64>,
    startup: Option<crate::store::topology::TopologyRoots>,
    discovering: bool,
    shutdown: bool,
}
impl Inbox {
    fn next_ready(&mut self) -> Option<u64> {
        self.ready
            .iter()
            .position(|id| self.slots.get(id).is_some_and(|slot| !slot.startup))
            .and_then(|position| self.ready.remove(position))
            .or_else(|| self.ready.pop_front())
    }
    fn has_pending_work(&self) -> bool {
        self.startup.is_some()
            || self.discovering
            || self
                .slots
                .values()
                .any(|slot| slot.running || slot.queued || slot.dirty || slot.retry_at.is_some())
    }
}

#[derive(Default)]
pub(super) struct Core {
    inbox: Mutex<Inbox>,
    search: Mutex<Weak<SearchService>>,
    startup_sources: Mutex<Vec<(Arc<dyn FillSource>, FillRegistration)>>,
    #[cfg(test)]
    before_startup_discovery: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    startup_discovery_sources: Mutex<Option<Vec<Arc<dyn FillSource>>>>,
    changed: Condvar,
    budget: Arc<MatrixBudget>,
    touches: std::sync::atomic::AtomicU64,
    startup_at_cap: AtomicBool,
}

impl Core {
    pub(super) fn startup_at_cap(&self) -> bool {
        self.startup_at_cap.load(Ordering::Acquire)
    }
    pub(super) fn touch(&self) -> u64 {
        self.touches.fetch_add(1, Ordering::Relaxed) + 1
    }
    pub(super) fn can_fit_alone(&self, bytes: usize) -> bool {
        bytes <= self.budget.0.lock().unwrap().limit
    }
    pub(super) fn reserve(&self, id: u64, bytes: usize, startup: bool) -> Option<MatrixCharge> {
        if let Some(charge) = self.budget.reserve(bytes) {
            return Some(charge);
        }
        if bytes > self.budget.0.lock().unwrap().limit {
            // A single oversized head says nothing about whether the other
            // recent checkouts fit. Only aggregate pressure stops prewarming.
            return None;
        }
        if startup {
            self.startup_at_cap.store(true, Ordering::Release);
            return None;
        }
        // Never hold the registry or budget while waiting for a scoring head.
        let mut heads = self
            .inbox
            .lock()
            .unwrap()
            .slots
            .iter()
            .filter(|(other, _)| **other != id)
            .map(|(_, slot)| slot.head.clone())
            .collect::<Vec<_>>();
        heads.sort_by_key(|head| head.last_touch());
        for head in heads {
            head.clear();
            if let Some(charge) = self.budget.reserve(bytes) {
                return Some(charge);
            }
        }
        None
    }
    pub(super) fn expire_idle_matrices(&self, now: Instant) -> Option<Duration> {
        // Ranking and publication lock individual heads. Never wait for those
        // locks with the inbox held, and let each head recheck its own timer.
        let heads = self
            .inbox
            .lock()
            .unwrap()
            .slots
            .values()
            // Only the owner changes running state. A batch sweep must not
            // evict its own live matrix or scratch; the next owner transition
            // rechecks it without extending the last user/publication touch.
            .filter(|slot| !slot.running)
            .map(|slot| slot.head.clone())
            .collect::<Vec<_>>();
        heads.iter().filter_map(|head| head.expire_idle(now)).min()
    }
    fn register_source(self: &Arc<Self>, source: Source) -> FillRegistration {
        self.register_source_mode(source, false)
            .expect("active registration is never skipped")
    }
    fn register_source_mode(
        self: &Arc<Self>,
        source: Source,
        startup: bool,
    ) -> Option<FillRegistration> {
        let mut inbox = self.inbox.lock().unwrap();
        let id = inbox.next_id;
        inbox.next_id += 1;
        let root_key = source.upgrade().and_then(|source| source.root_key());
        let startup = startup || source.upgrade().is_some_and(|source| source.is_startup());
        if startup
            && (inbox.shutdown
                || root_key
                    .as_ref()
                    .is_some_and(|key| inbox.roots.contains_key(key)))
        {
            return None;
        }
        if inbox.shutdown {
            let head = Arc::new(HeadSlot::new(
                Arc::downgrade(self),
                id,
                self.search.lock().unwrap().clone(),
            ));
            head.stop();
            return Some(FillRegistration {
                head,
                core: Arc::downgrade(self),
                id,
            });
        }
        let previous = root_key
            .as_ref()
            .and_then(|key| inbox.roots.remove(key))
            .and_then(|old_id| {
                inbox.ready.retain(|queued| *queued != old_id);
                inbox.slots.remove(&old_id)
            });
        let mut retired = None;
        let head = match previous {
            Some(previous) => {
                previous.cancel.store(true, Ordering::Release);
                if previous.running {
                    retired = Some(previous.head);
                    Arc::new(HeadSlot::new(
                        Arc::downgrade(self),
                        id,
                        self.search.lock().unwrap().clone(),
                    ))
                } else {
                    previous.head.rebind(id);
                    previous.head
                }
            }
            None => Arc::new(HeadSlot::new(
                Arc::downgrade(self),
                id,
                self.search.lock().unwrap().clone(),
            )),
        };
        if let Some(key) = &root_key {
            inbox.roots.insert(key.clone(), id);
        }
        inbox.slots.insert(
            id,
            Slot {
                source,
                head: head.clone(),
                root_key,
                startup,
                cancel: Arc::new(AtomicBool::new(false)),
                queued: false,
                dirty: false,
                running: false,
                running_pin: None,
                completed: None,
                retries: 0,
                retry_at: None,
                fault_cooldown: None,
            },
        );
        drop(inbox);
        if let Some(retired) = retired {
            retired.stop();
        }
        Some(FillRegistration {
            head,
            core: Arc::downgrade(self),
            id,
        })
    }
    pub(super) fn request(&self, id: u64) {
        let mut inbox = self.inbox.lock().unwrap();
        if inbox.shutdown {
            return;
        }
        let Some(head) = inbox.slots.get(&id).map(|slot| slot.head.clone()) else {
            return;
        };
        drop(inbox);
        head.rearm();
        if !head.needs_build() {
            return;
        }
        head.begin();
        inbox = self.inbox.lock().unwrap();
        if inbox.shutdown {
            return;
        }
        let Some(slot) = inbox.slots.get_mut(&id) else {
            return;
        };
        if slot.running || slot.queued || slot.retry_at.is_some() {
            return;
        }
        slot.dirty = true;
        slot.queued = true;
        inbox.ready.push_back(id);
        self.changed.notify_all();
    }
    pub(super) fn model_ready(&self) {
        let heads = self
            .inbox
            .lock()
            .unwrap()
            .slots
            .iter()
            .map(|(id, slot)| (*id, slot.head.clone()))
            .collect::<Vec<_>>();
        for (id, head) in heads {
            if !head.dormant() && head.needs_build() {
                self.request(id);
            }
        }
    }
    pub(super) fn committed(
        &self,
        model_id: &str,
        epoch: u64,
        vectors: &[(String, Vec<i8>)],
    ) -> Result<()> {
        let heads = self
            .inbox
            .lock()
            .unwrap()
            .slots
            .values()
            .map(|s| s.head.clone())
            .collect::<Vec<_>>();
        for head in heads {
            head.committed(model_id, epoch, vectors)?;
        }
        self.model_ready();
        Ok(())
    }
    pub(super) fn wake(&self) {
        let _inbox = self.inbox.lock().unwrap();
        self.changed.notify_all();
    }
}

/// Owns one blocking driver. Its inbox contains only weak checkout sources.
/// Dropping the service cancels work and wakes the driver; it never joins while
/// a checkout or registry lock might be held.
pub struct SearchFill {
    core: Arc<Core>,
}
impl SearchFill {
    pub fn start(search: Arc<SearchService>) -> Result<Arc<Self>> {
        let owner = Self::start_with(Arc::new(LocalBackend(search.clone())))?;
        search.set_maintenance(Arc::downgrade(&owner.core));
        owner.core.wake();
        Ok(owner)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn start_with_limit_for_tests(
        search: Arc<SearchService>,
        limit: usize,
    ) -> Result<Arc<Self>> {
        let core = Core {
            budget: Arc::new(MatrixBudget(Mutex::new(BudgetState { limit, used: 0 }))),
            ..Core::default()
        };
        let owner = Self::start_core(Arc::new(LocalBackend(search.clone())), core)?;
        search.set_maintenance(Arc::downgrade(&owner.core));
        owner.core.wake();
        Ok(owner)
    }

    fn start_with(backend: Arc<dyn FillBackend>) -> Result<Arc<Self>> {
        Self::start_core(backend, Core::default())
    }
    fn start_core(backend: Arc<dyn FillBackend>, core: Core) -> Result<Arc<Self>> {
        let core = Arc::new(core);
        *core.search.lock().unwrap() = backend.search();
        let driver = core.clone();
        std::thread::Builder::new()
            .name("trellis-search-fill".into())
            .spawn(move || drive(driver, backend))?;
        Ok(Arc::new(Self { core }))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn wait_until_idle_for_tests(&self) {
        let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
        let mut inbox = self.core.inbox.lock().unwrap();
        while inbox.startup.is_some()
            || inbox.discovering
            || inbox
                .slots
                .values()
                .any(|slot| slot.running || slot.queued || slot.retry_at.is_some())
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "fill did not settle");
            inbox = self.core.changed.wait_timeout(inbox, remaining).unwrap().0;
        }
    }

    pub fn queue_startup(&self, roots: crate::store::topology::TopologyRoots) {
        let mut inbox = self.core.inbox.lock().unwrap();
        if !inbox.shutdown {
            inbox.startup = Some(roots);
            self.core.changed.notify_all();
        }
    }

    /// Resident and reserved in-progress matrix bytes, daemon-wide.
    pub fn matrix_storage_bytes(&self) -> usize {
        self.core.budget.0.lock().unwrap().used
    }

    pub(crate) fn register(&self, scheduler: &Arc<DaemonState>) -> FillRegistration {
        let source: Arc<dyn FillSource> = scheduler.clone();
        self.register_source(Arc::downgrade(&source))
    }

    fn register_source(&self, source: Source) -> FillRegistration {
        self.core.register_source(source)
    }
}
impl Drop for SearchFill {
    fn drop(&mut self) {
        let heads = {
            let mut inbox = self.core.inbox.lock().unwrap();
            inbox.shutdown = true;
            for slot in inbox.slots.values() {
                slot.cancel.store(true, Ordering::Release);
            }
            self.core.changed.notify_one();
            inbox
                .slots
                .values()
                .map(|slot| slot.head.clone())
                .collect::<Vec<_>>()
        };
        for head in heads {
            head.stop();
        }
    }
}

pub(crate) struct FillRegistration {
    head: Arc<HeadSlot>,
    core: Weak<Core>,
    id: u64,
}
impl FillRegistration {
    pub(crate) fn head_slot(&self) -> Arc<HeadSlot> {
        self.head.clone()
    }
    /// The owner publishes the exact running head, separately from queued hints.
    /// A successor registration or another admitted head cannot borrow its work.
    pub(crate) fn building_for(&self, pin: IndexPin) -> bool {
        let Some(core) = self.core.upgrade() else {
            return false;
        };
        let inbox = core.inbox.lock().unwrap();
        inbox
            .slots
            .get(&self.id)
            .is_some_and(|slot| slot.running && slot.running_pin == Some(pin))
    }

    /// A hint only: no SQLite, model work, or wait for driver completion.
    pub(crate) fn notify(&self) {
        let Some(core) = self.core.upgrade() else {
            return;
        };
        self.head.begin();
        let mut inbox = core.inbox.lock().unwrap();
        let Some(slot) = inbox.slots.get_mut(&self.id) else {
            return;
        };
        slot.dirty = true;
        if !slot.running && !slot.queued && slot.retry_at.is_none() {
            if let Some(until) = slot.fault_cooldown.take() {
                slot.retry_at = Some(until.max(Instant::now()));
            } else {
                slot.queued = true;
                inbox.ready.push_back(self.id);
            }
        }
        core.changed.notify_one();
    }
}
impl Drop for FillRegistration {
    fn drop(&mut self) {
        let Some(core) = self.core.upgrade() else {
            return;
        };
        let removed = {
            let mut inbox = core.inbox.lock().unwrap();
            let removed = inbox.slots.remove(&self.id);
            if let Some(slot) = &removed {
                slot.cancel.store(true, Ordering::Release);
                if let Some(key) = &slot.root_key
                    && inbox.roots.get(key) == Some(&self.id)
                {
                    inbox.roots.remove(key);
                }
            }
            inbox.ready.retain(|id| *id != self.id);
            core.changed.notify_one();
            removed
        };
        if let Some(slot) = removed {
            slot.head.stop();
        }
    }
}

fn retryable_fault(error: &anyhow::Error) -> bool {
    error.is::<StorageBusy>()
        || error.is::<crate::store::SqliteContention>()
        || error.is::<crate::search_vectors::StoreUnavailable>()
        || error.is::<std::io::Error>()
        || crate::search_vectors::is_corrupt_cache(error)
        || matches!(
            error.downcast_ref::<EmbedError>(),
            Some(EmbedError::Runtime(_))
        )
}

fn drive(core: Arc<Core>, backend: Arc<dyn FillBackend>) {
    loop {
        let (id, source, cancel, completed, head) = {
            let mut inbox = core.inbox.lock().unwrap();
            loop {
                drop(inbox);
                let matrix_idle = core.expire_idle_matrices(backend.idle_now());
                inbox = core.inbox.lock().unwrap();
                if inbox.shutdown {
                    return;
                }
                let normal_ready = inbox
                    .ready
                    .iter()
                    .any(|id| inbox.slots.get(id).is_some_and(|slot| !slot.startup));
                if !normal_ready && let Some(roots) = inbox.startup.take() {
                    inbox.discovering = true;
                    drop(inbox);
                    #[cfg(test)]
                    if let Some(hook) = core.before_startup_discovery.lock().unwrap().take() {
                        hook();
                    }
                    #[cfg(test)]
                    let override_sources = core.startup_discovery_sources.lock().unwrap().take();
                    #[cfg(test)]
                    let sources = override_sources
                        .unwrap_or_else(|| super::search_startup::discover_sources(roots));
                    #[cfg(not(test))]
                    let sources = super::search_startup::discover_sources(roots);
                    for source in sources {
                        if core.inbox.lock().unwrap().shutdown {
                            break;
                        }
                        let Some(registration) =
                            core.register_source_mode(Arc::downgrade(&source), true)
                        else {
                            continue;
                        };
                        registration.notify();
                        core.startup_sources
                            .lock()
                            .unwrap()
                            .push((source, registration));
                    }
                    inbox = core.inbox.lock().unwrap();
                    inbox.discovering = false;
                    core.changed.notify_all();
                    continue;
                }
                let now = Instant::now();
                let due = inbox
                    .slots
                    .iter()
                    .filter_map(|(id, slot)| slot.retry_at.filter(|at| *at <= now).map(|_| *id))
                    .collect::<Vec<_>>();
                for id in due {
                    let slot = inbox.slots.get_mut(&id).unwrap();
                    slot.retry_at = None;
                    slot.queued = true;
                    inbox.ready.push_back(id);
                }
                if let Some(id) = inbox.next_ready() {
                    let Some(slot) = inbox.slots.get_mut(&id) else {
                        continue;
                    };
                    slot.queued = false;
                    slot.dirty = false;
                    slot.running = true;
                    slot.running_pin = None;
                    break (
                        id,
                        slot.source.clone(),
                        slot.cancel.clone(),
                        slot.completed,
                        slot.head.clone(),
                    );
                }
                let pending_fill = inbox.has_pending_work();
                // Inbox -> admission only claims the transition. Never wait for
                // the model with this inbox held; notifications use this mutex.
                if !pending_fill && backend.begin_idle_expiry() {
                    drop(inbox);
                    // Model lifetime is global. Each resident matrix has its
                    // own timer and is not invalidated by model unloading.
                    backend.finish_idle_expiry();
                    inbox = core.inbox.lock().unwrap();
                    continue;
                }
                let retry = inbox
                    .slots
                    .values()
                    .filter_map(|slot| slot.retry_at)
                    .min()
                    .map(|at| at.saturating_duration_since(now));
                // An expired idle cutoff must not spin while a future retry is
                // pending. Pending fill work takes priority over idle eviction.
                let idle = if pending_fill {
                    None
                } else {
                    backend.idle_remaining()
                };
                let remaining = [retry, idle, matrix_idle].into_iter().flatten().min();
                inbox = match remaining {
                    Some(remaining) => core.changed.wait_timeout(inbox, remaining).unwrap().0,
                    None => core.changed.wait(inbox).unwrap(),
                };
            }
        };
        head.begin();
        let Some(source) = source.upgrade() else {
            head.stop();
            core.inbox.lock().unwrap().slots.remove(&id);
            continue;
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if source.is_startup() && core.startup_at_cap() {
                return Outcome {
                    pin: None,
                    cache_epoch: 0,
                    result: Err(MemoryDeclined.into()),
                };
            }
            if let Err(error) = backend.prepare(source.as_ref(), &cancel, &head) {
                return Outcome {
                    pin: None,
                    cache_epoch: 0,
                    result: Err(error),
                };
            }
            // Cancellation/unregister can discard the resident head during
            // inference. Keep its already-reserved staging charge until the
            // running job physically drops every vector buffer.
            let _scratch = head.fill_scratch();
            if backend
                .search()
                .upgrade()
                .is_some_and(|search| search.model_configured())
                && _scratch.is_none()
            {
                return Outcome {
                    pin: None,
                    cache_epoch: 0,
                    result: Err(MemoryDeclined.into()),
                };
            }
            backend.run(source.as_ref(), &cancel, completed, &|pin| {
                let mut inbox = core.inbox.lock().unwrap();
                if let Some(slot) = inbox.slots.get_mut(&id) {
                    slot.running_pin = Some(pin);
                }
                core.changed.notify_all();
            })
        }))
        .unwrap_or_else(|_| Outcome {
            pin: completed.map(|done| done.pin),
            cache_epoch: completed.map_or(0, |done| done.cache_epoch),
            result: Err(EmbedError::Runtime("fill job panicked".into()).into()),
        });
        // Release the source before publishing completion. It does not keep a
        // retired checkout alive while this owner waits for its next hint.
        drop(source);
        head.finish(&outcome.result);
        let mut inbox = core.inbox.lock().unwrap();
        let Some(slot) = inbox.slots.get_mut(&id) else {
            // A dropped registration can still have held staging until this
            // owner transition. Publish that its physical cleanup completed.
            core.changed.notify_all();
            continue;
        };
        slot.running = false;
        slot.running_pin = None;
        let changed = outcome
            .result
            .as_ref()
            .is_err_and(|error| error.is::<SearchHeadChanged>());
        let retryable = outcome.result.as_ref().is_err_and(retryable_fault);
        let deferred = outcome
            .result
            .as_ref()
            .is_err_and(|error| error.is::<SearchDeferred>());
        let memory_declined = outcome
            .result
            .as_ref()
            .is_err_and(|error| error.is::<MemoryDeclined>());
        if memory_declined {
            slot.completed = None;
            slot.retries = 0;
            // An explicit successor hint can name a smaller/newer pin. It
            // must survive this attempt's admission failure.
        } else if deferred {
            slot.completed = None;
            slot.retries = 0;
            // dirty records only an explicit hint received after this attempt
            // started; that successor must survive transient deferral.
        } else if changed {
            slot.completed = None;
            slot.retries = 0;
        } else if retryable && slot.retries < MAX_FAULT_RETRIES {
            slot.retries += 1;
            slot.retry_at = Some(Instant::now() + FAULT_RETRY);
            core.changed.notify_all();
            continue;
        } else if retryable {
            // A later checkout signal can restart bounded fault recovery. Do
            // not mark a failed head complete or turn accumulated hints into
            // an unbounded self-retry loop.
            slot.completed = None;
            slot.retries = 0;
            slot.dirty = false;
            slot.fault_cooldown = Some(Instant::now() + FAULT_RETRY);
        } else {
            slot.completed = if outcome.result.is_ok() {
                outcome.pin.map(|pin| Completed {
                    pin,
                    cache_epoch: outcome.cache_epoch,
                })
            } else {
                None
            };
            slot.retries = 0;
        }
        if changed || slot.dirty {
            slot.queued = true;
            inbox.ready.push_back(id);
        }
        core.changed.notify_all();
    }
}

#[cfg(test)]
#[path = "search_memory_tests.rs"]
mod memory_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::HeadSearchCard;
    use std::sync::mpsc::{self, Receiver, Sender};

    type TestEvent = (IndexPin, Arc<AtomicBool>);
    type PausedHarness = (Arc<SearchFill>, Receiver<TestEvent>, Sender<()>);

    struct TestSource(IndexPin);
    impl FillSource for TestSource {
        fn page(&self, _: Option<IndexPin>, _: Option<&str>) -> Result<HeadSearchCards> {
            Ok(HeadSearchCards {
                pin: self.0,
                cards: Vec::<HeadSearchCard>::new(),
            })
        }
    }
    struct PausedBackend {
        entered: Sender<TestEvent>,
        resume: Mutex<Receiver<()>>,
        busy: bool,
    }
    impl FillBackend for PausedBackend {
        fn run(
            &self,
            source: &dyn FillSource,
            cancel: &AtomicBool,
            _: Option<Completed>,
            publish: &dyn Fn(IndexPin),
        ) -> Outcome {
            let pin = source.page(None, None).unwrap().pin;
            publish(pin);
            // A separate observation bit proves the operation saw cancellation.
            let observed = Arc::new(AtomicBool::new(false));
            self.entered.send((pin, observed.clone())).unwrap();
            self.resume.lock().unwrap().recv().unwrap();
            observed.store(cancel.load(Ordering::Acquire), Ordering::Release);
            Outcome {
                pin: Some(pin),
                cache_epoch: 0,
                result: if self.busy {
                    Err(StorageBusy.into())
                } else {
                    Ok(())
                },
            }
        }
    }
    fn pin(revision: u64) -> IndexPin {
        IndexPin {
            index_generation: uuid::Uuid::nil(),
            index_revision: revision,
        }
    }
    fn register(owner: &SearchFill, revision: u64) -> (Arc<dyn FillSource>, FillRegistration) {
        let source: Arc<dyn FillSource> = Arc::new(TestSource(pin(revision)));
        let registration = owner.register_source(Arc::downgrade(&source));
        (source, registration)
    }
    fn wait_for(core: &Core, predicate: impl Fn(&Inbox) -> bool) {
        let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
        let mut inbox = core.inbox.lock().unwrap();
        while !predicate(&inbox) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "fill owner did not publish expected transition"
            );
            inbox = core.changed.wait_timeout(inbox, remaining).unwrap().0;
        }
    }
    fn paused(busy: bool) -> PausedHarness {
        let (entered, events) = mpsc::channel();
        let (resume, proceed) = mpsc::channel();
        let owner = SearchFill::start_with(Arc::new(PausedBackend {
            entered,
            resume: Mutex::new(proceed),
            busy,
        }))
        .unwrap();
        (owner, events, resume)
    }
    fn event(events: &Receiver<TestEvent>) -> TestEvent {
        events.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap()
    }

    struct PausedMatrixBackend {
        entered: Sender<()>,
        resume: Mutex<Receiver<()>>,
    }
    impl FillBackend for PausedMatrixBackend {
        fn prepare(&self, _: &dyn FillSource, cancel: &AtomicBool, head: &HeadSlot) -> Result<()> {
            self.entered.send(()).unwrap();
            self.resume.lock().unwrap().recv().unwrap();
            check_cancel(cancel)?;
            head.build(
                super::super::search_head::HeadUpdate {
                    pin: pin(1),
                    declarations: 0,
                    change: super::super::search_head::HeadChange::Full(Vec::new().into()),
                },
                "model",
                0,
                None,
                &mut || check_cancel(cancel),
            )
        }
        fn run(
            &self,
            _: &dyn FillSource,
            _: &AtomicBool,
            _: Option<Completed>,
            _: &dyn Fn(IndexPin),
        ) -> Outcome {
            Outcome {
                pin: Some(pin(1)),
                cache_epoch: 0,
                result: Ok(()),
            }
        }
    }
    #[test]
    fn underestimated_charge_returns_typed_error_without_mutation_or_poisoning() {
        let budget = Arc::new(MatrixBudget(Mutex::new(BudgetState { limit: 64, used: 0 })));
        let mut charge = budget.reserve(32).unwrap();
        let error = charge.shrink(33).unwrap_err();
        let estimate = error.downcast_ref::<MatrixEstimateExceeded>().unwrap();
        assert_eq!((estimate.reserved, estimate.actual), (32, 33));
        assert_eq!(charge.bytes, 32);
        assert_eq!(budget.0.lock().unwrap().used, 32);
        charge.shrink(16).unwrap();
        assert_eq!(budget.0.lock().unwrap().used, 16);
        drop(charge);
        assert_eq!(budget.0.lock().unwrap().used, 0);
    }

    #[test]
    fn one_oversized_startup_head_does_not_stop_smaller_recent_heads() {
        let core = Arc::new(Core {
            budget: Arc::new(MatrixBudget(Mutex::new(BudgetState { limit: 64, used: 0 }))),
            ..Core::default()
        });
        assert!(core.reserve(0, 65, true).is_none());
        assert!(!core.startup_at_cap());
        let smaller = core.reserve(1, 32, true).unwrap();
        assert!(!core.startup_at_cap());
        assert_eq!(core.budget.0.lock().unwrap().used, 32);
        // Aggregate pressure still terminates startup rather than evicting a
        // previous matrix or competing with explicit demand.
        assert!(core.reserve(2, 33, true).is_none());
        assert!(core.startup_at_cap());
        assert_eq!(core.budget.0.lock().unwrap().used, 32);
        drop(smaller);
    }

    #[test]
    fn owner_finishes_matrix_after_waiter_budget_fallback() {
        let (entered, arrived) = mpsc::channel();
        let (resume, released) = mpsc::channel();
        let owner = SearchFill::start_with(Arc::new(PausedMatrixBackend {
            entered,
            resume: Mutex::new(released),
        }))
        .unwrap();
        let (_source, registration) = register(&owner, 1);
        let head = registration.head_slot();
        head.request();
        arrived.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        let now = Instant::now();
        let budget = crate::search_embed::QueryBudget::new(now, Arc::new(move || now));
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            head.wait_latest(&cancel, &budget)
                .unwrap_err()
                .downcast_ref::<EmbedError>(),
            Some(EmbedError::BudgetExceeded)
        ));
        assert!(!cancel.load(Ordering::Acquire));
        resume.send(()).unwrap();
        owner.wait_until_idle_for_tests();
        assert_eq!(head.declarations(pin(1), "model", 0).unwrap(), Some(0));
    }

    struct RootSource;
    impl FillSource for RootSource {
        fn root_key(&self) -> Option<String> {
            Some("root".into())
        }
        fn page(&self, _: Option<IndexPin>, _: Option<&str>) -> Result<HeadSearchCards> {
            Ok(HeadSearchCards {
                pin: pin(1),
                cards: Vec::new(),
            })
        }
    }
    #[test]
    fn startup_head_adoption_survives_old_registration_drop() {
        let (owner, _events, _resume) = paused(false);
        let source: Arc<dyn FillSource> = Arc::new(RootSource);
        let old = owner.register_source(Arc::downgrade(&source));
        let head = old.head_slot();
        head.build(
            super::super::search_head::HeadUpdate {
                pin: pin(1),
                declarations: 0,
                change: super::super::search_head::HeadChange::Full(Vec::new().into()),
            },
            "model",
            0,
            None,
            &mut || Ok(()),
        )
        .unwrap();
        let successor = owner.register_source(Arc::downgrade(&source));
        assert!(Arc::ptr_eq(&head, &successor.head_slot()));
        drop(old);
        assert_eq!(head.declarations(pin(1), "model", 0).unwrap(), Some(0));
        assert!(
            owner
                .core
                .inbox
                .lock()
                .unwrap()
                .slots
                .contains_key(&successor.id)
        );
    }

    fn publish_empty(head: &HeadSlot, revision: u64) {
        head.build(
            HeadUpdate {
                pin: pin(revision),
                declarations: 0,
                change: super::super::search_head::HeadChange::Full(Vec::new().into()),
            },
            "model",
            0,
            None,
            &mut || Ok(()),
        )
        .unwrap();
    }
    fn wait_warm(head: Arc<HeadSlot>) {
        let (completed, result) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let now = Instant::now();
            let budget = crate::search_embed::QueryBudget::new(
                now + crate::testing::HANG_BACKSTOP,
                Arc::new(Instant::now),
            );
            completed
                .send(head.wait_latest(&AtomicBool::new(false), &budget))
                .unwrap();
        });
        result
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .unwrap()
            .unwrap();
        waiter.join().unwrap();
    }
    #[test]
    fn warm_matrix_does_not_wait_for_its_fill_or_a_sibling_checkout() {
        let (owner, events, resume) = paused(false);
        let (_a_source, a) = register(&owner, 1);
        let (_b_source, b) = register(&owner, 2);
        publish_empty(&a.head_slot(), 1);
        publish_empty(&b.head_slot(), 2);
        a.notify();
        assert_eq!(event(&events).0, pin(1));
        wait_warm(a.head_slot());
        b.notify();
        // B's published matrix remains usable while its hint is queued behind A.
        wait_warm(b.head_slot());
        resume.send(()).unwrap();
        assert_eq!(event(&events).0, pin(2));
        wait_warm(b.head_slot());
        resume.send(()).unwrap();
        owner.wait_until_idle_for_tests();
    }

    struct DeferredMatrixBackend {
        calls: std::sync::atomic::AtomicUsize,
    }
    impl FillBackend for DeferredMatrixBackend {
        fn prepare(&self, _: &dyn FillSource, _: &AtomicBool, head: &HeadSlot) -> Result<()> {
            if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
                return Err(SearchDeferred.into());
            }
            publish_empty(head, 1);
            Ok(())
        }
        fn run(
            &self,
            _: &dyn FillSource,
            _: &AtomicBool,
            _: Option<Completed>,
            _: &dyn Fn(IndexPin),
        ) -> Outcome {
            Outcome {
                pin: Some(pin(1)),
                cache_epoch: 0,
                result: Ok(()),
            }
        }
    }
    #[test]
    fn transient_background_deferral_is_not_a_fault_and_a_later_hint_recovers() {
        let backend = Arc::new(DeferredMatrixBackend {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let owner = SearchFill::start_with(backend.clone()).unwrap();
        let (_source, registration) = register(&owner, 1);
        registration.notify();
        owner.wait_until_idle_for_tests();
        assert_eq!(backend.calls.load(Ordering::Acquire), 1);
        registration.head_slot().fault().unwrap();
        assert_eq!(
            registration
                .head_slot()
                .declarations(pin(1), "model", 0)
                .unwrap(),
            None
        );
        registration.notify();
        owner.wait_until_idle_for_tests();
        assert_eq!(backend.calls.load(Ordering::Acquire), 2);
        assert_eq!(
            registration
                .head_slot()
                .declarations(pin(1), "model", 0)
                .unwrap(),
            Some(0)
        );
    }

    struct PausedDeferredBackend {
        entered: Sender<()>,
        resume: Mutex<Receiver<()>>,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl FillBackend for PausedDeferredBackend {
        fn prepare(&self, _: &dyn FillSource, _: &AtomicBool, head: &HeadSlot) -> Result<()> {
            if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
                self.entered.send(()).unwrap();
                self.resume.lock().unwrap().recv().unwrap();
                return Err(SearchDeferred.into());
            }
            publish_empty(head, 1);
            Ok(())
        }
        fn run(
            &self,
            _: &dyn FillSource,
            _: &AtomicBool,
            _: Option<Completed>,
            _: &dyn Fn(IndexPin),
        ) -> Outcome {
            Outcome {
                pin: Some(pin(1)),
                cache_epoch: 0,
                result: Ok(()),
            }
        }
    }
    #[test]
    fn explicit_hint_during_deferred_attempt_is_preserved() {
        let (entered, arrived) = mpsc::channel();
        let (resume, release) = mpsc::channel();
        let backend = Arc::new(PausedDeferredBackend {
            entered,
            resume: Mutex::new(release),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let owner = SearchFill::start_with(backend.clone()).unwrap();
        let (_source, registration) = register(&owner, 1);
        registration.notify();
        arrived.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        registration.notify();
        resume.send(()).unwrap();
        owner.wait_until_idle_for_tests();
        assert_eq!(backend.calls.load(Ordering::Acquire), 2);
        assert_eq!(
            registration
                .head_slot()
                .declarations(pin(1), "model", 0)
                .unwrap(),
            Some(0)
        );
    }

    #[test]
    fn startup_discovery_runs_on_existing_driver_without_holding_the_inbox() {
        let (owner, _events, _resume) = paused(false);
        let directory = tempfile::tempdir().unwrap();
        let roots = crate::store::topology::TopologyRoots::isolated_for_tests(
            directory.path().join("cache"),
            directory.path().join("data"),
        );
        let (entered, arrived) = mpsc::channel();
        let (resume, released) = mpsc::channel();
        *owner.core.before_startup_discovery.lock().unwrap() = Some(Box::new(move || {
            entered
                .send(std::thread::current().name().map(str::to_owned))
                .unwrap();
            released
                .recv_timeout(crate::testing::HANG_BACKSTOP)
                .unwrap();
        }));
        owner.queue_startup(roots);
        assert_eq!(
            arrived
                .recv_timeout(crate::testing::HANG_BACKSTOP)
                .unwrap()
                .as_deref(),
            Some("trellis-search-fill")
        );
        // Discovery is paused, but registration can still acquire the owner inbox.
        let (_source, registration) = register(&owner, 1);
        assert!(
            owner
                .core
                .inbox
                .lock()
                .unwrap()
                .slots
                .contains_key(&registration.id)
        );
        resume.send(()).unwrap();
        owner.wait_until_idle_for_tests();
    }

    #[test]
    fn startup_discovery_cannot_replace_a_checkout_activated_during_discovery() {
        let (owner, _events, _resume) = paused(false);
        let directory = tempfile::tempdir().unwrap();
        let roots = crate::store::topology::TopologyRoots::isolated_for_tests(
            directory.path().join("cache"),
            directory.path().join("data"),
        );
        let startup: Arc<dyn FillSource> = Arc::new(RootSource);
        *owner.core.startup_discovery_sources.lock().unwrap() = Some(vec![startup]);
        let (entered, arrived) = mpsc::channel();
        let (resume, released) = mpsc::channel();
        *owner.core.before_startup_discovery.lock().unwrap() = Some(Box::new(move || {
            entered.send(()).unwrap();
            released
                .recv_timeout(crate::testing::HANG_BACKSTOP)
                .unwrap();
        }));
        owner.queue_startup(roots);
        arrived.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        let active: Arc<dyn FillSource> = Arc::new(RootSource);
        let registration = owner.register_source(Arc::downgrade(&active));
        let head = registration.head_slot();
        publish_empty(&head, 1);
        resume.send(()).unwrap();
        owner.wait_until_idle_for_tests();
        let inbox = owner.core.inbox.lock().unwrap();
        assert_eq!(inbox.roots.get("root"), Some(&registration.id));
        assert_eq!(inbox.slots.len(), 1);
        assert!(!inbox.slots[&registration.id].cancel.load(Ordering::Acquire));
        assert!(Arc::ptr_eq(&inbox.slots[&registration.id].head, &head));
        assert_eq!(head.declarations(pin(1), "model", 0).unwrap(), Some(0));
    }

    #[test]
    fn hints_coalesce_and_completion_preserves_a_later_notification() {
        let (owner, events, resume) = paused(false);
        let (_first, a) = register(&owner, 1);
        let (_second, b) = register(&owner, 2);
        assert!(!a.building_for(pin(1)));
        a.notify();
        assert_eq!(event(&events).0, pin(1));
        assert!(a.building_for(pin(1)));
        assert!(!a.building_for(pin(2)));
        assert!(!b.building_for(pin(1)));
        for _ in 0..20 {
            a.notify();
            b.notify();
        }
        {
            let inbox = owner.core.inbox.lock().unwrap();
            assert_eq!(inbox.ready.len(), 1);
            assert_eq!(inbox.slots.len(), 2);
        }
        resume.send(()).unwrap();
        assert_eq!(event(&events).0, pin(2));
        resume.send(()).unwrap();
        assert_eq!(event(&events).0, pin(1));
        resume.send(()).unwrap();
        wait_for(&owner.core, |inbox| {
            inbox
                .slots
                .values()
                .all(|slot| !slot.running && !slot.queued)
        });
        assert!(events.try_recv().is_err());
        assert!(!a.building_for(pin(1)));
        assert!(!b.building_for(pin(2)));
    }

    #[test]
    fn retiring_source_cancels_without_waiting_and_does_not_remove_successor() {
        let (owner, events, resume) = paused(false);
        let (_source, old) = register(&owner, 1);
        old.notify();
        let (_, observed) = event(&events);
        drop(old);
        let (_new_source, new) = register(&owner, 2);
        new.notify();
        assert_eq!(owner.core.inbox.lock().unwrap().slots.len(), 1);
        resume.send(()).unwrap();
        assert_eq!(event(&events).0, pin(2));
        assert!(observed.load(Ordering::Acquire));
        resume.send(()).unwrap();
        wait_for(&owner.core, |inbox| !inbox.slots[&new.id].running);
        assert!(owner.core.inbox.lock().unwrap().slots.contains_key(&new.id));
    }

    #[test]
    fn shutdown_cancels_active_work_without_joining_it() {
        let (owner, events, resume) = paused(false);
        let (_source, registration) = register(&owner, 1);
        registration.notify();
        let (_, observed) = event(&events);
        let core = owner.core.clone();
        drop(owner);
        assert!(core.inbox.lock().unwrap().shutdown);
        resume.send(()).unwrap();
        wait_for(&core, |inbox| !inbox.slots[&registration.id].running);
        assert!(observed.load(Ordering::Acquire));
    }

    #[cfg(feature = "test-support")]
    #[tokio::test]
    async fn real_checkout_publication_fifo_and_retirement_do_not_wait_for_fill() {
        use crate::{
            daemon::registry::{CHECKOUT_RELEASE_DELAY, CheckoutOptions, CheckoutRegistry},
            indexer::IndexOptions,
            store::{
                Store,
                topology::{TopologyRoots, WorkspaceIdentity},
            },
        };

        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().join("work");
        std::fs::create_dir(&checkout).unwrap();
        std::fs::write(checkout.join("a.js"), "function original() {}\n").unwrap();
        let identity = WorkspaceIdentity::discover(Some(&checkout), &checkout)
            .unwrap()
            .attach_marker()
            .unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            directory.path().join("cache"),
            directory.path().join("data"),
        );
        let now = Instant::now();
        let (owner, events, resume) = paused(false);
        let mut registry = CheckoutRegistry::with_roots_at(roots.clone(), now);
        registry.set_search_fill(owner.clone());
        registry
            .register(&identity, CheckoutOptions(serde_json::json!({})))
            .unwrap();
        registry.attach_launch(1, &identity).unwrap();
        let runtime = registry.activate(&identity.root_key).unwrap();
        // This event requires the real registration, startup publication, worker
        // hint and DaemonState page read; the backend remains paused afterward.
        let (events, (startup, cancelled)) = tokio::task::spawn_blocking(move || {
            let entered = event(&events);
            (events, entered)
        })
        .await
        .unwrap();
        let store_identity = WorkspaceIdentity::discover(Some(&checkout), &checkout).unwrap();
        let store = Store::open(roots, store_identity).unwrap();
        assert_eq!(store.status().unwrap().revision, startup);
        assert!(
            !store
                .head_search_cards(Some(startup), None, BATCH)
                .unwrap()
                .cards
                .is_empty()
        );

        let options = IndexOptions::new(checkout.clone());
        let terminal = Arc::new(tokio::sync::Notify::new());
        let claims = Arc::new(Mutex::new(Vec::new()));
        let mut requests = None;
        runtime.hold_worker_step_for_tests(|| {
            std::fs::write(checkout.join("a.js"), "function changed() {}\n").unwrap();
            let first = store.enqueue_request(&options, None).unwrap();
            let second = store.enqueue_request(&options, None).unwrap();
            runtime
                .notify_on_terminal_request_for_tests(&second.id, terminal.clone())
                .unwrap();
            let ids = [first.id.clone(), second.id.clone()];
            let claim_store = store.clone();
            let claim_log = claims.clone();
            let next_runtime = runtime.clone();
            runtime.set_after_claim_hook_for_tests(move || {
                let states = ids
                    .iter()
                    .map(|id| claim_store.request_by_id(id).unwrap().unwrap().state)
                    .collect::<Vec<_>>();
                claim_log.lock().unwrap().push(states);
                next_runtime.set_after_claim_hook_for_tests(move || {
                    let states = ids
                        .iter()
                        .map(|id| claim_store.request_by_id(id).unwrap().unwrap().state)
                        .collect::<Vec<_>>();
                    claim_log.lock().unwrap().push(states);
                });
            });
            requests = Some((first, second));
        });
        tokio::time::timeout(crate::testing::HANG_BACKSTOP, terminal.notified())
            .await
            .expect("FIFO completion waited for the paused fill");
        let (first, second) = requests.unwrap();
        assert!(first.seq < second.seq);
        let first = store.request_by_id(&first.id).unwrap().unwrap();
        let second = store.request_by_id(&second.id).unwrap().unwrap();
        assert_eq!(first.state, "done");
        assert_eq!(second.state, "done");
        assert_ne!(first.revision.unwrap(), startup);
        assert_eq!(
            *claims.lock().unwrap(),
            vec![
                vec!["running".to_owned(), "queued".to_owned()],
                vec!["done".to_owned(), "running".to_owned()],
            ]
        );
        assert!(
            owner
                .core
                .inbox
                .lock()
                .unwrap()
                .slots
                .values()
                .any(|slot| slot.running && slot.dirty)
        );

        registry.disconnect_at(1, now);
        let lifecycle = registry.lifecycle_signal();
        let due = now + CHECKOUT_RELEASE_DELAY;
        tokio::time::timeout(crate::testing::HANG_BACKSTOP, async {
            loop {
                let wake = lifecycle.notified();
                if !registry.advance(due).unwrap().released.is_empty() {
                    break;
                }
                wake.await;
            }
        })
        .await
        .expect("retirement waited for the paused fill");
        assert!(!runtime.has_active_resources());
        assert!(owner.core.inbox.lock().unwrap().slots.is_empty());

        // Reactivation has a new registration. Its event also proves the old
        // backend returned and observed retirement cancellation after resuming.
        registry.attach_launch(2, &identity).unwrap();
        let successor = registry.activate(&identity.root_key).unwrap();
        resume.send(()).unwrap();
        let (_, successor_cancelled) = tokio::task::spawn_blocking(move || event(&events))
            .await
            .unwrap();
        assert!(cancelled.load(Ordering::Acquire));
        assert!(!successor_cancelled.load(Ordering::Acquire));
        registry.disconnect_at(2, now);
        tokio::time::timeout(crate::testing::HANG_BACKSTOP, async {
            loop {
                let wake = lifecycle.notified();
                if !registry.advance(due).unwrap().released.is_empty() {
                    break;
                }
                wake.await;
            }
        })
        .await
        .expect("successor retirement waited for the paused fill");
        assert!(!successor.has_active_resources());
        resume.send(()).unwrap();
    }

    struct CachedSource {
        cards: Vec<crate::store::HeadSearchCard>,
        pages: std::sync::atomic::AtomicUsize,
    }
    impl FillSource for CachedSource {
        fn page(&self, expected: Option<IndexPin>, after: Option<&str>) -> Result<HeadSearchCards> {
            assert!(expected.is_none_or(|value| value == pin(1)));
            self.pages.fetch_add(1, Ordering::Relaxed);
            Ok(HeadSearchCards {
                pin: pin(1),
                cards: if after.is_none() {
                    self.cards.clone()
                } else {
                    Vec::new()
                },
            })
        }
    }

    #[test]
    fn future_fill_retry_is_pending_work_for_idle_eviction() {
        let source: Arc<dyn FillSource> = Arc::new(TestSource(pin(1)));
        let now = Instant::now();
        let retry = now + FAULT_RETRY;
        let mut inbox = Inbox::default();
        inbox.slots.insert(
            1,
            Slot {
                source: Arc::downgrade(&source),
                head: Arc::new(HeadSlot::new(Weak::new(), 0, Weak::new())),
                root_key: None,
                startup: false,
                cancel: Arc::new(AtomicBool::new(false)),
                queued: false,
                dirty: false,
                running: false,
                running_pin: None,
                completed: None,
                retries: 1,
                retry_at: Some(retry),
                fault_cooldown: None,
            },
        );
        assert!(
            inbox.has_pending_work(),
            "a future retry defers idle eviction"
        );
        assert!(inbox.ready.is_empty());
        assert_eq!(
            inbox.slots.values().filter_map(|slot| slot.retry_at).min(),
            Some(retry)
        );
        // The driver ignores idle remaining while this predicate is true and
        // waits for the retry deadline instead of spinning at expired idle.
        inbox.slots.get_mut(&1).unwrap().retry_at = None;
        assert!(!inbox.has_pending_work());
    }

    #[test]
    fn completed_pin_skips_pages_until_cache_invalidation() {
        use crate::store::topology::TopologyRoots;
        let directory = tempfile::tempdir().unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            directory.path().join("cache"),
            directory.path().join("data"),
        );
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let service = Arc::new(
            SearchService::with_embedder_for_tests(
                roots.clone(),
                Arc::new(RecordingFactory {
                    loads: loads.clone(),
                    drops: drops.clone(),
                    calls: calls.clone(),
                    pause: None,
                }),
            )
            .unwrap(),
        );
        let model_id = model_id_for_card_format(CardFormat::V1.as_str());
        let cards = (0..BATCH)
            .map(|index| HeadSearchCard {
                syntax_id: format!("sid:v1:{index:032x}"),
                card_text: "cached".into(),
                card_hash: format!("{index:064x}"),
            })
            .collect::<Vec<_>>();
        service
            .with_vector_writer(|writer| {
                let entries = cards
                    .iter()
                    .map(|card| {
                        (
                            model_id.as_str(),
                            card.card_hash.as_str(),
                            &[1; DIMENSIONS][..],
                        )
                    })
                    .collect::<Vec<_>>();
                writer.put_many(&entries)
            })
            .unwrap();
        let source = CachedSource {
            cards,
            pages: std::sync::atomic::AtomicUsize::new(0),
        };
        let backend = LocalBackend(service.clone());
        let first = backend.run(&source, &AtomicBool::new(false), None, &|_| {});
        first.result.unwrap();
        assert_eq!(source.pages.load(Ordering::Relaxed), 2);
        let completed = Some(Completed {
            pin: pin(1),
            cache_epoch: first.cache_epoch,
        });
        backend
            .run(&source, &AtomicBool::new(false), completed, &|_| {})
            .result
            .unwrap();
        assert_eq!(
            source.pages.load(Ordering::Relaxed),
            3,
            "only the pin probe is read"
        );
        assert_eq!(loads.load(Ordering::Relaxed), 0);
        assert!(calls.lock().unwrap().is_empty());
        {
            let path = std::fs::canonicalize(roots.vectors_db()).unwrap();
            let _witness = crate::store::retained_sqlite_file(&path, true, false, true).unwrap();
            let db = crate::store::protected_sqlite_open(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            )
            .unwrap();
            db.pragma_update(None, "user_version", 2).unwrap();
        }
        let error = service
            .with_vector_snapshot::<()>(|_| panic!("corrupt cache is not absence"))
            .unwrap_err();
        assert!(error.is::<crate::search_vectors::StoreUnavailable>());
        assert!(service.cache_epoch() > first.cache_epoch);
        assert_eq!(source.pages.load(Ordering::Relaxed), 3);
        assert_eq!(loads.load(Ordering::Relaxed), 0);
        assert!(calls.lock().unwrap().is_empty());

        // The read only invalidates. A later ordinary hint for the same pin
        // must recheck and fill, rather than fast-skip its completed job.
        backend
            .run(&source, &AtomicBool::new(false), completed, &|_| {})
            .result
            .unwrap();
        assert_eq!(
            source.pages.load(Ordering::Relaxed),
            5,
            "invalidated cache causes a whole job recheck"
        );
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), BATCH);
        drop(calls);
        let present = service
            .with_vector_snapshot(|snapshot| {
                let snapshot = snapshot.unwrap();
                assert_eq!(snapshot.entry_count(), BATCH as u64);
                snapshot.get(&model_id, &source.cards[0].card_hash)
            })
            .unwrap();
        assert_eq!(present, Some(vec![1; DIMENSIONS]));
    }

    type BatchPause = (Sender<()>, Arc<Mutex<Receiver<()>>>);

    struct RecordingFactory {
        loads: Arc<std::sync::atomic::AtomicUsize>,
        drops: Arc<std::sync::atomic::AtomicUsize>,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
        pause: Option<BatchPause>,
    }
    struct RecordingEmbedder {
        drops: Arc<std::sync::atomic::AtomicUsize>,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
        pause: Option<BatchPause>,
    }
    impl Drop for RecordingEmbedder {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }
    impl crate::search_embed::SearchEmbedderFactory for RecordingFactory {
        fn load(&self, _: &EmbeddingControl<'_>) -> Result<Box<dyn SearchEmbedder>, EmbedError> {
            self.loads.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(RecordingEmbedder {
                drops: self.drops.clone(),
                calls: self.calls.clone(),
                pause: self.pause.clone(),
            }))
        }
    }
    impl SearchEmbedder for RecordingEmbedder {
        fn embed(
            &self,
            texts: &[String],
            kind: InputKind,
            control: &EmbeddingControl<'_>,
        ) -> Result<Vec<crate::search_embed::QuantizedVector>, EmbedError> {
            if kind == InputKind::Document {
                let first = {
                    let mut calls = self.calls.lock().unwrap();
                    let first = calls.is_empty();
                    calls.push(texts.to_vec());
                    first
                };
                if first && let Some((entered, resume)) = &self.pause {
                    entered.send(()).unwrap();
                    resume
                        .lock()
                        .unwrap()
                        .recv_timeout(crate::testing::HANG_BACKSTOP)
                        .unwrap();
                }
            }
            control.check()?;
            Ok(texts
                .iter()
                .map(|_| crate::search_embed::QuantizedVector {
                    values: vec![1; DIMENSIONS],
                })
                .collect())
        }
    }
    struct PagedSource {
        cards: Vec<HeadSearchCard>,
        changed: AtomicBool,
        pages: std::sync::atomic::AtomicUsize,
    }
    impl FillSource for PagedSource {
        fn page(&self, expected: Option<IndexPin>, after: Option<&str>) -> Result<HeadSearchCards> {
            self.pages.fetch_add(1, Ordering::Relaxed);
            let current = pin(if self.changed.load(Ordering::Acquire) {
                2
            } else {
                1
            });
            if expected.is_some_and(|value| value != current) {
                return Err(SearchHeadChanged.into());
            }
            Ok(HeadSearchCards {
                pin: current,
                cards: self
                    .cards
                    .iter()
                    .filter(|card| after.is_none_or(|cursor| card.syntax_id.as_str() > cursor))
                    .take(BATCH)
                    .cloned()
                    .collect(),
            })
        }
    }
    fn paged_source() -> PagedSource {
        // Duplicate within batch and across pages. Fresh reader transactions
        // must observe the first batch's writes before deciding page two misses.
        PagedSource {
            cards: (0..BATCH + 3)
                .map(|index| HeadSearchCard {
                    syntax_id: format!("sid:v1:{index:032x}"),
                    card_text: format!(
                        "text-{}",
                        if index == BATCH + 2 {
                            BATCH
                        } else {
                            index % (BATCH - 1)
                        }
                    ),
                    card_hash: format!(
                        "{:064x}",
                        if index == BATCH + 2 {
                            BATCH
                        } else {
                            index % (BATCH - 1)
                        }
                    ),
                })
                .collect(),
            changed: AtomicBool::new(false),
            pages: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    fn recording_service(factory: RecordingFactory) -> (tempfile::TempDir, Arc<SearchService>) {
        let directory = tempfile::tempdir().unwrap();
        let roots = crate::store::topology::TopologyRoots::isolated_for_tests(
            directory.path().join("cache"),
            directory.path().join("data"),
        );
        let service =
            Arc::new(SearchService::with_embedder_for_tests(roots, Arc::new(factory)).unwrap());
        (directory, service)
    }

    struct MatrixOnlySource;
    impl FillSource for MatrixOnlySource {
        fn head(
            &self,
            previous: Option<IndexPin>,
            admit: &mut dyn FnMut(usize, usize, bool) -> Result<()>,
            check: &mut dyn FnMut() -> Result<()>,
        ) -> Result<Option<HeadUpdate>> {
            check()?;
            admit(
                usize::from(previous != Some(pin(1))),
                0,
                previous != Some(pin(1)),
            )?;
            let change = if previous == Some(pin(1)) {
                super::super::search_head::HeadChange::Delta {
                    removed_version_ids: Vec::new(),
                    added: Vec::new().into(),
                }
            } else {
                super::super::search_head::HeadChange::Full(
                    vec![crate::search_rank::SearchCandidate {
                        key: crate::search_rank::SearchCandidateKey {
                            version_id: 1,
                            syntax_id: "sid:v1:00000000000000000000000000000001".into(),
                        },
                        card_hash: "a".repeat(64),
                    }]
                    .into(),
                )
            };
            Ok(Some(HeadUpdate {
                pin: pin(1),
                declarations: 1,
                change,
            }))
        }
        fn page(&self, _: Option<IndexPin>, _: Option<&str>) -> Result<HeadSearchCards> {
            Ok(HeadSearchCards {
                pin: pin(1),
                cards: Vec::new(),
            })
        }
    }
    #[test]
    fn matrix_idle_is_per_checkout_even_with_a_pending_sibling_retry() {
        let (_directory, mut service) = recording_service(RecordingFactory {
            loads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            drops: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            calls: Arc::new(Mutex::new(Vec::new())),
            pause: None,
        });
        let now = Instant::now();
        let clock = Arc::new(Mutex::new(now));
        let observed = clock.clone();
        Arc::get_mut(&mut service)
            .unwrap()
            .set_model_clock_for_tests(Arc::new(move || *observed.lock().unwrap()));
        let core = Arc::new(Core::default());
        *core.search.lock().unwrap() = Arc::downgrade(&service);
        service.set_maintenance(Arc::downgrade(&core));
        let a_source: Arc<dyn FillSource> = Arc::new(TestSource(pin(1)));
        let a = core.register_source(Arc::downgrade(&a_source));
        let b_source: Arc<dyn FillSource> = Arc::new(TestSource(pin(2)));
        let b = core.register_source(Arc::downgrade(&b_source));
        let model_id = model_id_for_card_format(CardFormat::V1.as_str());
        let epoch = service.cache_epoch();
        let publish = |head: &HeadSlot, revision| {
            head.build(
                HeadUpdate {
                    pin: pin(revision),
                    declarations: 0,
                    change: super::super::search_head::HeadChange::Full(Vec::new().into()),
                },
                &model_id,
                epoch,
                None,
                &mut || Ok(()),
            )
            .unwrap();
        };
        publish(&a.head, 1);
        publish(&b.head, 2);
        let both = core.budget.0.lock().unwrap().used;
        assert!(both > 0);
        assert_eq!(
            core.expire_idle_matrices(now + Duration::from_secs(599)),
            Some(Duration::from_secs(1))
        );
        *clock.lock().unwrap() = now + Duration::from_secs(599);
        assert!(b.head.resident().unwrap());
        {
            let mut inbox = core.inbox.lock().unwrap();
            inbox.slots.get_mut(&b.id).unwrap().retry_at = Some(now + Duration::from_secs(601));
            assert!(inbox.has_pending_work());
        }
        // The same maintenance route runs between bounded fill batches.
        *clock.lock().unwrap() = now + Duration::from_secs(600);
        service.expire_idle_matrices();
        // B's query and pending work must not extend A's ten-minute timer.
        assert_eq!(
            core.expire_idle_matrices(now + Duration::from_secs(600)),
            Some(Duration::from_secs(599))
        );
        assert_eq!(a.head.declarations(pin(1), &model_id, epoch).unwrap(), None);
        assert!(a.head.dormant());
        assert_eq!(
            b.head.declarations(pin(2), &model_id, epoch).unwrap(),
            Some(0)
        );
        let remaining = core.budget.0.lock().unwrap().used;
        assert!(remaining > 0 && remaining < both);
        // A batch sweep skips the current owner job but does not reset its
        // timer. Finishing that job makes its already-expired head eligible.
        core.inbox
            .lock()
            .unwrap()
            .slots
            .get_mut(&b.id)
            .unwrap()
            .running = true;
        assert_eq!(
            core.expire_idle_matrices(now + Duration::from_secs(1199)),
            None
        );
        assert_eq!(
            b.head.declarations(pin(2), &model_id, epoch).unwrap(),
            Some(0)
        );
        core.inbox
            .lock()
            .unwrap()
            .slots
            .get_mut(&b.id)
            .unwrap()
            .running = false;
        assert_eq!(
            core.expire_idle_matrices(now + Duration::from_secs(1199)),
            None
        );
        assert!(b.head.dormant());
        assert_eq!(core.budget.0.lock().unwrap().used, 0);
    }

    #[test]
    fn idle_matrix_and_model_expire_and_periodic_hints_do_not_reload_them() {
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_directory, mut service) = recording_service(RecordingFactory {
            loads: loads.clone(),
            drops: drops.clone(),
            calls: Arc::new(Mutex::new(Vec::new())),
            pause: None,
        });
        let now = Instant::now();
        let clock = Arc::new(Mutex::new(now));
        let observed = clock.clone();
        Arc::get_mut(&mut service)
            .unwrap()
            .set_model_clock_for_tests(Arc::new(move || *observed.lock().unwrap()));
        let owner = SearchFill::start(service.clone()).unwrap();
        let source: Arc<dyn FillSource> = Arc::new(MatrixOnlySource);
        let registration = owner.register_source(Arc::downgrade(&source));
        registration.notify();
        owner.wait_until_idle_for_tests();
        assert!(owner.matrix_storage_bytes() > 0);
        assert_eq!(loads.load(Ordering::Acquire), 1);
        assert_eq!(drops.load(Ordering::Acquire), 0);

        *clock.lock().unwrap() = now + Duration::from_secs(600);
        service.wake_model_clock_for_tests();
        service.wait_for_model_eviction_for_tests();
        assert_eq!(owner.matrix_storage_bytes(), 0);
        assert_eq!(drops.load(Ordering::Acquire), 1);

        registration.notify();
        owner.wait_until_idle_for_tests();
        assert_eq!(owner.matrix_storage_bytes(), 0);
        assert_eq!(
            loads.load(Ordering::Acquire),
            1,
            "periodic hints cannot revive evicted search state"
        );

        registration.head_slot().request();
        owner.wait_until_idle_for_tests();
        assert!(owner.matrix_storage_bytes() > 0);
        assert_eq!(loads.load(Ordering::Acquire), 2);
        assert_eq!(drops.load(Ordering::Acquire), 1);
    }

    #[test]
    fn production_fill_pages_deduplicates_and_reuses_reader_after_put_many() {
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (_directory, service) = recording_service(RecordingFactory {
            loads: loads.clone(),
            drops: drops.clone(),
            calls: calls.clone(),
            pause: None,
        });
        let source = paged_source();
        let backend = LocalBackend(service.clone());
        let outcome = backend.run(&source, &AtomicBool::new(false), None, &|_| {});
        outcome.result.unwrap();
        assert_eq!(source.pages.load(Ordering::Relaxed), 2);
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            [BATCH - 1, 1]
        );
        assert_eq!(service.vector_report().unwrap().entries, BATCH as u64);
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        backend
            .run(
                &source,
                &AtomicBool::new(false),
                Some(Completed {
                    pin: pin(1),
                    cache_epoch: outcome.cache_epoch,
                }),
                &|_| {},
            )
            .result
            .unwrap();
        assert_eq!(source.pages.load(Ordering::Relaxed), 3);
        assert_eq!(loads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn head_change_mid_loaded_fill_keeps_committed_progress_and_resident_model() {
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (entered, events) = mpsc::channel();
        let (resume, proceed) = mpsc::channel();
        let (_directory, service) = recording_service(RecordingFactory {
            loads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            drops: drops.clone(),
            calls: Arc::new(Mutex::new(Vec::new())),
            pause: Some((entered, Arc::new(Mutex::new(proceed)))),
        });
        let source = Arc::new(paged_source());
        let job_source = source.clone();
        let job_service = service.clone();
        let job = std::thread::spawn(move || {
            LocalBackend(job_service).run(
                job_source.as_ref(),
                &AtomicBool::new(false),
                None,
                &|_| {},
            )
        });
        events.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        source.changed.store(true, Ordering::Release);
        resume.send(()).unwrap();
        assert!(
            job.join()
                .unwrap()
                .result
                .unwrap_err()
                .is::<SearchHeadChanged>()
        );
        assert_eq!(service.vector_report().unwrap().entries, (BATCH - 1) as u64);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        LocalBackend(service.clone())
            .run(source.as_ref(), &AtomicBool::new(false), None, &|_| {})
            .result
            .unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert_eq!(
            service.vector_report().unwrap().entries,
            BATCH as u64,
            "reconciliation fills only the remaining misses"
        );
    }

    struct FaultOnceSource {
        source: PagedSource,
        fault: AtomicBool,
    }
    impl FillSource for FaultOnceSource {
        fn page(&self, pin: Option<IndexPin>, after: Option<&str>) -> Result<HeadSearchCards> {
            if pin.is_some() && self.fault.swap(false, Ordering::AcqRel) {
                return Err(StorageBusy.into());
            }
            self.source.page(pin, after)
        }
    }

    #[test]
    fn ordinary_read_fault_preserves_committed_batches_for_reconciliation() {
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_directory, service) = recording_service(RecordingFactory {
            loads: loads.clone(),
            drops: drops.clone(),
            calls: Arc::new(Mutex::new(Vec::new())),
            pause: None,
        });
        let source = FaultOnceSource {
            source: paged_source(),
            fault: AtomicBool::new(true),
        };
        let backend = LocalBackend(service.clone());
        assert!(
            backend
                .run(&source, &AtomicBool::new(false), None, &|_| {})
                .result
                .unwrap_err()
                .is::<StorageBusy>()
        );
        assert_eq!(service.vector_report().unwrap().entries, (BATCH - 1) as u64);
        backend
            .run(&source, &AtomicBool::new(false), None, &|_| {})
            .result
            .unwrap();
        assert_eq!(
            loads.load(Ordering::Relaxed),
            1,
            "reconcile reuses the model for the remaining page-two miss"
        );
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert_eq!(service.vector_report().unwrap().entries, BATCH as u64);
    }

    #[test]
    fn cancelled_loaded_fill_retains_weights_without_publishing_unfinished_batch() {
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (entered, events) = mpsc::channel();
        let (resume, proceed) = mpsc::channel();
        let (_directory, service) = recording_service(RecordingFactory {
            loads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            drops: drops.clone(),
            calls: Arc::new(Mutex::new(Vec::new())),
            pause: Some((entered, Arc::new(Mutex::new(proceed)))),
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let job_cancel = cancel.clone();
        let job_service = service.clone();
        let job = std::thread::spawn(move || {
            LocalBackend(job_service).run(&paged_source(), &job_cancel, None, &|_| {})
        });
        events.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        cancel.store(true, Ordering::Release);
        resume.send(()).unwrap();
        assert!(matches!(
            job.join()
                .unwrap()
                .result
                .unwrap_err()
                .downcast_ref::<EmbedError>(),
            Some(EmbedError::Cancelled)
        ));
        assert_eq!(service.vector_report().unwrap().entries, 0);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert!(
            service
                .query_embedding(
                    "after cancellation",
                    &Arc::new(AtomicBool::new(false)),
                    Instant::now() + crate::testing::HANG_BACKSTOP,
                    None
                )
                .unwrap()
                .vector
                .is_some()
        );
        assert_eq!(drops.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn queued_query_is_served_by_fill_without_a_second_model_load() {
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (entered, events) = mpsc::channel();
        let (resume, proceed) = mpsc::channel();
        let (_directory, service) = recording_service(RecordingFactory {
            loads: loads.clone(),
            drops: drops.clone(),
            calls: Arc::new(Mutex::new(Vec::new())),
            pause: Some((entered, Arc::new(Mutex::new(proceed)))),
        });
        let source = paged_source();
        let job_service = service.clone();
        let job = std::thread::spawn(move || {
            LocalBackend(job_service).run(&source, &AtomicBool::new(false), None, &|_| {})
        });
        events.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        let query_service = service.clone();
        let query = std::thread::spawn(move || {
            query_service.query_embedding(
                "queued",
                &Arc::new(AtomicBool::new(false)),
                Instant::now() + crate::testing::HANG_BACKSTOP,
                None,
            )
        });
        service.wait_for_queued_query_for_tests();
        resume.send(()).unwrap();
        assert!(query.join().unwrap().unwrap().vector.is_some());
        job.join().unwrap().result.unwrap();
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn unconfigured_background_fill_never_creates_optional_vector_cache() {
        use crate::{search_model::SearchConfig, store::topology::TopologyRoots};
        let directory = tempfile::tempdir().unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            directory.path().join("cache"),
            directory.path().join("data"),
        );
        let service = Arc::new(SearchService::new(roots.clone(), SearchConfig::default()).unwrap());
        let backend = LocalBackend(service);
        let outcome = backend.run(&TestSource(pin(1)), &AtomicBool::new(false), None, &|_| {});
        assert_eq!(outcome.pin, Some(pin(1)));
        assert!(matches!(
            outcome.result.unwrap_err().downcast_ref::<EmbedError>(),
            Some(EmbedError::ModelUnavailable(_))
        ));
        assert!(!roots.vectors_db().exists());
    }

    #[test]
    fn index_read_contention_and_model_runtime_faults_remain_retryable() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open_for_tests(state.path(), root.path()).unwrap();
        let identity =
            crate::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
                .unwrap();
        let path = state
            .path()
            .join("cache/indexes")
            .join(identity.root_key)
            .join("index.db");
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let db = rusqlite::Connection::open(path).unwrap();
            db.execute_batch("BEGIN EXCLUSIVE").unwrap();
            locked_tx.send(()).unwrap();
            release_rx
                .recv_timeout(crate::testing::HANG_BACKSTOP)
                .unwrap();
            db.execute_batch("ROLLBACK").unwrap();
        });
        locked_rx
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .unwrap();
        // Use the existing read-scope deadline to make the held-lock refusal
        // deterministic, rather than waiting for SQLite's ordinary busy budget.
        let error = crate::store::with_read_deadline(Instant::now(), || {
            store.head_search_cards(None, None, BATCH)
        })
        .unwrap_err();
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(error.is::<crate::store::SqliteContention>(), "{error:#}");
        assert!(retryable_fault(&error));
        assert!(retryable_fault(
            &EmbedError::Runtime("device operation failed".into()).into()
        ));
        assert!(!retryable_fault(
            &EmbedError::ModelUnavailable("not configured".into()).into()
        ));
        assert!(!retryable_fault(&crate::search_vectors::Capacity.into()));
    }

    #[test]
    fn ordinary_contention_has_a_finite_owner_retry_budget() {
        let (owner, events, resume) = paused(true);
        let (_source, registration) = register(&owner, 1);
        registration.notify();
        for _ in 0..=MAX_FAULT_RETRIES {
            assert_eq!(event(&events).0, pin(1));
            resume.send(()).unwrap();
        }
        wait_for(&owner.core, |inbox| {
            let slot = &inbox.slots[&registration.id];
            !slot.running && slot.retry_at.is_none() && slot.fault_cooldown.is_some()
        });
        assert!(events.try_recv().is_err());
        registration.notify();
        assert_eq!(event(&events).0, pin(1));
        drop(registration);
        resume.send(()).unwrap();
    }
}
