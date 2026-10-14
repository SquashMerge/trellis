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
fn startup_orphan_queue_blocks_idle_exit_without_any_client() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function queued() {}\n").unwrap();
    let id = identity(&checkout);
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    let store = trellis::store::Store::open(roots.clone(), id).unwrap();
    store
        .enqueue_request(&trellis::indexer::IndexOptions::new(checkout), None)
        .unwrap();
    drop(store);
    let now = Instant::now();
    let mut restarted = CheckoutRegistry::with_roots_at(roots, now);
    assert!(!restarted.advance(now + DAEMON_IDLE_DELAY).unwrap().exit);
}

#[test]
fn empty_interrupted_index_exits_but_unknown_queue_does_not() {
    let (base, id, _, now) = fixture();
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    let store = trellis::store::Store::open(roots.clone(), identity(&id.root)).unwrap();
    let dir = base.path().join("cache/indexes").join(&id.root_key);
    drop(store);
    fs::remove_file(dir.join("index.db")).unwrap();
    let queue = dir.join("requests.db");
    if queue.exists() {
        fs::remove_file(&queue).unwrap();
    }
    let mut registry = CheckoutRegistry::with_roots_at(roots.clone(), now);
    assert!(registry.advance(now + DAEMON_IDLE_DELAY).unwrap().exit);
    fs::write(&queue, b"not a queue").unwrap();
    let mut registry = CheckoutRegistry::with_roots_at(roots, now);
    assert!(!registry.advance(now + DAEMON_IDLE_DELAY).unwrap().exit);
    assert_eq!(fs::read(&queue).unwrap(), b"not a queue");
    fs::remove_file(&queue).unwrap();
    let sidecar = dir.join("requests.db-wal");
    fs::write(&sidecar, b"unfinished queue").unwrap();
    let mut registry = CheckoutRegistry::with_roots_at(
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data")),
        now,
    );
    assert!(!registry.advance(now + DAEMON_IDLE_DELAY).unwrap().exit);
    assert_eq!(fs::read(&sidecar).unwrap(), b"unfinished queue");
}

#[test]
fn orphan_scan_refuses_hot_index_sidecars_and_wal_header_without_creation() {
    use std::os::unix::fs::FileExt;
    let (base, id, _, now) = fixture();
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    let store = trellis::store::Store::open(roots.clone(), identity(&id.root)).unwrap();
    drop(store);
    let dir = base.path().join("cache/indexes").join(&id.root_key);
    let index = dir.join("index.db");
    let names = || {
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    for suffix in ["-journal", "-wal"] {
        let sidecar = dir.join(format!("index.db{suffix}"));
        fs::write(&sidecar, b"unfinished journal").unwrap();
        let before = names();
        let mut registry = CheckoutRegistry::with_roots_at(roots.clone(), now);
        assert!(!registry.advance(now + DAEMON_IDLE_DELAY).unwrap().exit);
        assert_eq!(names(), before);
        assert_eq!(fs::read(&sidecar).unwrap(), b"unfinished journal");
        fs::remove_file(sidecar).unwrap();
    }
    let file = fs::OpenOptions::new().write(true).open(&index).unwrap();
    file.write_at(&[2, 2], 18).unwrap();
    drop(file);
    let before = names();
    let mut registry = CheckoutRegistry::with_roots_at(roots, now);
    assert!(!registry.advance(now + DAEMON_IDLE_DELAY).unwrap().exit);
    assert_eq!(names(), before);
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
    let resumed = registry.activate(&id.root_key).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    resumed.set_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx.lock().unwrap().recv().unwrap();
    }));
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
async fn active_writer_and_protected_reader_overlap_orphan_idle_scan() {
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
    let runtime = registry.activate(&id.root_key).unwrap();
    // startup reconciliation must not claim the accepted row before the contention scan. Install
    // the hook before yielding to the spawned reconciliation worker.
    let (h_entered_tx, h_entered_rx) = std::sync::mpsc::sync_channel(1);
    let (h_resume_tx, h_resume_rx) = std::sync::mpsc::sync_channel(1);
    let h_resume_rx = std::sync::Mutex::new(h_resume_rx);
    runtime.set_startup_hook_for_tests(std::sync::Arc::new(move || {
        h_entered_tx.send(()).unwrap();
        h_resume_rx.lock().unwrap().recv().unwrap();
    }));
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
    let active = std::iter::once(id.root_key.clone()).collect();
    registry.disconnect_at(1, now);
    // SQLite busy retries must run off the Tokio scheduler while the writer
    // waits on its channel. The scanner sees both the active and orphan views
    // of the SAME root before the transaction is allowed to commit.
    let (active_pending, orphan_pending, tick, returned_registry) =
        tokio::task::spawn_blocking(move || {
            let active_pending = trellis::store::Store::orphan_queues_pending(&roots, &active);
            let orphan_pending =
                trellis::store::Store::orphan_queues_pending(&roots, &Default::default());
            let tick = registry.advance(now + DAEMON_IDLE_DELAY).unwrap();
            (active_pending, orphan_pending, tick, registry)
        })
        .await
        .unwrap();
    registry = returned_registry;
    assert!(
        !active_pending,
        "an active root must not enter the orphan scanner"
    );
    assert!(
        orphan_pending,
        "the live writer and accepted row are not clear orphan work"
    );
    assert!(tick.released.is_empty());
    assert!(!tick.exit);
    assert!(runtime.has_active_resources());
    assert_eq!(reader.check(), MaintenanceQueueState::Unknown);

    writer_resume_tx.send(()).unwrap();
    let second = tokio::task::spawn_blocking(move || writer.join().unwrap().unwrap())
        .await
        .unwrap();
    assert!(first.seq < second.seq);
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
    assert!(
        first_done.revision.unwrap().index_revision < second_done.revision.unwrap().index_revision
    );
    await_release_after_drain(&mut registry, now + DAEMON_IDLE_DELAY, &id.root_key).await;
    assert!(!runtime.has_active_resources());
}

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
    let first = store.enqueue_request(&options, None).unwrap();
    let second = store.enqueue_request(&options, None).unwrap();
    assert!(first.seq < second.seq);
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
    assert!(first_revision.index_revision < second_revision.index_revision);
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

#[test]
fn pending_orphan_scans_are_not_repeated_every_tick() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    let id = identity(&checkout);
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    let store = trellis::store::Store::open(roots.clone(), id).unwrap();
    store
        .enqueue_request(&trellis::indexer::IndexOptions::new(checkout), None)
        .unwrap();
    drop(store);
    let now = Instant::now();
    let mut registry = CheckoutRegistry::with_roots_at(roots, now);
    assert_eq!(registry.orphan_scan_count_for_tests(), 0);
    registry
        .advance(now + DAEMON_IDLE_DELAY - Duration::from_nanos(1))
        .unwrap();
    assert_eq!(registry.orphan_scan_count_for_tests(), 0);
    assert!(!registry.advance(now + DAEMON_IDLE_DELAY).unwrap().exit);
    assert_eq!(registry.orphan_scan_count_for_tests(), 1);
    assert!(
        !registry
            .advance(now + DAEMON_IDLE_DELAY + Duration::from_millis(250))
            .unwrap()
            .exit
    );
    assert_eq!(registry.orphan_scan_count_for_tests(), 1);
    assert!(
        !registry
            .advance(now + DAEMON_IDLE_DELAY + Duration::from_secs(5))
            .unwrap()
            .exit
    );
    assert_eq!(registry.orphan_scan_count_for_tests(), 2);
}

#[test]
fn orphan_published_non_git_root_that_became_git_does_not_pin_exit() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(
        checkout.join("a.js"),
        "function beforeGit() {}
",
    )
    .unwrap();
    let old = identity(&checkout);
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    let store = trellis::store::Store::open(roots.clone(), old).unwrap();
    let owner = trellis::index_coordinator::establish_serving_session(
        &store,
        Some(&trellis::indexer::IndexOptions::new(checkout.clone())),
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    drop(owner);
    drop(store);
    assert!(
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(&checkout)
            .status()
            .unwrap()
            .success()
    );
    identity(&checkout);
    let now = Instant::now();
    let mut registry = CheckoutRegistry::with_roots_at(roots, now);
    assert!(registry.advance(now + DAEMON_IDLE_DELAY).unwrap().exit);
}

#[test]
fn orphan_ambiguous_same_inode_git_metadata_stays_busy() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    let old = identity(&checkout);
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    drop(trellis::store::Store::open(roots.clone(), old).unwrap());
    fs::write(
        checkout.join(".git"),
        "invalid pointer
",
    )
    .unwrap();
    let now = Instant::now();
    let mut registry = CheckoutRegistry::with_roots_at(roots, now);
    assert!(
        !registry.advance(now + DAEMON_IDLE_DELAY).unwrap().exit,
        "unreadable same-inode Git identity is not proof the queue is abandoned"
    );
}
