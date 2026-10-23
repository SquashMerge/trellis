use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};
use trellis::{
    daemon::registry::{
        BROWSER_IDLE_DELAY, CHECKOUT_RELEASE_DELAY, CheckoutOptions, CheckoutRegistry,
        DAEMON_IDLE_DELAY,
    },
    store::topology::{TopologyRoots, WorkspaceIdentity},
};

fn identity(root: &Path) -> WorkspaceIdentity {
    WorkspaceIdentity::discover(Some(root), root)
        .unwrap()
        .attach_marker()
        .unwrap()
}
/// Records every tracked row's state at each FIFO claim, then re-arms for the
/// next claim. The worker runs the after-claim hook after the claim COMMITs and
/// before it starts the row's work, so each entry is an event-ordered snapshot
/// of the claim boundary. Unlike millisecond timestamps, it cannot pass a
/// reversed or overlapping claim order.
#[cfg(feature = "test-support")]
type ClaimLog = std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>;
#[cfg(feature = "test-support")]
fn record_claims(
    runtime: std::sync::Arc<trellis::daemon::registry::CheckoutRuntime>,
    store: trellis::store::Store,
    tracked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    log: ClaimLog,
) {
    let next = runtime.clone();
    runtime.set_after_claim_hook_for_tests(move || {
        let ids = tracked.lock().unwrap().clone();
        let states = ids
            .iter()
            .map(|id| store.request_by_id(id).unwrap().unwrap().state)
            .collect();
        log.lock().unwrap().push(states);
        record_claims(next, store, tracked, log);
    });
}

fn fixture() -> (
    tempfile::TempDir,
    WorkspaceIdentity,
    CheckoutRegistry,
    Instant,
) {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function original() {}\n").unwrap();
    let id = identity(&checkout);
    let now = Instant::now();
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots_at(roots, now);
    registry
        .register(&id, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    (base, id, registry, now)
}
async fn ready(runtime: &trellis::daemon::registry::CheckoutRuntime) {
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((response, catching_up)) = runtime.evidence_response() {
                let finished = response.finish(()).is_ok();
                if finished && !catching_up {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

async fn await_idle_release(registry: &mut CheckoutRegistry, at: Instant, key: &str) {
    let started = Instant::now();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let tick = registry.advance(at + started.elapsed()).unwrap();
            if !tick.released.is_empty() {
                assert_eq!(tick.released, vec![key.to_owned()]);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("checkout did not release after watcher quiescence");
}

async fn await_release_after_drain(registry: &mut CheckoutRegistry, at: Instant, key: &str) {
    let started = Instant::now();
    let mut released = false;
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let tick = registry.advance(at + started.elapsed()).unwrap();
            if !tick.released.is_empty() {
                assert_eq!(tick.released, vec![key.to_owned()]);
                assert!(!released, "checkout released more than once");
                released = true;
            }
            if tick.exit {
                assert!(released, "daemon exited before releasing the checkout");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("idle release did not finish after FIFO drain and watcher quiescence");
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn idle_exit_waits_for_release_when_read_finishes_between_idle_probes() {
    let (_base, id, mut registry, now) = fixture();
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    let (response, _) = runtime.evidence_response().unwrap();
    registry.disconnect_at(1, now);
    let due = now + DAEMON_IDLE_DELAY;
    let (probed_tx, probed_rx) = tokio::sync::oneshot::channel();
    let (continue_tx, continue_rx) = std::sync::mpsc::channel();
    registry.set_after_due_release_for_tests(move || {
        let _ = probed_tx.send(());
        let _ = continue_rx.recv();
    });
    let tick = tokio::task::spawn_blocking(move || {
        let result = registry.advance(due).unwrap();
        (registry, result)
    });
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, probed_rx)
        .await
        .expect("idle tick did not finish the due-release probe")
        .unwrap();
    assert!(runtime.has_active_resources());
    response.finish(()).unwrap();
    drop(response);
    continue_tx.send(()).unwrap();
    let (mut registry, first) = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, tick)
        .await
        .expect("idle tick did not resume")
        .unwrap();
    assert!(first.released.is_empty(), "active read had fenced release");
    assert!(!first.exit, "unreleased runtime must fence daemon exit");
    assert!(runtime.has_active_resources());
    await_idle_release(&mut registry, due, &id.root_key).await;
    assert!(!runtime.has_active_resources());
    assert!(registry.advance(due + Duration::from_secs(1)).unwrap().exit);
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn lost_active_index_releases_after_read_and_preserves_idle_clocks() {
    let (base, id, mut registry, now) = fixture();
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    let (response, _) = runtime.evidence_response().unwrap();
    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let holder = runtime.clone();
    let held = tokio::task::spawn_blocking(move || {
        holder.hold_worker_step_for_tests(|| {
            let _ = held_tx.send(());
            let _ = resume_rx.recv();
        });
    });
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, held_rx)
        .await
        .unwrap()
        .unwrap();
    let index_dir = base.path().join("cache/indexes").join(&id.root_key);
    fs::remove_dir_all(&index_dir).unwrap();
    registry.disconnect_at(1, now);
    let due = now + DAEMON_IDLE_DELAY;
    let tick = registry.advance(due).unwrap();
    assert!(tick.released.is_empty());
    assert!(!tick.exit);
    assert!(runtime.has_active_resources());
    // The owner cannot release while this already-admitted read is alive.
    drop(response);
    resume_tx.send(()).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, held)
        .await
        .unwrap()
        .unwrap();
    await_release_after_drain(&mut registry, due, &id.root_key).await;
    assert!(!runtime.has_active_resources());
    assert!(
        !index_dir.exists(),
        "release recreated the lost durable queue"
    );
    assert_eq!(registry.idle_exit_deadline(), Some(due));
}

#[tokio::test]
async fn lifecycle_driver_exits_when_startup_deadline_is_due() {
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(CheckoutRegistry::starting_at(
        Instant::now() - DAEMON_IDLE_DELAY,
    )));
    tokio::time::timeout(
        trellis::testing::HANG_BACKSTOP,
        trellis::daemon::run_idle_lifecycle(registry),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn browser_expiry_starts_a_separate_release_grace_and_exit_clock() {
    let (_base, id, mut registry, now) = fixture();
    registry.browser_request_at(&id, now).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    let epsilon = Duration::from_nanos(1);
    assert!(
        !registry
            .advance(now + BROWSER_IDLE_DELAY - epsilon)
            .unwrap()
            .exit
    );
    assert_eq!(registry.idle_exit_deadline(), None);
    assert!(runtime.has_active_resources());
    registry.advance(now + BROWSER_IDLE_DELAY).unwrap();
    assert_eq!(
        registry.idle_exit_deadline(),
        Some(now + BROWSER_IDLE_DELAY + DAEMON_IDLE_DELAY)
    );
    assert!(runtime.has_active_resources());
    assert!(
        registry
            .advance(now + BROWSER_IDLE_DELAY + CHECKOUT_RELEASE_DELAY - epsilon)
            .unwrap()
            .released
            .is_empty()
    );
    await_idle_release(
        &mut registry,
        now + BROWSER_IDLE_DELAY + CHECKOUT_RELEASE_DELAY,
        &id.root_key,
    )
    .await;
    assert!(!runtime.has_active_resources());
    assert!(
        !registry
            .advance(now + BROWSER_IDLE_DELAY + DAEMON_IDLE_DELAY - epsilon)
            .unwrap()
            .exit
    );
    assert!(
        registry
            .advance(now + BROWSER_IDLE_DELAY + DAEMON_IDLE_DELAY)
            .unwrap()
            .exit
    );
}

#[tokio::test]
async fn browser_client_cannot_collide_with_session_and_late_tick_keeps_deadlines() {
    let (_base, id, mut registry, now) = fixture();
    registry.attach_launch(u64::MAX, &id).unwrap();
    registry.browser_request_at(&id, now).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    registry.disconnect_at(u64::MAX, now);
    registry
        .advance(now + BROWSER_IDLE_DELAY - Duration::from_nanos(1))
        .unwrap();
    assert_eq!(registry.idle_exit_deadline(), None);
    let late = now + BROWSER_IDLE_DELAY + DAEMON_IDLE_DELAY + Duration::from_secs(1);
    await_release_after_drain(&mut registry, late, &id.root_key).await;
    assert!(!runtime.has_active_resources());
    assert_eq!(
        registry.idle_exit_deadline(),
        Some(now + BROWSER_IDLE_DELAY + DAEMON_IDLE_DELAY)
    );
}

#[tokio::test]
async fn final_mcp_disconnect_after_unpolled_browser_expiry_starts_its_own_grace() {
    let (_base, id, mut registry, now) = fixture();
    registry.attach_launch(1, &id).unwrap();
    registry.browser_request_at(&id, now).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    let disconnected = now + BROWSER_IDLE_DELAY + Duration::from_secs(5);
    registry.disconnect_at(1, disconnected);
    assert_eq!(
        registry.idle_exit_deadline(),
        Some(disconnected + DAEMON_IDLE_DELAY)
    );
    registry.disconnect_at(99, disconnected + Duration::from_secs(3));
    assert_eq!(
        registry.idle_exit_deadline(),
        Some(disconnected + DAEMON_IDLE_DELAY)
    );
    assert!(
        registry
            .advance(disconnected + CHECKOUT_RELEASE_DELAY - Duration::from_nanos(1))
            .unwrap()
            .released
            .is_empty()
    );
    await_idle_release(
        &mut registry,
        disconnected + CHECKOUT_RELEASE_DELAY,
        &id.root_key,
    )
    .await;
    assert!(!runtime.has_active_resources());
}

#[tokio::test]
async fn selected_browser_request_renews_virtual_client() {
    let (_base, id, mut registry, now) = fixture();
    registry.browser_request_at(&id, now).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    let next = now + BROWSER_IDLE_DELAY - Duration::from_nanos(1);
    registry.browser_request_at(&id, next).unwrap();
    registry.advance(now + BROWSER_IDLE_DELAY).unwrap();
    assert_eq!(registry.idle_exit_deadline(), None);
    assert!(runtime.has_active_resources());
    registry.advance(next + BROWSER_IDLE_DELAY).unwrap();
    assert_eq!(
        registry.idle_exit_deadline(),
        Some(next + BROWSER_IDLE_DELAY + DAEMON_IDLE_DELAY)
    );
}

#[tokio::test]
async fn final_session_disconnect_and_later_attach_reset_deadlines() {
    let (_base, id, mut registry, now) = fixture();
    registry.attach_launch(1, &id).unwrap();
    registry.attach_launch(2, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    registry.disconnect_at(1, now);
    assert_eq!(registry.idle_exit_deadline(), None);
    registry.disconnect_at(2, now + Duration::from_secs(4));
    assert_eq!(
        registry.idle_exit_deadline(),
        Some(now + Duration::from_secs(4) + DAEMON_IDLE_DELAY)
    );
    let before = now + Duration::from_secs(4) + CHECKOUT_RELEASE_DELAY;
    assert!(
        registry
            .advance(before - Duration::from_nanos(1))
            .unwrap()
            .released
            .is_empty()
    );
    registry.attach_launch(3, &id).unwrap();
    assert_eq!(registry.idle_exit_deadline(), None);
    assert!(registry.advance(before).unwrap().released.is_empty());
    registry.disconnect_at(3, before);
    await_idle_release(&mut registry, before + CHECKOUT_RELEASE_DELAY, &id.root_key).await;
    assert!(!runtime.has_active_resources());
}

#[test]
fn registration_and_selector_free_traffic_do_not_postpone_startup_exit() {
    let (_base, _id, mut registry, now) = fixture();
    assert!(
        !registry
            .advance(now + DAEMON_IDLE_DELAY - Duration::from_nanos(1))
            .unwrap()
            .exit
    );
    assert!(registry.advance(now + DAEMON_IDLE_DELAY).unwrap().exit);
}

#[tokio::test]
async fn queued_or_inflight_work_defers_expired_deadlines_until_drain() {
    let (_base, id, mut registry, now) = fixture();
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    registry.disconnect_at(1, now);
    registry.set_pending_work(&id.root_key, true).unwrap();
    let elapsed = now + DAEMON_IDLE_DELAY;
    // A real in-flight response keeps the owner alive even after both clocks expire.
    let (response, _) = runtime.evidence_response().unwrap();
    assert!(!registry.advance(elapsed).unwrap().exit);
    assert!(runtime.has_active_resources());
    response.finish(()).unwrap();
    drop(response);
    registry.set_pending_work(&id.root_key, false).unwrap();
    await_release_after_drain(&mut registry, elapsed, &id.root_key).await;
    assert!(!runtime.has_active_resources());
}

#[test]
fn leftover_index_state_never_keeps_an_idle_daemon_alive() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function queued() {}\n").unwrap();
    let id = identity(&checkout);
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    let store = trellis::store::Store::open(roots.clone(), identity(&checkout)).unwrap();
    let accepted = store
        .enqueue_request(&trellis::indexer::IndexOptions::new(checkout), None)
        .unwrap();
    let dir = roots.index_db(&id).parent().unwrap().to_owned();
    let queue = roots.requests_db(&id);
    let queue_bytes = fs::read(&queue).unwrap();
    let journal = dir.join("index.db-journal");
    fs::write(&journal, b"unfinished journal").unwrap();
    fs::create_dir(roots.cache.join("indexes/unknown-entry")).unwrap();
    let now = Instant::now();
    let mut registry = CheckoutRegistry::with_roots_at(roots.clone(), now);
    assert!(
        !registry
            .advance(now + DAEMON_IDLE_DELAY - Duration::from_nanos(1))
            .unwrap()
            .exit
    );
    assert!(registry.advance(now + DAEMON_IDLE_DELAY).unwrap().exit);
    assert_eq!(fs::read(&queue).unwrap(), queue_bytes);
    assert_eq!(fs::read(&journal).unwrap(), b"unfinished journal");
    assert_eq!(
        store.request_by_id(&accepted.id).unwrap().unwrap().state,
        "queued"
    );
    // Unreadable queue bytes in a never-activated checkout are not an exit veto.
    drop(store);
    let before = b"not a queue";
    // Keep the real accepted queue untouched; use a separate leftover directory.
    let stray = roots.cache.join("indexes/unknown-entry/requests.db");
    fs::write(&stray, before).unwrap();
    let mut registry = CheckoutRegistry::with_roots_at(roots, now);
    assert!(registry.advance(now + DAEMON_IDLE_DELAY).unwrap().exit);
    assert_eq!(fs::read(stray).unwrap(), before);
}

#[cfg(unix)]
fn open_descriptors(path: &Path) -> usize {
    use std::os::unix::fs::MetadataExt;
    let expected = fs::metadata(path).unwrap();
    (0..1024)
        .filter(|fd| {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe { libc::fstat(*fd, stat.as_mut_ptr()) } != 0 {
                return false;
            }
            let stat = unsafe { stat.assume_init() };
            #[cfg(target_os = "macos")]
            let dev = stat.st_dev as u64;
            #[cfg(not(target_os = "macos"))]
            let dev = stat.st_dev;
            (dev, stat.st_ino) == (expected.dev(), expected.ino())
        })
        .count()
}

#[tokio::test]
async fn timed_release_closes_checkout_handles_and_reattach_runs_startup() {
    let (base, id, mut registry, now) = fixture();
    let root_baseline = open_descriptors(&id.root);
    let lock = base
        .path()
        .join("cache/indexes")
        .join(format!("{}.lock", id.root_key));
    registry.attach_launch(1, &id).unwrap();
    let old = registry.activate(&id.root_key).unwrap();
    ready(&old).await;
    assert!(open_descriptors(&id.root) > root_baseline);
    assert!(open_descriptors(&lock) > 0);
    registry.disconnect_at(1, now);
    await_idle_release(&mut registry, now + CHECKOUT_RELEASE_DELAY, &id.root_key).await;
    assert!(!old.has_active_resources());
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while open_descriptors(&id.root) != root_baseline || open_descriptors(&lock) != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    fs::write(id.root.join("a.js"), "function changed() {}\n").unwrap();
    registry.attach_launch(2, &id).unwrap();
    let resumed = registry.activate(&id.root_key).unwrap();
    assert!(resumed.catching_up());
    ready(&resumed).await;
    assert!(open_descriptors(&lock) > 0);
    let (response, stale) = resumed.evidence_response().unwrap();
    assert!(!stale);
    let (_, source) = response.source_at("a.js", None).unwrap().unwrap();
    assert!(source.text.contains("changed"));
    response.finish(()).unwrap();
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn invalid_prior_head_does_not_pin_idle_resources_or_skip_reattach_startup() {
    use std::os::unix::fs::MetadataExt;
    let (_base, id, mut registry, now) = fixture();
    registry.attach_launch(1, &id).unwrap();
    let old = registry.activate(&id.root_key).unwrap();
    ready(&old).await;
    let queue = _base
        .path()
        .join("cache/indexes")
        .join(&id.root_key)
        .join("requests.db");
    let queue_inode = fs::metadata(&queue).unwrap().ino();
    registry.disconnect_at(1, now);
    await_idle_release(&mut registry, now + CHECKOUT_RELEASE_DELAY, &id.root_key).await;
    assert!(!old.has_active_resources());
    assert!(registry.advance(now + DAEMON_IDLE_DELAY).unwrap().exit);
    // The released daemon owns no live SQLite handle. Losing its disposable
    // index.db requires a new startup reconciliation, with no validated predecessor to select.
    fs::remove_file(
        _base
            .path()
            .join("cache/indexes")
            .join(&id.root_key)
            .join("index.db"),
    )
    .unwrap();
    registry.attach_launch(2, &id).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx.lock().unwrap().recv().unwrap();
    }));
    let resumed = registry.activate(&id.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while entered_rx.try_recv().is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("mandatory startup reconciliation did not reach pause");
    assert!(resumed.catching_up());
    // Err carries no CheckoutEvidence, so there is no admitted basis to report.
    let error = match resumed.evidence_response() {
        Ok(_) => panic!("a head was admitted without the prior-head permit"),
        Err(error) => error,
    };
    assert!(
        error
            .downcast_ref::<trellis::store::topology::IndexNotReady>()
            .is_some(),
        "{error:#}"
    );
    resume_tx.send(()).unwrap();
    let recovered = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((response, catching_up)) = resumed.evidence_response() {
                let finished = response.finish(()).is_ok();
                if finished && !catching_up {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if recovered.is_err() {
        let diagnostic = resumed
            .evidence_response()
            .err()
            .map(|error| format!("{error:#}"));
        panic!(
            "H did not recover invalid selected head: catching_up={}, retry_waiting={}, reconciliation_error={:?}, admission={diagnostic:?}",
            resumed.catching_up(),
            resumed.retry_waiting_for_tests(),
            resumed.reconciliation_error()
        );
    }
    let (response, catching_up) = resumed.evidence_response().unwrap();
    assert!(!catching_up);
    response.finish(()).unwrap();
    assert_eq!(
        fs::metadata(&queue).unwrap().ino(),
        queue_inode,
        "H recovery must retain the durable queue inode"
    );
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn active_writer_and_protected_reader_keep_activated_checkout_pending() {
    use trellis::store::MaintenanceQueueState;

    let (base, id, mut registry, now) = fixture();
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    let store = trellis::store::Store::open(roots.clone(), identity(&id.root)).unwrap();
    let options = trellis::indexer::IndexOptions::new(id.root.clone());
    let first = store.enqueue_request(&options, None).unwrap();
    assert_eq!(
        store.request_by_id(&first.id).unwrap().unwrap().state,
        "queued"
    );
    let reader = store.open_maintenance_queue_probe().unwrap();
    assert!(matches!(
        reader,
        trellis::store::QueueProbeAdmission::Ready(_)
    ));
    assert_eq!(reader.check(), MaintenanceQueueState::Pending);

    registry.attach_launch(1, &id).unwrap();
    // startup reconciliation must not claim the accepted row before the contention scan. The
    // hook is installed before activation starts the worker.
    let (h_entered_tx, h_entered_rx) = std::sync::mpsc::sync_channel(1);
    let (h_resume_tx, h_resume_rx) = std::sync::mpsc::sync_channel(1);
    let h_resume_rx = std::sync::Mutex::new(h_resume_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        h_entered_tx.send(()).unwrap();
        h_resume_rx.lock().unwrap().recv().unwrap();
    }));
    let runtime = registry.activate(&id.root_key).unwrap();
    // Claims start only after startup reconciliation, which is paused here.
    let tracked = std::sync::Arc::new(std::sync::Mutex::new(vec![first.id.clone()]));
    let claims = ClaimLog::default();
    record_claims(
        runtime.clone(),
        store.clone(),
        tracked.clone(),
        claims.clone(),
    );
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while h_entered_rx.try_recv().is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("mandatory startup reconciliation did not reach pause");
    assert_eq!(
        store.request_by_id(&first.id).unwrap().unwrap().state,
        "queued"
    );

    let (writer_entered_tx, writer_entered_rx) = std::sync::mpsc::sync_channel(1);
    let (writer_resume_tx, writer_resume_rx) = std::sync::mpsc::sync_channel(1);
    let writer_store = store.clone();
    let writer = std::thread::spawn(move || {
        writer_store.enqueue_request_after_insert_for_tests(&options, None, || {
            writer_entered_tx.send(()).unwrap();
            writer_resume_rx.recv().unwrap();
        })
    });
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while writer_entered_rx.try_recv().is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("protected queue writer did not enter its transaction");
    // The retained reader observes the rollback journal of the live writer,
    // rather than a writer thread that is merely waiting after its commit.
    assert_eq!(reader.check(), MaintenanceQueueState::Unknown);
    registry.disconnect_at(1, now);
    // The held startup attempt and accepted FIFO rows are active work. The
    // lifecycle observes their owner's publication, not a filesystem scan.
    let tick = registry.advance(now + DAEMON_IDLE_DELAY).unwrap();
    assert!(tick.released.is_empty());
    assert!(!tick.exit);
    assert!(runtime.has_active_resources());
    assert_eq!(reader.check(), MaintenanceQueueState::Unknown);

    writer_resume_tx.send(()).unwrap();
    let second = tokio::task::spawn_blocking(move || writer.join().unwrap().unwrap())
        .await
        .unwrap();
    assert!(first.seq < second.seq);
    tracked.lock().unwrap().push(second.id.clone());
    assert_eq!(
        store.request_by_id(&first.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(
        store.request_by_id(&second.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(
        store.earliest_unfinished_request().unwrap().unwrap().id,
        first.id
    );
    drop(reader);
    h_resume_tx.send(()).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let a = store.request_by_id(&first.id).unwrap().unwrap();
            let b = store.request_by_id(&second.id).unwrap().unwrap();
            if a.finished_at.is_some() && b.finished_at.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(store.earliest_unfinished_request().unwrap().is_none());
    let first_done = store.request_by_id(&first.id).unwrap().unwrap();
    let second_done = store.request_by_id(&second.id).unwrap().unwrap();
    assert_eq!(first_done.state, "done");
    assert_eq!(second_done.state, "done");
    assert_eq!(
        first_done.revision.unwrap().index_generation,
        second_done.revision.unwrap().index_generation
    );
    // Both no-op rows complete at the unchanged head (Decision 0006 §3).
    assert_eq!(first_done.revision, second_done.revision);
    // FIFO: the first claim finds the second row queued; the second claim
    // finds the first row already terminal.
    assert_eq!(
        *claims.lock().unwrap(),
        vec![
            vec!["running".to_owned(), "queued".to_owned()],
            vec!["done".to_owned(), "running".to_owned()],
        ],
        "claim-boundary snapshots"
    );
    await_release_after_drain(&mut registry, now + DAEMON_IDLE_DELAY, &id.root_key).await;
    assert!(!runtime.has_active_resources());
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn committed_fifo_queue_keeps_both_expired_clocks_until_normal_daemon_drain() {
    let (base, id, mut registry, now) = fixture();
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    let store = trellis::store::Store::open(roots, identity(&id.root)).unwrap();
    let options = trellis::indexer::IndexOptions::new(id.root.clone());
    // The live worker claims the first request as soon as it is committed:
    // hold it there, so the committed queue is observed before it drains.
    let (claim_tx, claim_rx) = std::sync::mpsc::sync_channel(1);
    let (continue_tx, continue_rx) = std::sync::mpsc::sync_channel(1);
    let continue_rx = std::sync::Mutex::new(continue_rx);
    // After the held first claim, record the queue at every later claim.
    let tracked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let claims = ClaimLog::default();
    let (recorder_runtime, recorder_store, recorder_tracked, recorder_claims) = (
        runtime.clone(),
        store.clone(),
        tracked.clone(),
        claims.clone(),
    );
    runtime.set_after_claim_hook_for_tests(move || {
        claim_tx.send(()).unwrap();
        continue_rx.lock().unwrap().recv().unwrap();
        record_claims(
            recorder_runtime,
            recorder_store,
            recorder_tracked,
            recorder_claims,
        );
    });
    let first = store.enqueue_request(&options, None).unwrap();
    let second = store.enqueue_request(&options, None).unwrap();
    assert!(first.seq < second.seq);
    tracked
        .lock()
        .unwrap()
        .extend([first.id.clone(), second.id.clone()]);
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while claim_rx.try_recv().is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first FIFO claim did not reach barrier");
    assert_eq!(
        store.earliest_unfinished_request().unwrap().unwrap().id,
        first.id
    );
    assert_eq!(
        store.request_by_id(&second.id).unwrap().unwrap().state,
        "queued"
    );
    registry.disconnect_at(1, now);
    let deadline = now + DAEMON_IDLE_DELAY;
    let busy = registry.advance(deadline).unwrap();
    assert!(busy.released.is_empty());
    assert!(!busy.exit);
    assert!(runtime.has_active_resources());
    continue_tx.send(()).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let a = store.request_by_id(&first.id).unwrap().unwrap();
            let b = store.request_by_id(&second.id).unwrap().unwrap();
            if a.finished_at.is_some() && b.finished_at.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(store.earliest_unfinished_request().unwrap().is_none());
    let first_done = store.request_by_id(&first.id).unwrap().unwrap();
    let second_done = store.request_by_id(&second.id).unwrap().unwrap();
    assert_eq!(first_done.state, "done");
    assert_eq!(second_done.state, "done");
    let first_revision = first_done.revision.unwrap();
    let second_revision = second_done.revision.unwrap();
    assert_eq!(
        first_revision.index_generation,
        second_revision.index_generation
    );
    // Both no-op rows complete at the unchanged head (Decision 0006 §3).
    assert_eq!(first_revision, second_revision);
    // FIFO: the second row is claimed only once the first is terminal.
    assert_eq!(
        *claims.lock().unwrap(),
        vec![vec!["done".to_owned(), "running".to_owned()]],
        "claim-boundary snapshots"
    );
    await_release_after_drain(&mut registry, deadline, &id.root_key).await;
    assert!(!runtime.has_active_resources());
}

#[test]
fn deleted_or_replaced_orphan_root_with_empty_queue_allows_exit() {
    for replace in [false, true] {
        let base = tempfile::tempdir().unwrap();
        let checkout = base.path().join("work");
        fs::create_dir(&checkout).unwrap();
        let id = identity(&checkout);
        let roots =
            TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
        drop(trellis::store::Store::open(roots.clone(), id).unwrap());
        fs::remove_dir_all(&checkout).unwrap();
        if replace {
            fs::create_dir(&checkout).unwrap();
        }
        let now = Instant::now();
        let mut registry = CheckoutRegistry::with_roots_at(roots, now);
        assert!(
            registry.advance(now + DAEMON_IDLE_DELAY).unwrap().exit,
            "orphan whose root is gone or replaced cannot keep daemon alive"
        );
    }
}

#[test]
fn replaced_path_retires_only_quiescent_old_checkout() {
    let (base, old, mut registry, now) = fixture();
    registry.attach_launch(1, &old).unwrap();
    let moved = base.path().join("old");
    fs::rename(&old.root, &moved).unwrap();
    fs::create_dir(&old.root).unwrap();
    let next = identity(&old.root);
    assert_ne!(old.inode, next.inode);
    assert!(
        registry.attach_launch(2, &next).is_err(),
        "live old client still holds the original checkout"
    );
    registry.disconnect_at(1, now);
    registry
        .attach_launch(2, &next)
        .expect("quiescent checkout can be replaced");
    registry.disconnect_at(2, now);
    assert!(registry.release(&next.root_key).unwrap());
}

#[test]
fn git_init_retires_quiescent_non_git_identity_at_same_path() {
    let (_base, old, mut registry, now) = fixture();
    registry.attach_launch(1, &old).unwrap();
    let created = std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .arg(&old.root)
        .status()
        .unwrap();
    assert!(created.success());
    let next = identity(&old.root);
    assert_ne!(old.record_id, next.record_id);
    assert!(registry.attach_launch(2, &next).is_err());
    registry.disconnect_at(1, now);
    registry
        .attach_launch(2, &next)
        .expect("git-init changes the checkout identity");
}

#[test]
fn replaced_path_refuses_live_browser_or_pending_work() {
    let (base, old, mut registry, now) = fixture();
    registry.browser_request_at(&old, now).unwrap();
    let moved = base.path().join("old");
    fs::rename(&old.root, moved).unwrap();
    fs::create_dir(&old.root).unwrap();
    let next = identity(&old.root);
    assert!(
        registry
            .can_register(&next, &CheckoutOptions(serde_json::json!({})))
            .is_err()
    );
    assert!(registry.attach_launch(1, &next).is_err());
    registry.advance(now + BROWSER_IDLE_DELAY).unwrap();
    registry.set_pending_work(&old.root_key, true).unwrap();
    assert!(
        registry.attach_launch(1, &next).is_err(),
        "pending old work stays attributed"
    );
    registry.set_pending_work(&old.root_key, false).unwrap();
    let options = CheckoutOptions(serde_json::json!({"maxFileBytes": 4096}));
    registry.can_register(&next, &options).unwrap();
    registry.register(&next, options).unwrap();
    registry.attach_launch(1, &next).unwrap();
    assert_eq!(
        registry.browser_identity(&next.root_key).unwrap().record_id,
        next.record_id
    );
}
