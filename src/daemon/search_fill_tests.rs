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
    let store = crate::testing::retry_admission(|| {
        Store::open(
            roots.clone(),
            WorkspaceIdentity::discover(Some(&checkout), &checkout)?,
        )
    });
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
        let db =
            crate::store::protected_sqlite_open(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
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
                    role: crate::native_evidence::DeclarationRole::Production,
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
        [BATCH]
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

struct BoundarySource {
    source: PagedSource,
    boundary: Mutex<Option<BatchPause>>,
}
impl FillSource for BoundarySource {
    fn page(&self, pin: Option<IndexPin>, after: Option<&str>) -> Result<HeadSearchCards> {
        if after == Some(format!("sid:v1:{:032x}", TOKEN_WINDOW - 1).as_str()) {
            let pause = self.boundary.lock().unwrap().take();
            if let Some((entered, resume)) = pause {
                entered.send(()).unwrap();
                resume
                    .lock()
                    .unwrap()
                    .recv_timeout(crate::testing::HANG_BACKSTOP)
                    .unwrap();
            }
        }
        self.source.page(pin, after)
    }
}

#[test]
fn head_change_at_prefetch_boundary_keeps_committed_progress_and_resident_model() {
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (entered, events) = mpsc::channel();
    let (resume, proceed) = mpsc::channel();
    let (_directory, service) = recording_service(RecordingFactory {
        loads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        drops: drops.clone(),
        calls: Arc::new(Mutex::new(Vec::new())),
        pause: Some((entered, Arc::new(Mutex::new(proceed)))),
    });
    let (reading, read) = mpsc::channel();
    let (advance, release) = mpsc::channel();
    let source = Arc::new(BoundarySource {
        source: PagedSource {
            cards: (0..TOKEN_WINDOW + 3)
                .map(|index| HeadSearchCard {
                    syntax_id: format!("sid:v1:{index:032x}"),
                    card_text: format!("text-{index}"),
                    card_hash: format!("{index:064x}"),
                })
                .collect(),
            changed: AtomicBool::new(false),
            pages: std::sync::atomic::AtomicUsize::new(0),
        },
        boundary: Mutex::new(Some((reading, Arc::new(Mutex::new(release))))),
    });
    let job_source = source.clone();
    let job_service = service.clone();
    let job = std::thread::spawn(move || {
        LocalBackend(job_service).run(job_source.as_ref(), &AtomicBool::new(false), None, &|_| {})
    });
    read.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
    events.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
    source.source.changed.store(true, Ordering::Release);
    advance.send(()).unwrap();
    resume.send(()).unwrap();
    assert!(
        job.join()
            .unwrap()
            .result
            .unwrap_err()
            .is::<SearchHeadChanged>()
    );
    assert_eq!(
        service.vector_report().unwrap().entries,
        TOKEN_WINDOW as u64
    );
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    LocalBackend(service.clone())
        .run(source.as_ref(), &AtomicBool::new(false), None, &|_| {})
        .result
        .unwrap();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert_eq!(
        service.vector_report().unwrap().entries,
        (TOKEN_WINDOW + 3) as u64,
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
    let store = crate::testing::retry_admission(|| {
        crate::store::Store::open_for_tests(state.path(), root.path())
    });
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
    // Exhausted retries leave no work: the idle head is not pending
    // without a new request or hint (#777).
    let head = registration.head_slot();
    assert!(!head.pending_for(pin(1), 0));
    registration.notify();
    assert!(head.pending_for(pin(1), 0));
    assert_eq!(event(&events).0, pin(1));
    drop(registration);
    resume.send(()).unwrap();
}

#[test]
fn hint_during_the_final_busy_attempt_cannot_undo_its_settlement() {
    let (owner, events, resume) = paused(true);
    let (_source, registration) = register(&owner, 1);
    let head = registration.head_slot();
    registration.notify();
    for _ in 0..MAX_FAULT_RETRIES {
        assert_eq!(event(&events).0, pin(1));
        resume.send(()).unwrap();
    }
    // The final attempt is running. A hint records itself, releases the
    // inbox and parks at that boundary before returning.
    assert_eq!(event(&events).0, pin(1));
    let (parked_tx, parked) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    *owner.core.after_notify_release.lock().unwrap() = Some(Box::new(move || {
        parked_tx.send(()).unwrap();
        released
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .unwrap();
    }));
    std::thread::scope(|scope| {
        let notifier = scope.spawn(|| registration.notify());
        parked.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        // The owner exhausts its budget and settles while the notifier
        // is parked after its inbox transition.
        resume.send(()).unwrap();
        wait_for(&owner.core, |inbox| {
            let slot = &inbox.slots[&registration.id];
            !slot.running && slot.retry_at.is_none() && slot.fault_cooldown.is_some()
        });
        assert!(!head.pending_for(pin(1), 0));
        release.send(()).unwrap();
        notifier.join().unwrap();
    });
    // Resuming the earlier notifier must not resurrect pending work.
    assert!(!head.pending_for(pin(1), 0));
    assert!(events.try_recv().is_err());
    // A genuine hint after settlement is still recorded and pending.
    registration.notify();
    assert!(head.pending_for(pin(1), 0));
    assert_eq!(event(&events).0, pin(1));
    drop(registration);
    resume.send(()).unwrap();
}
