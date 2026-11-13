//! One daemon-wide owner for disposable, post-publication vector filling.
#[cfg(test)]
#[path = "search_fill_pipeline_tests.rs"]
mod pipeline_tests;
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
#[cfg(test)]
use crate::search_embed::InputKind;
use crate::{
    http::DaemonState,
    model::IndexPin,
    search_card::CardFormat,
    search_embed::{
        DIMENSIONS, DOCUMENT_BATCH_MAX, DOCUMENT_BATCH_TOKEN_WORK_MAX, DocumentTokenizer,
        EmbedError, EmbeddingControl, PreparedDocument, SearchEmbedder, model_id_for_card_format,
    },
    search_vectors::VectorCacheReader,
    store::{HeadSearchCards, SEARCH_CARD_BATCH_SIZE, SearchHeadChanged, topology::StorageBusy},
};
use anyhow::Result;

const BATCH: usize = SEARCH_CARD_BATCH_SIZE;
const FAULT_RETRY: Duration = Duration::from_millis(250);
const MAX_FAULT_RETRIES: usize = 3;
const MATRIX_RAM_LIMIT: usize = 2 * 1024 * 1024 * 1024;
const TOKEN_WINDOW: usize = 512;
// Covers pipelined int8 vectors, publication bookkeeping, and the normalized
// 2048-f32 host output while quantizing a slice. Model tensors/weights remain
// outside the matrix cap. Token ids/text are bounded separately by TOKEN_WINDOW.
pub(super) const MATRIX_FILL_SCRATCH: usize = DOCUMENT_BATCH_MAX
    * (5 * (DIMENSIONS + 256)
        + 2048 * std::mem::size_of::<f32>()
        + std::mem::size_of::<Vec<f32>>());

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
            #[cfg(any(test, feature = "test-support"))]
            head.run_before_build_hook_for_tests();
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
        reader: Option<&mut VectorCacheReader>,
    ) -> Result<()> {
        // Helpers own only source/tokenizer and cache I/O. All inference and
        // matrix publication stay on the original model/fill owner.
        let tokenizer = model.document_tokenizer();
        let pending = Mutex::new(std::collections::BTreeSet::new());
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let (prepared_tx, prepared_rx) = std::sync::mpsc::sync_channel(1);
            let (write_tx, write_rx) = std::sync::mpsc::sync_channel::<FilledSlice>(1);
            let (commit_tx, commit_rx) = std::sync::mpsc::sync_channel(1);
            let pending_ref = &pending;
            let stop_ref = &stop;
            let producer = scope.spawn(move || {
                let result = pipeline_boundary(|| {
                    prepare_windows(
                        source,
                        cancel,
                        stop_ref,
                        model_id,
                        first_page,
                        reader,
                        tokenizer,
                        pending_ref,
                        |window| {
                            let sent = prepared_tx.send(Ok(Some(window))).is_ok();
                            if sent {
                                self.0.fill_progress();
                            }
                            sent
                        },
                    )
                });
                let _ = prepared_tx.send(result.map(|()| None));
                self.0.fill_progress();
            });
            let writer = scope.spawn(move || {
                while let Ok(slice) = write_rx.recv() {
                    let result = pipeline_boundary(|| {
                        #[cfg(test)]
                        {
                            let hook = self.0.before_fill_put.lock().unwrap().take();
                            if let Some(hook) = hook {
                                hook();
                            }
                        }
                        check_cancel(cancel)?;
                        self.0.with_vector_writer(|writer| {
                            let entries = slice
                                .hashes
                                .iter()
                                .zip(&slice.vectors)
                                .map(|(hash, vector)| {
                                    (model_id, hash.as_str(), vector.values.as_slice())
                                })
                                .collect::<Vec<_>>();
                            writer.put_many(&entries)
                        })?;
                        let mut pending = pending_ref.lock().unwrap();
                        for hash in &slice.hashes {
                            pending.remove(hash);
                        }
                        Ok(slice)
                    });
                    let failed = result.is_err();
                    if commit_tx.send(result).is_err() {
                        break;
                    }
                    self.0.fill_progress();
                    if failed {
                        break;
                    }
                }
            });
            let mut awaiting_commit = false;
            let commit = || -> Result<()> {
                let slice: FilledSlice =
                    self.0
                        .wait_fill_progress(model, || match commit_rx.try_recv() {
                            Ok(result) => Some(result),
                            Err(std::sync::mpsc::TryRecvError::Empty) => None,
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                Some(Err(EmbedError::Runtime("fill writer stopped".into()).into()))
                            }
                        })?;
                let vectors = slice
                    .hashes
                    .into_iter()
                    .zip(slice.vectors)
                    .map(|(hash, vector)| (hash, vector.values))
                    .collect::<Vec<_>>();
                self.0.publish_head_vectors(model_id, &vectors)
            };
            let result = (|| {
                loop {
                    self.0.expire_idle_matrices();
                    self.0.serve_queries(model);
                    check_cancel(cancel)?;
                    let window =
                        self.0
                            .wait_fill_progress(model, || match prepared_rx.try_recv() {
                                Ok(result) => Some(result),
                                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Ok(None)),
                            })?;
                    let Some(window) = window else {
                        break;
                    };
                    let mut offset = 0;
                    while offset < window.documents.len() {
                        self.0.serve_queries(model);
                        check_cancel(cancel)?;
                        let end = slice_end(&window.documents, offset);
                        let vectors = model.embed_prepared(
                            &window.documents[offset..end],
                            &EmbeddingControl::new(cancel),
                        )?;
                        if vectors.len() != end - offset {
                            return Err(EmbedError::Runtime(
                                "document embedding returned the wrong batch size".into(),
                            )
                            .into());
                        }
                        self.0.serve_queries(model);
                        check_cancel(cancel)?;
                        // Overlap this inference with the previous put_many,
                        // but publish its acknowledgement before adding work.
                        if awaiting_commit {
                            awaiting_commit = false;
                            commit()?;
                        }
                        write_tx
                            .send(FilledSlice {
                                hashes: window.hashes[offset..end].to_vec(),
                                vectors,
                            })
                            .map_err(|_| EmbedError::Runtime("fill writer stopped".into()))?;
                        awaiting_commit = true;
                        offset = end;
                    }
                }
                Ok(())
            })();
            // Drain already-submitted writes on ordinary read/inference faults.
            // Cancellation still prevents an unfinished write from committing.
            let committed = if awaiting_commit { commit() } else { Ok(()) };
            stop.store(true, Ordering::Release);
            drop(prepared_rx);
            drop(write_tx);
            drop(commit_rx);
            producer
                .join()
                .map_err(|_| EmbedError::Runtime("fill reader panicked".into()))?;
            writer
                .join()
                .map_err(|_| EmbedError::Runtime("fill writer panicked".into()))?;
            result.and(committed)
        })
    }
}
fn pipeline_boundary<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
        .unwrap_or_else(|_| Err(EmbedError::Runtime("fill pipeline stage panicked".into()).into()))
}

struct PreparedWindow {
    hashes: Vec<String>,
    documents: Vec<PreparedDocument>,
}
struct FilledSlice {
    hashes: Vec<String>,
    vectors: Vec<crate::search_embed::QuantizedVector>,
}

fn slice_end(documents: &[PreparedDocument], start: usize) -> usize {
    let mut end = start;
    let mut longest = 0;
    while end < documents.len() && end - start < DOCUMENT_BATCH_MAX {
        longest = longest.max(documents[end].token_len());
        if (end - start + 1) * longest > DOCUMENT_BATCH_TOKEN_WORK_MAX {
            break;
        }
        end += 1;
    }
    end
}

#[allow(clippy::too_many_arguments)]
fn prepare_windows(
    source: &dyn FillSource,
    cancel: &AtomicBool,
    stop: &AtomicBool,
    model_id: &str,
    first_page: (HeadSearchCards, Vec<bool>),
    mut reader: Option<&mut VectorCacheReader>,
    tokenizer: DocumentTokenizer,
    pending: &Mutex<std::collections::BTreeSet<String>>,
    mut publish: impl FnMut(PreparedWindow) -> bool,
) -> Result<()> {
    let (mut page, _) = first_page;
    let mut terminal = None;
    loop {
        let mut hashes = Vec::new();
        let mut documents = Vec::new();
        let mut scanned = 0;
        loop {
            check_cancel(cancel)?;
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            // Keep hashes until their writes commit. This bounds deduplication
            // by pipeline capacity, rather than retaining the entire head.
            let cards = {
                let mut pending = pending.lock().unwrap();
                let missing = LocalBackend::missing(reader.as_deref_mut(), model_id, &page)?;
                page.cards
                    .iter()
                    .zip(&missing)
                    .filter(|(card, absent)| **absent && pending.insert(card.card_hash.clone()))
                    .map(|(card, _)| card)
                    .collect::<Vec<_>>()
            };
            let texts = cards
                .iter()
                .map(|card| card.card_text.clone())
                .collect::<Vec<_>>();
            documents.extend(tokenizer.prepare(&texts, &EmbeddingControl::new(cancel))?);
            hashes.extend(cards.iter().map(|card| card.card_hash.clone()));
            scanned += page.cards.len();
            let Some(after) = next_cursor(&page) else {
                break;
            };
            if scanned >= TOKEN_WINDOW {
                // Read the following page only after publishing this window.
                break;
            }
            match source.page(Some(page.pin), Some(&after)) {
                Ok(next) => {
                    page = next;
                }
                Err(error) => {
                    terminal = Some(error);
                    break;
                }
            }
        }
        let mut order = (0..documents.len()).collect::<Vec<_>>();
        order.sort_by_key(|&index| documents[index].token_len());
        // Move rather than clone token buffers into sorted order.
        let mut pairs = hashes
            .into_iter()
            .zip(documents)
            .map(Some)
            .collect::<Vec<_>>();
        let mut window = PreparedWindow {
            hashes: Vec::with_capacity(order.len()),
            documents: Vec::with_capacity(order.len()),
        };
        for index in order {
            let (hash, document) = pairs[index].take().unwrap();
            window.hashes.push(hash);
            window.documents.push(document);
        }
        if !window.documents.is_empty() && !publish(window) {
            return Ok(());
        }
        if let Some(error) = terminal {
            return Err(error);
        }
        let Some(after) = next_cursor(&page) else {
            return Ok(());
        };
        check_cancel(cancel)?;
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        page = source.page(Some(page.pin), Some(&after))?;
    }
}

/// Diagnostic admission through the production paging, tokenizing and sorting path.
/// Ordinals travel as card hashes so callers can restore the original row order.
#[cfg(feature = "test-support")]
#[allow(clippy::type_complexity)]
pub fn prepare_document_windows_for_tests(
    texts: &[String],
    tokenizer: DocumentTokenizer,
    cancel: &AtomicBool,
) -> Result<Vec<(Vec<usize>, Vec<PreparedDocument>)>> {
    struct Documents<'a>(&'a [String]);
    impl FillSource for Documents<'_> {
        fn page(&self, _: Option<IndexPin>, after: Option<&str>) -> Result<HeadSearchCards> {
            let start = after.map_or(Ok(0), |cursor| cursor.parse::<usize>().map(|n| n + 1))?;
            Ok(HeadSearchCards {
                pin: IndexPin {
                    index_generation: uuid::Uuid::nil(),
                    index_revision: 1,
                },
                cards: self
                    .0
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(BATCH)
                    .map(|(ordinal, text)| crate::store::HeadSearchCard {
                        syntax_id: ordinal.to_string(),
                        card_hash: ordinal.to_string(),
                        card_text: text.clone(),
                    })
                    .collect(),
            })
        }
    }
    let source = Documents(texts);
    let first = source.page(None, None)?;
    let missing = vec![true; first.cards.len()];
    let mut windows = Vec::new();
    prepare_windows(
        &source,
        cancel,
        &AtomicBool::new(false),
        &model_id_for_card_format(CardFormat::V1.as_str()),
        (first, missing),
        None,
        tokenizer,
        &Mutex::new(Default::default()),
        |window| {
            let ordinals = window
                .hashes
                .iter()
                .map(|hash| hash.parse().unwrap())
                .collect();
            windows.push((ordinals, window.documents));
            true
        },
    )?;
    Ok(windows)
}

#[cfg(feature = "test-support")]
pub fn prepared_document_slice_end_for_tests(
    documents: &[PreparedDocument],
    start: usize,
) -> usize {
    slice_end(documents, start)
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
    /// Runs in `notify` after it released the inbox, so a test can park a
    /// notifier at that boundary while the owner decides (#777).
    #[cfg(test)]
    after_notify_release: Mutex<Option<Box<dyn FnOnce() + Send>>>,
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
        inbox = self.inbox.lock().unwrap();
        if inbox.shutdown {
            return;
        }
        let Some(slot) = inbox.slots.get_mut(&id) else {
            return;
        };
        // Publish pending in the same inbox transition that records the work
        // (#777): the owner's settle is also an inbox transition, so the two
        // are totally ordered and neither can undo the other.
        head.begin();
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
        let mut inbox = core.inbox.lock().unwrap();
        let Some(slot) = inbox.slots.get_mut(&self.id) else {
            return;
        };
        // Mark pending inside the inbox transition that records this hint, so
        // it is ordered with the owner's retry/settle decision (#777). A hint
        // recorded before an exhausting attempt ends is consumed by that
        // settle; one recorded after it marks the head again.
        self.head.begin();
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
        drop(inbox);
        #[cfg(test)]
        if let Some(hook) = core.after_notify_release.lock().unwrap().take() {
            hook();
        }
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
            // The owner decided no retry remains: publish that the head is
            // settled, or describe/status keep reading busy work as pending
            // (#777). A later hint begins it again after its inbox update.
            head.settle();
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
#[path = "search_fill_tests.rs"]
mod tests;
