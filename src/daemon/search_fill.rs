//! One daemon-wide owner for disposable, post-publication vector filling.
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use super::search::SearchService;
use crate::{
    http::DaemonState,
    model::IndexPin,
    search_card::CardFormat,
    search_embed::{EmbedError, Embedder, InputKind, model_id_for_card_format},
    store::{HeadSearchCards, SEARCH_CARD_BATCH_SIZE, SearchHeadChanged, topology::StorageBusy},
};
use anyhow::Result;

const BATCH: usize = SEARCH_CARD_BATCH_SIZE;
const FAULT_RETRY: Duration = Duration::from_millis(250);
const MAX_FAULT_RETRIES: usize = 3;

type Source = Weak<dyn FillSource>;
trait FillSource: Send + Sync {
    fn page(&self, pin: Option<IndexPin>, after: Option<&str>) -> Result<HeadSearchCards>;
}
impl FillSource for DaemonState {
    fn page(&self, pin: Option<IndexPin>, after: Option<&str>) -> Result<HeadSearchCards> {
        self.search_fill_page(pin, after, BATCH)
    }
}

struct Outcome {
    pin: Option<IndexPin>,
    result: Result<()>,
}
trait FillBackend: Send + Sync {
    fn run(
        &self,
        source: &dyn FillSource,
        cancel: &AtomicBool,
        completed: Option<IndexPin>,
        publish: &dyn Fn(IndexPin),
    ) -> Outcome;
}
struct LocalBackend(Arc<SearchService>);
impl FillBackend for LocalBackend {
    fn run(
        &self,
        source: &dyn FillSource,
        cancel: &AtomicBool,
        _completed: Option<IndexPin>,
        publish: &dyn Fn(IndexPin),
    ) -> Outcome {
        let mut pin = None;
        let result = (|| {
            check_cancel(cancel)?;
            let mut page = source.page(None, None)?;
            pin = Some(page.pin);
            publish(page.pin);
            // A completed head can lose evictable vectors without changing its
            // index pin. Every owner hint rechecks misses before skipping work.
            if !self.0.model_configured() {
                return Err(EmbedError::ModelUnavailable(
                    "no local search model is configured".into(),
                )
                .into());
            }
            let model_id = model_id_for_card_format(CardFormat::V1.as_str());
            self.0.with_vector_writer(|_| Ok(()))?;
            // No model is loaded when all pages are already cached.
            loop {
                check_cancel(cancel)?;
                let missing = self.missing(&model_id, &page)?;
                if missing.iter().any(|value| *value) {
                    // One head-scoped job, with bounded pages and no index/cache
                    // snapshot held while loading or running the model.
                    return self.0.with_model_job(cancel, |model| {
                        Ok(self.fill_loaded(source, cancel, model, &model_id, page, missing))
                    })?;
                }
                let Some(after) = next_cursor(&page) else {
                    return Ok(());
                };
                page = source.page(pin, Some(&after))?;
            }
        })();
        Outcome { pin, result }
    }
}
impl LocalBackend {
    fn missing(&self, model_id: &str, page: &HeadSearchCards) -> Result<Vec<bool>> {
        let hashes = page
            .cards
            .iter()
            .map(|card| card.card_hash.as_str())
            .collect::<Vec<_>>();
        self.0.missing_vectors(model_id, &hashes)
    }

    fn fill_loaded(
        &self,
        source: &dyn FillSource,
        cancel: &AtomicBool,
        model: &Embedder,
        model_id: &str,
        mut page: HeadSearchCards,
        mut missing: Vec<bool>,
    ) -> Result<()> {
        loop {
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
                let texts = cards
                    .iter()
                    .map(|card| card.card_text.clone())
                    .collect::<Vec<_>>();
                let vectors = model.embed(&texts, InputKind::Document, cancel)?;
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
            }
            let Some(after) = next_cursor(&page) else {
                return Ok(());
            };
            check_cancel(cancel)?;
            page = source.page(Some(page.pin), Some(&after))?;
            missing = self.missing(model_id, &page)?;
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
    cancel: Arc<AtomicBool>,
    queued: bool,
    dirty: bool,
    running: bool,
    running_pin: Option<IndexPin>,
    completed: Option<IndexPin>,
    retries: usize,
    retry_at: Option<Instant>,
    fault_cooldown: Option<Instant>,
}
#[derive(Default)]
struct Inbox {
    next_id: u64,
    slots: BTreeMap<u64, Slot>,
    ready: VecDeque<u64>,
    shutdown: bool,
}
#[derive(Default)]
struct Core {
    inbox: Mutex<Inbox>,
    changed: Condvar,
}

/// Owns one blocking driver. Its inbox contains only weak checkout sources.
/// Dropping the service cancels work and wakes the driver; it never joins while
/// a checkout or registry lock might be held.
pub struct SearchFill {
    core: Arc<Core>,
}
impl SearchFill {
    pub fn start(search: Arc<SearchService>) -> Result<Arc<Self>> {
        Self::start_with(Arc::new(LocalBackend(search)))
    }

    fn start_with(backend: Arc<dyn FillBackend>) -> Result<Arc<Self>> {
        let core = Arc::new(Core::default());
        let driver = core.clone();
        std::thread::Builder::new()
            .name("trellis-search-fill".into())
            .spawn(move || drive(driver, backend))?;
        Ok(Arc::new(Self { core }))
    }

    pub(crate) fn register(&self, scheduler: &Arc<DaemonState>) -> FillRegistration {
        let source: Arc<dyn FillSource> = scheduler.clone();
        self.register_source(Arc::downgrade(&source))
    }

    fn register_source(&self, source: Source) -> FillRegistration {
        let mut inbox = self.core.inbox.lock().unwrap();
        let id = inbox.next_id;
        inbox.next_id += 1;
        inbox.slots.insert(
            id,
            Slot {
                source,
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
        FillRegistration {
            core: Arc::downgrade(&self.core),
            id,
        }
    }
}
impl Drop for SearchFill {
    fn drop(&mut self) {
        let mut inbox = self.core.inbox.lock().unwrap();
        inbox.shutdown = true;
        for slot in inbox.slots.values() {
            slot.cancel.store(true, Ordering::Release);
        }
        self.core.changed.notify_one();
    }
}

pub(crate) struct FillRegistration {
    core: Weak<Core>,
    id: u64,
}
impl FillRegistration {
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
        let mut inbox = core.inbox.lock().unwrap();
        if let Some(slot) = inbox.slots.remove(&self.id) {
            slot.cancel.store(true, Ordering::Release);
        }
        // Removing the ID keeps the queue bounded across runtime replacement.
        inbox.ready.retain(|id| *id != self.id);
        core.changed.notify_one();
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
        let (id, source, cancel, completed) = {
            let mut inbox = core.inbox.lock().unwrap();
            loop {
                if inbox.shutdown {
                    return;
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
                if let Some(id) = inbox.ready.pop_front() {
                    let Some(slot) = inbox.slots.get_mut(&id) else {
                        continue;
                    };
                    slot.queued = false;
                    slot.dirty = false;
                    slot.running = true;
                    slot.running_pin = None;
                    break (id, slot.source.clone(), slot.cancel.clone(), slot.completed);
                }
                let deadline = inbox.slots.values().filter_map(|slot| slot.retry_at).min();
                inbox = match deadline {
                    Some(at) => {
                        core.changed
                            .wait_timeout(inbox, at.saturating_duration_since(now))
                            .unwrap()
                            .0
                    }
                    None => core.changed.wait(inbox).unwrap(),
                };
            }
        };
        let Some(source) = source.upgrade() else {
            core.inbox.lock().unwrap().slots.remove(&id);
            continue;
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            backend.run(source.as_ref(), &cancel, completed, &|pin| {
                let mut inbox = core.inbox.lock().unwrap();
                if let Some(slot) = inbox.slots.get_mut(&id) {
                    slot.running_pin = Some(pin);
                }
                core.changed.notify_all();
            })
        }))
        .unwrap_or_else(|_| Outcome {
            pin: completed,
            result: Err(EmbedError::Runtime("fill job panicked".into()).into()),
        });
        // Release the source before publishing completion. It does not keep a
        // retired checkout alive while this owner waits for its next hint.
        drop(source);
        let mut inbox = core.inbox.lock().unwrap();
        let Some(slot) = inbox.slots.get_mut(&id) else {
            continue;
        };
        slot.running = false;
        slot.running_pin = None;
        let changed = outcome
            .result
            .as_ref()
            .is_err_and(|error| error.is::<SearchHeadChanged>());
        let retryable = outcome.result.as_ref().is_err_and(retryable_fault);
        if changed {
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
            slot.completed = outcome.pin;
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
            _: Option<IndexPin>,
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
    fn a_completed_head_rechecks_cache_and_retries_misses_without_a_new_pin() {
        use crate::{search_model::SearchConfig, store::topology::TopologyRoots};
        let directory = tempfile::tempdir().unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            directory.path().join("cache"),
            directory.path().join("data"),
        );
        // Deliberately missing files: a fully cached rescan must not load them.
        let service = Arc::new(
            SearchService::new(
                roots.clone(),
                SearchConfig::from_values(
                    Some(directory.path().join("missing-model")),
                    Some("cpu"),
                )
                .unwrap(),
            )
            .unwrap(),
        );
        let model_id = model_id_for_card_format(CardFormat::V1.as_str());
        let cards = (0..BATCH)
            .map(|index| crate::store::HeadSearchCard {
                syntax_id: format!("card-{index:03}"),
                card_text: "measured card".into(),
                card_hash: format!("{index:064x}"),
            })
            .collect::<Vec<_>>();
        service
            .with_vector_writer(|writer| {
                for card in &cards {
                    writer.put(&model_id, &card.card_hash, &vec![1; 1024])?;
                }
                Ok(())
            })
            .unwrap();
        let source = CachedSource {
            cards,
            pages: std::sync::atomic::AtomicUsize::new(0),
        };
        let backend = LocalBackend(service);
        let outcome = backend.run(&source, &AtomicBool::new(false), Some(pin(1)), &|_| {});
        outcome.result.unwrap();
        assert_eq!(
            source.pages.load(Ordering::Relaxed),
            2,
            "completed pin does not bypass cache recheck"
        );
        {
            let path = std::fs::canonicalize(roots.vectors_db()).unwrap();
            let _witness = crate::store::retained_sqlite_file(&path, true, false, true).unwrap();
            let db = crate::store::protected_sqlite_open(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            )
            .unwrap();
            // A miss at this same index head must cause a fresh model job.
            db.execute(
                "DELETE FROM vectors WHERE card_hash=?1",
                [&source.cards[0].card_hash],
            )
            .unwrap();
        }
        let outcome = backend.run(&source, &AtomicBool::new(false), Some(pin(1)), &|_| {});
        assert!(matches!(
            outcome.result.unwrap_err().downcast_ref::<EmbedError>(),
            Some(EmbedError::ModelUnavailable(_))
        ));
        assert_eq!(source.pages.load(Ordering::Relaxed), 3);
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
