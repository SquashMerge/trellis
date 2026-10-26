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
    search_embed::{DIMENSIONS, EmbedError, InputKind, SearchEmbedder, model_id_for_card_format},
    search_vectors::VectorCacheReader,
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
                let texts = cards
                    .iter()
                    .map(|card| card.card_text.clone())
                    .collect::<Vec<_>>();
                let vectors = model.embed(&texts, InputKind::Document, cancel)?;
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

    #[cfg(any(test, feature = "test-support"))]
    pub fn wait_until_idle_for_tests(&self) {
        let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
        let mut inbox = self.core.inbox.lock().unwrap();
        while inbox
            .slots
            .values()
            .any(|slot| slot.running || slot.queued || slot.retry_at.is_some())
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "fill did not settle");
            inbox = self.core.changed.wait_timeout(inbox, remaining).unwrap().0;
        }
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
            pin: completed.map(|done| done.pin),
            cache_epoch: completed.map_or(0, |done| done.cache_epoch),
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
                syntax_id: format!("card-{index:03}"),
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
        assert_eq!(drops.load(Ordering::Relaxed), 1);
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
        fn load(&self, _: &AtomicBool) -> Result<Box<dyn SearchEmbedder>, EmbedError> {
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
            cancel: &AtomicBool,
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
            if cancel.load(Ordering::Acquire) {
                return Err(EmbedError::Cancelled);
            }
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
                    syntax_id: format!("{index:04}"),
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
        assert_eq!(drops.load(Ordering::Relaxed), 1);
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
    fn head_change_mid_loaded_fill_keeps_committed_progress_and_drops_model() {
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
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        LocalBackend(service.clone())
            .run(source.as_ref(), &AtomicBool::new(false), None, &|_| {})
            .result
            .unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 2);
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
            2,
            "reconcile loads only for the remaining page-two miss"
        );
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert_eq!(service.vector_report().unwrap().entries, BATCH as u64);
    }

    #[test]
    fn cancelled_loaded_fill_drops_weights_without_publishing_unfinished_batch() {
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
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(
            service
                .query_embedding(
                    "after cancellation",
                    &Arc::new(AtomicBool::new(false)),
                    Instant::now() + crate::testing::HANG_BACKSTOP
                )
                .unwrap()
                .vector
                .is_some()
        );
        assert_eq!(drops.load(Ordering::Relaxed), 2);
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
            )
        });
        service.wait_for_queued_query_for_tests();
        resume.send(()).unwrap();
        assert!(query.join().unwrap().unwrap().vector.is_some());
        job.join().unwrap().result.unwrap();
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
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
