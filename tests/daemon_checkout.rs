use std::{fs, path::Path, time::Duration};
use trellis::{
    daemon::registry::{CheckoutOptions, CheckoutRegistry},
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

fn identity(root: &Path) -> WorkspaceIdentity {
    WorkspaceIdentity::discover(Some(root), root)
        .unwrap()
        .attach_marker()
        .unwrap()
}
fn roots(state: &Path) -> TopologyRoots {
    TopologyRoots::isolated_for_tests(state.join("cache"), state.join("data"))
}
async fn ready(runtime: &trellis::daemon::registry::CheckoutRuntime) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok((response, catching_up)) = runtime.evidence_response() {
                let finished = response.finish(()).is_ok();
                if finished && !catching_up {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("H did not finish: {:?}", runtime.reconciliation_error()));
}

async fn release_when_idle(registry: &mut CheckoutRegistry, key: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !registry.release(key).unwrap() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("checkout did not release after H and watcher quiescence");
}

#[tokio::test]
async fn registration_does_not_open_a_store_and_active_checkouts_are_isolated() {
    let base = tempfile::tempdir().unwrap();
    let a = base.path().join("a");
    let b = base.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(
        a.join("a.js"),
        "function checkoutA() {}
",
    )
    .unwrap();
    fs::write(
        b.join("b.js"),
        "function checkoutB() {}
",
    )
    .unwrap();
    let first = identity(&a);
    let second = identity(&b);
    let topology = roots(base.path());
    let mut registry = CheckoutRegistry::with_roots(topology.clone());
    registry
        .register(&first, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    registry
        .register(&second, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    assert!(!topology.index_dir(&first).exists());
    assert!(!topology.index_dir(&second).exists());
    registry.attach_launch(1, &first).unwrap();
    let active = registry.activate(&first.root_key).unwrap();
    assert!(topology.index_dir(&first).exists());
    assert!(!topology.index_dir(&second).exists());
    ready(&active).await;
    registry.attach_launch(2, &second).unwrap();
    let other = registry.activate(&second.root_key).unwrap();
    ready(&other).await;
    let (response, _) = active.evidence_response().unwrap();
    assert_eq!(
        response.status().unwrap().workspace_root,
        fs::canonicalize(&a).unwrap().to_string_lossy()
    );
    response.finish(()).unwrap();
    let (response, _) = other.evidence_response().unwrap();
    assert_eq!(
        response.status().unwrap().workspace_root,
        fs::canonicalize(&b).unwrap().to_string_lossy()
    );
    response.finish(()).unwrap();
}

#[tokio::test]
async fn external_standalone_leader_keeps_daemon_a_follower() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(
        checkout.join("a.js"),
        "function oldHead() {}
",
    )
    .unwrap();
    let id = identity(&checkout);
    let topology = roots(base.path());
    let standalone = Store::open(topology.clone(), identity(&checkout)).unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let leader = trellis::index_coordinator::establish_serving_session(
        &standalone,
        Some(&trellis::indexer::IndexOptions::new(checkout.clone())),
        &cancel,
    )
    .unwrap();
    assert!(leader.is_leader());
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    assert!(leader.verify().is_ok());
    assert!(runtime.is_follower());
}

#[tokio::test]
async fn reattach_reports_catching_up_and_keeps_complete_head_until_h() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(
        checkout.join("a.js"),
        "function oldHead() {}
",
    )
    .unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    assert!(runtime.catching_up(), "mandatory H has not run yet");
    ready(&runtime).await;
    let (old, stale) = runtime.evidence_response().unwrap();
    assert!(!stale);
    let first = old.status().unwrap().revision;
    old.finish(()).unwrap();
    drop(old);
    registry.disconnect(1);
    release_when_idle(&mut registry, &id.root_key).await;
    fs::write(
        checkout.join("a.js"),
        "function newHead() {}
",
    )
    .unwrap();
    registry.attach_launch(2, &id).unwrap();
    let resumed = registry.activate(&id.root_key).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    resumed.set_pre_h_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx.lock().unwrap().recv().unwrap();
    }));
    tokio::time::timeout(Duration::from_secs(5), async {
        while entered_rx.try_recv().is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pre-H worker did not reach pause");
    assert!(resumed.catching_up());
    let (prior, catching_up) = resumed
        .evidence_response()
        .expect("validated old head while H paused");
    assert!(catching_up);
    assert_eq!(prior.status().unwrap().revision, first);
    let (_, old_source) = prior.source_at("a.js", None).unwrap().unwrap();
    assert!(old_source.text.contains("oldHead"));
    prior.finish(()).unwrap();
    drop(prior);
    resume_tx.send(()).unwrap();
    ready(&resumed).await;
    let (current, catching_up) = resumed.evidence_response().unwrap();
    assert!(!catching_up);
    let new_revision = current.status().unwrap().revision;
    assert!(new_revision.index_revision > first.index_revision);
    current.finish(()).unwrap();
}

#[tokio::test]
async fn committed_queue_survives_runtime_restart() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(
        checkout.join("a.js"),
        "function queued() {}
",
    )
    .unwrap();
    let id = identity(&checkout);
    let topology = roots(base.path());
    let store = Store::open(topology.clone(), identity(&checkout)).unwrap();
    let options = trellis::indexer::IndexOptions::new(checkout.clone());
    let accepted = store.enqueue_request(&options, None).unwrap();
    drop(store);
    let mut restarted = CheckoutRegistry::with_roots(topology.clone());
    restarted.attach_launch(1, &id).unwrap();
    let _runtime = restarted.activate(&id.root_key).unwrap();
    let observed = Store::open(topology, identity(&checkout)).unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if observed
                .request_by_id(&accepted.id)
                .unwrap()
                .unwrap()
                .finished_at
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        observed.request_by_id(&accepted.id).unwrap().unwrap().state,
        "done"
    );
}

#[cfg(unix)]
fn file_descriptors(path: &Path) -> usize {
    use std::os::unix::fs::MetadataExt;
    let expected = fs::metadata(path).unwrap();
    (0..1024)
        .filter(|fd| {
            let mut held = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe { libc::fstat(*fd, held.as_mut_ptr()) } != 0 {
                return false;
            }
            let held = unsafe { held.assume_init() };
            #[cfg(target_os = "macos")]
            let device = held.st_dev as u64;
            #[cfg(not(target_os = "macos"))]
            let device = held.st_dev;
            (device, held.st_ino) == (expected.dev(), expected.ino())
        })
        .count()
}

#[tokio::test]
async fn retained_registration_and_old_runtime_handle_hold_no_checkout_descriptors() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function beforeRelease() {}\n").unwrap();
    let baseline = file_descriptors(&checkout);
    let id = identity(&checkout);
    let key = id.root_key.clone();
    let topology = roots(base.path());
    let lock_path = topology.leader_lock(&id);
    let index_db = topology.index_db(&id);
    let requests_db = topology.requests_db(&id);
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry
        .register(&id, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    drop(id);
    assert_eq!(
        file_descriptors(&checkout),
        baseline,
        "registration must retain metadata only"
    );
    let id = identity(&checkout);
    registry.attach_launch(1, &id).unwrap();
    drop(id);
    let old = registry.activate(&key).unwrap();
    ready(&old).await;
    assert!(old.has_active_resources());
    assert!(file_descriptors(&checkout) > baseline);
    assert!(
        file_descriptors(&lock_path) > 0,
        "active leader holds checkout lock"
    );
    assert!(file_descriptors(&index_db) > 0);
    assert!(file_descriptors(&requests_db) > 0);
    registry.disconnect(1);
    release_when_idle(&mut registry, &key).await;
    assert!(!old.has_active_resources());
    assert!(
        old.evidence_response().is_err(),
        "released handles cannot answer"
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while file_descriptors(&checkout) != baseline
            || file_descriptors(&lock_path) != 0
            || file_descriptors(&index_db) != 0
            || file_descriptors(&requests_db) != 0
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("released runtime, permit and registration must close root handles");
    fs::write(checkout.join("a.js"), "function afterRelease() {}\n").unwrap();
    let id = identity(&checkout);
    registry.attach_launch(2, &id).unwrap();
    drop(id);
    let resumed = registry.activate(&key).unwrap();
    ready(&resumed).await;
    assert!(!old.has_active_resources());
    let (response, catching_up) = resumed.evidence_response().unwrap();
    assert!(!catching_up);
    response.finish(()).unwrap();
}

#[tokio::test]
async fn queued_follower_is_pending_even_after_client_disconnect() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function pendingWork() {}\n").unwrap();
    let id = identity(&checkout);
    let topology = roots(base.path());
    let standalone = Store::open(topology.clone(), identity(&checkout)).unwrap();
    let options = trellis::indexer::IndexOptions::new(checkout.clone());
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let leader =
        trellis::index_coordinator::establish_serving_session(&standalone, Some(&options), &cancel)
            .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    assert!(runtime.is_follower());
    let row = standalone.enqueue_request(&options, None).unwrap();
    assert_eq!(
        standalone.request_by_id(&row.id).unwrap().unwrap().state,
        "queued"
    );
    assert!(registry.refresh_pending_work(&id.root_key).unwrap());
    registry.disconnect(1);
    assert!(!registry.release(&id.root_key).unwrap());
    assert!(leader.verify().is_ok());
}

#[tokio::test]
async fn follower_takeover_marks_h_pending_and_next_reattach_admits_old_head() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function firstHead() {}\n").unwrap();
    let id = identity(&checkout);
    let topology = roots(base.path());
    let standalone = Store::open(topology.clone(), identity(&checkout)).unwrap();
    let options = trellis::indexer::IndexOptions::new(checkout.clone());
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let leader =
        trellis::index_coordinator::establish_serving_session(&standalone, Some(&options), &cancel)
            .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    assert!(runtime.is_follower());
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    runtime.set_takeover_h_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx.lock().unwrap().recv().unwrap();
    }));
    fs::write(checkout.join("a.js"), "function takeoverHead() {}\n").unwrap();
    let accepted = standalone.enqueue_request(&options, None).unwrap();
    drop(leader);
    tokio::time::timeout(Duration::from_secs(10), async {
        while entered_rx.try_recv().is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon did not elect after standalone released");
    assert!(
        runtime.catching_up(),
        "the newly elected owner has not committed H"
    );
    assert!(
        !runtime.is_follower(),
        "invalid predecessor follower must not remain ready"
    );
    assert_eq!(
        standalone
            .request_by_id(&accepted.id)
            .unwrap()
            .unwrap()
            .state,
        "queued"
    );
    resume_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while runtime.is_follower()
            || runtime.catching_up()
            || standalone
                .request_by_id(&accepted.id)
                .unwrap()
                .unwrap()
                .finished_at
                .is_none()
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("new daemon leader did not finish H and FIFO");
    let (current, flag) = runtime.evidence_response().unwrap();
    assert!(!flag);
    let established = current.status().unwrap().revision;
    current.finish(()).unwrap();
    drop(current);
    registry.disconnect(1);
    release_when_idle(&mut registry, &id.root_key).await;
    fs::write(checkout.join("a.js"), "function nextHead() {}\n").unwrap();
    registry.attach_launch(2, &id).unwrap();
    let next = registry.activate(&id.root_key).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    next.set_pre_h_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx.lock().unwrap().recv().unwrap();
    }));
    tokio::time::timeout(Duration::from_secs(5), async {
        while entered_rx.try_recv().is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("new daemon leader did not mint a reattach permit");
    let (prior, flag) = next
        .evidence_response()
        .expect("takeover leader must mint pre-H permit");
    assert!(flag);
    assert_eq!(prior.status().unwrap().revision, established);
    prior.finish(()).unwrap();
    drop(prior);
    resume_tx.send(()).unwrap();
    ready(&next).await;
    let (updated, flag) = next.evidence_response().unwrap();
    assert!(!flag);
    assert!(updated.status().unwrap().revision.index_revision > established.index_revision);
    updated.finish(()).unwrap();
}

#[tokio::test]
async fn root_loss_fails_accepted_work_then_releases_old_runtime_handles() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    let moved = base.path().join("moved");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function oldRoot() {}\n").unwrap();
    let id = identity(&checkout);
    let key = id.root_key.clone();
    let topology = roots(base.path());
    let db_path = topology.requests_db(&id);
    let leader_path = topology.leader_lock(&id);
    let mut registry = CheckoutRegistry::with_roots(topology.clone());
    registry.attach_launch(1, &id).unwrap();
    drop(id);
    let old = registry.activate(&key).unwrap();
    ready(&old).await;
    let store = Store::open(topology, identity(&checkout)).unwrap();
    let row = store
        .enqueue_request(&trellis::indexer::IndexOptions::new(checkout.clone()), None)
        .unwrap();
    drop(store);
    fs::rename(&checkout, &moved).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function newRoot() {}\n").unwrap();
    let replacement = identity(&checkout);
    registry.disconnect(1);
    assert!(
        !registry.release(&key).unwrap(),
        "committed queued row cannot release before terminal root-loss result"
    );
    assert!(
        registry.browser_identity(&key).is_err(),
        "browser must not retire an old incarnation with accepted FIFO work"
    );
    assert_eq!(
        registry
            .browser_checkouts()
            .iter()
            .find(|row| row["rootKey"] == key)
            .unwrap()["state"],
        "unavailable",
        "accepted FIFO row prevents replacement listing"
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while registry.refresh_pending_work(&key).unwrap() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("old-root queue did not reach a terminal result");
    let db = rusqlite::Connection::open(db_path).unwrap();
    let (state, error): (String, String) = db
        .query_row(
            "SELECT state,error_code FROM requests WHERE id=?1",
            [&row.id],
            |record| Ok((record.get(0)?, record.get(1)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), error.as_str()), ("failed", "root_changed"));
    drop(db);
    release_when_idle(&mut registry, &key).await;
    assert_eq!(
        registry.browser_identity(&key).unwrap().inode,
        replacement.inode
    );
    assert!(!old.has_active_resources());
    assert!(old.evidence_response().is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while file_descriptors(&moved) != 0 || file_descriptors(&leader_path) != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("old runtime retained root/leader descriptors after release");
}

#[tokio::test]
async fn root_loss_during_paused_reattach_h_waits_for_worker_then_releases() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    let moved = base.path().join("moved");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function beforeRelease() {}\n").unwrap();
    let id = identity(&checkout);
    let key = id.root_key.clone();
    let topology = roots(base.path());
    let leader_path = topology.leader_lock(&id);
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(1, &id).unwrap();
    let original = registry.activate(&key).unwrap();
    ready(&original).await;
    registry.disconnect(1);
    release_when_idle(&mut registry, &key).await;
    registry.attach_launch(2, &id).unwrap();
    drop(id);
    let paused = registry.activate(&key).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    paused.set_pre_h_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx.lock().unwrap().recv().unwrap();
    }));
    tokio::time::timeout(Duration::from_secs(5), async {
        while entered_rx.try_recv().is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("idle reattach did not elect before H pause");
    fs::rename(&checkout, &moved).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function replacement() {}\n").unwrap();
    let replacement = identity(&checkout);
    registry.disconnect(2);
    assert!(
        registry.browser_identity(&key).is_err(),
        "paused H keeps old incarnation even after client disconnect"
    );
    assert_eq!(
        registry
            .browser_checkouts()
            .iter()
            .find(|row| row["rootKey"] == key)
            .unwrap()["state"],
        "unavailable",
        "paused H prevents replacement listing"
    );
    assert!(
        paused.catching_up(),
        "paused H is pending despite an empty old-root queue"
    );
    assert!(
        !registry.release(&key).unwrap(),
        "live H owner must block idle release"
    );
    assert!(paused.has_active_resources());
    assert!(file_descriptors(&moved) > 0);
    assert!(file_descriptors(&leader_path) > 0);
    resume_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !registry.release(&key).unwrap() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("root-lost H did not retire and release");
    assert!(!paused.has_active_resources());
    assert!(paused.evidence_response().is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while file_descriptors(&moved) != 0 || file_descriptors(&leader_path) != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("paused H leaked old-root or leader descriptors");
    assert_eq!(
        registry.browser_identity(&key).unwrap().inode,
        replacement.inode
    );
}

#[tokio::test]
async fn cold_activation_preserves_recorded_scip_option() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(
        checkout.join("a.js"),
        "function recordedScip() {}
",
    )
    .unwrap();
    let id = identity(&checkout);
    let topology = roots(base.path());
    let store = Store::open(topology.clone(), identity(&checkout)).unwrap();
    let mut options = trellis::indexer::IndexOptions::new(checkout.clone());
    options.scip_path = Some(checkout.join("recorded.scip"));
    options.max_file_bytes = 8192;
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let owner =
        trellis::index_coordinator::establish_serving_session(&store, Some(&options), &cancel)
            .unwrap();
    assert!(owner.is_leader());
    drop(owner);
    drop(store);
    let mut registry = CheckoutRegistry::with_roots(topology.clone());
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    let observed = Store::open(topology.clone(), identity(&checkout))
        .unwrap()
        .recorded_index_options()
        .unwrap()
        .unwrap();
    assert_eq!(observed.scip_path, options.scip_path);
    assert_eq!(observed.max_file_bytes, options.max_file_bytes);

    registry.disconnect(1);
    release_when_idle(&mut registry, &id.root_key).await;
    registry
        .register(
            &id,
            CheckoutOptions(serde_json::json!({"maxFileBytes":2097152})),
        )
        .unwrap();
    registry.attach_launch(2, &id).unwrap();
    let explicit_default = registry.activate(&id.root_key).unwrap();
    ready(&explicit_default).await;
    let default_selected = Store::open(topology.clone(), identity(&checkout))
        .unwrap()
        .recorded_index_options()
        .unwrap()
        .unwrap();
    assert_eq!(
        default_selected.max_file_bytes, 2_097_152,
        "explicit default is an instruction, not implicit fallback"
    );
    assert!(default_selected.scip_path.is_none());
    registry.disconnect(2);
    release_when_idle(&mut registry, &id.root_key).await;
    registry
        .register(
            &id,
            CheckoutOptions(serde_json::json!({"maxFileBytes":4096})),
        )
        .unwrap();
    registry.attach_launch(3, &id).unwrap();
    let explicit = registry.activate(&id.root_key).unwrap();
    ready(&explicit).await;
    let selected = Store::open(topology, identity(&checkout))
        .unwrap()
        .recorded_index_options()
        .unwrap()
        .unwrap();
    assert_eq!(selected.max_file_bytes, 4096);
    assert!(
        selected.scip_path.is_none(),
        "explicit browser options override recorded inputs"
    );
}

#[tokio::test]
async fn failed_h_backs_off_and_empty_idle_checkout_can_release() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    runtime.set_pre_h_hook_for_tests(std::sync::Arc::new(|| panic!("injected H failure")));
    tokio::time::timeout(Duration::from_secs(5), async {
        while runtime.reconciliation_error().is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    registry.disconnect(1);
    tokio::time::timeout(Duration::from_millis(180), async {
        while !registry.release(&id.root_key).unwrap() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("idle failed H may release during backoff");
    assert!(!runtime.has_active_resources());
}

#[tokio::test]
async fn failed_h_backoff_never_releases_accepted_fifo_work() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    let id = identity(&checkout);
    let topology = roots(base.path());
    let store = Store::open(topology.clone(), identity(&checkout)).unwrap();
    let queued = store
        .enqueue_request(&trellis::indexer::IndexOptions::new(checkout), None)
        .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    runtime.set_pre_h_hook_for_tests(std::sync::Arc::new(|| panic!("injected queued H failure")));
    tokio::time::timeout(Duration::from_secs(5), async {
        while runtime.reconciliation_error().is_none() || !runtime.retry_waiting_for_tests() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        runtime.retry_waiting_for_tests(),
        "H has ended; this is the backoff window"
    );
    registry.disconnect(1);
    assert!(
        !registry.release(&id.root_key).unwrap(),
        "accepted FIFO work, not h_in_flight, must pin owner during backoff"
    );
    assert!(runtime.has_active_resources());
    assert_eq!(
        store.request_by_id(&queued.id).unwrap().unwrap().state,
        "queued"
    );
}

#[tokio::test]
async fn replaced_path_cold_runtime_publishes_new_identity_not_old_head() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    let moved = base.path().join("old");
    fs::create_dir(&checkout).unwrap();
    fs::write(
        checkout.join("a.js"),
        "function priorIdentity() {}
",
    )
    .unwrap();
    let old_id = identity(&checkout);
    let topology = roots(base.path());
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(1, &old_id).unwrap();
    let prior = registry.activate(&old_id.root_key).unwrap();
    ready(&prior).await;
    registry.disconnect(1);
    release_when_idle(&mut registry, &old_id.root_key).await;
    fs::rename(&checkout, &moved).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(
        checkout.join("a.js"),
        "function newIdentity() {}
",
    )
    .unwrap();
    let next = identity(&checkout);
    registry.attach_launch(2, &next).unwrap();
    let current = registry.activate(&next.root_key).unwrap();
    ready(&current).await;
    let (answer, _) = current.evidence_response().unwrap();
    let (_, source) = answer.source_at("a.js", None).unwrap().unwrap();
    assert!(source.text.contains("newIdentity"));
    assert!(!source.text.contains("priorIdentity"));
    answer.finish(()).unwrap();
    assert!(prior.evidence_response().is_err());
}

#[tokio::test]
async fn browser_identity_retires_replaced_root_after_lease_and_reads_finish() {
    use std::time::Instant;
    use trellis::daemon::registry::{BROWSER_IDLE_DELAY, CHECKOUT_RELEASE_DELAY};
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    let moved = base.path().join("old");
    fs::create_dir(&checkout).unwrap();
    fs::write(
        checkout.join("a.js"),
        "function oldBrowser() {}
",
    )
    .unwrap();
    let old_id = identity(&checkout);
    let key = old_id.root_key.clone();
    let now = Instant::now();
    let mut registry = CheckoutRegistry::with_roots_at(roots(base.path()), now);
    registry
        .register(&old_id, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    registry.browser_request_at(&old_id, now).unwrap();
    let old = registry.activate(&key).unwrap();
    ready(&old).await;
    let (held, _) = old.evidence_response().unwrap();
    fs::rename(&checkout, &moved).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(
        checkout.join("a.js"),
        "function newBrowser() {}
",
    )
    .unwrap();
    let new_id = identity(&checkout);
    assert!(
        registry.browser_identity(&key).is_err(),
        "old browser lease forbids takeover"
    );
    assert_eq!(
        registry
            .browser_checkouts()
            .iter()
            .find(|row| row["rootKey"] == key)
            .unwrap()["state"],
        "unavailable",
        "live browser lease must not list replacement as selectable"
    );
    let due = now + BROWSER_IDLE_DELAY + CHECKOUT_RELEASE_DELAY;
    assert!(
        registry.advance(due).unwrap().released.is_empty(),
        "held read must fence release"
    );
    assert!(
        registry.browser_identity(&key).is_err(),
        "held read forbids replacement"
    );
    assert_eq!(
        registry
            .browser_checkouts()
            .iter()
            .find(|row| row["rootKey"] == key)
            .unwrap()["state"],
        "unavailable",
        "held read must keep listing unavailable"
    );
    drop(held);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if registry.advance(due).unwrap().released.contains(&key) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("old browser runtime did not retire");
    let listed = registry.browser_checkouts();
    let row = listed.iter().find(|row| row["rootKey"] == key).unwrap();
    assert_eq!(
        row["state"], "available",
        "quiescent replacement must be selectable before browser_identity"
    );
    assert_eq!(
        row["active"], false,
        "old index is not an active new checkout"
    );
    assert_eq!(
        row["workspaceRoot"],
        new_id.root.to_string_lossy().to_string()
    );
    assert!(
        registry.registration(&key).is_some(),
        "listing is read-only and cannot retire old registration"
    );
    let discovered = registry
        .browser_identity(&key)
        .expect("browser recovers replaced checkout without CLI attach");
    assert_eq!(
        (
            discovered.device,
            discovered.inode,
            discovered.record_id.as_str()
        ),
        (new_id.device, new_id.inode, new_id.record_id.as_str())
    );
    registry.browser_request_at(&discovered, due).unwrap();
    let current = registry.activate(&key).unwrap();
    ready(&current).await;
    let (answer, _) = current.evidence_response().unwrap();
    let (_, source) = answer.source_at("a.js", None).unwrap().unwrap();
    assert!(source.text.contains("newBrowser"));
    answer.finish(()).unwrap();
    assert!(old.evidence_response().is_err());
}

#[tokio::test]
async fn browser_listing_and_discovery_do_not_reopen_released_sqlite_witnesses() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    let other = base.path().join("other");
    fs::create_dir(&checkout).unwrap();
    fs::create_dir(&other).unwrap();
    fs::write(
        checkout.join("a.js"),
        "function oldListing() {}
",
    )
    .unwrap();
    let id = identity(&checkout);
    let other_id = identity(&other);
    let key = id.root_key.clone();
    let topology = roots(base.path());
    let index_db = topology.index_db(&id);
    let request_db = topology.requests_db(&id);
    let mut registry = CheckoutRegistry::with_roots(topology.clone());
    registry.attach_launch(1, &id).unwrap();
    let released = registry.activate(&key).unwrap();
    ready(&released).await;
    registry.attach_launch(2, &other_id).unwrap();
    let active = registry.activate(&other_id.root_key).unwrap();
    ready(&active).await;
    registry.disconnect(1);
    release_when_idle(&mut registry, &key).await;
    assert_eq!(
        (file_descriptors(&index_db), file_descriptors(&request_db)),
        (0, 0)
    );
    let listed = registry.browser_checkouts();
    assert!(listed.iter().any(|row| row["rootKey"] == key));
    assert_eq!(
        file_descriptors(&index_db),
        0,
        "global listing must not pin released index"
    );
    let mut discovered = CheckoutRegistry::with_roots(topology.clone());
    assert_eq!(discovered.browser_identity(&key).unwrap().inode, id.inode);
    assert_eq!(
        file_descriptors(&index_db),
        0,
        "discovery must not pin released index"
    );
    let independent = Store::open(topology, identity(&checkout)).unwrap();
    let owner = trellis::index_coordinator::establish_serving_session(
        &independent,
        None,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    assert!(owner.is_leader());
    let held = independent.evidence_response().unwrap();
    let _ = registry.browser_checkouts();
    assert!(
        file_descriptors(&index_db) > 0,
        "browser probe must not close a concurrently live SQLite inode"
    );
    held.finish(()).unwrap();
    drop(held);
    assert!(
        file_descriptors(&index_db) > 0,
        "active leader retains its witness between SQLite operations"
    );
    drop(owner);
    drop(independent);
    let _ = registry.browser_checkouts();
    assert_eq!(
        file_descriptors(&index_db),
        0,
        "idle EX proof retires the witness after the independent owner exits"
    );
    let db = rusqlite::Connection::open(&index_db).unwrap();
    db.execute(
        "UPDATE index_metadata SET root_spelling='/bad-root' WHERE singleton=1",
        [],
    )
    .unwrap();
    drop(db);
    let _ = registry.browser_checkouts();
    assert_eq!(
        file_descriptors(&index_db),
        0,
        "corrupt listing must retire probe witness"
    );
    assert!(active.has_active_resources());
}

#[tokio::test]
async fn explicit_null_scip_clears_recorded_input_on_cold_activation() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(
        checkout.join("a.js"),
        "function oldScip() {}
",
    )
    .unwrap();
    let id = identity(&checkout);
    let topology = roots(base.path());
    let store = Store::open(topology.clone(), identity(&checkout)).unwrap();
    let mut options = trellis::indexer::IndexOptions::new(checkout.clone());
    options.scip_path = Some(checkout.join("old.scip"));
    let owner = trellis::index_coordinator::establish_serving_session(
        &store,
        Some(&options),
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    drop(owner);
    drop(store);
    let mut registry = CheckoutRegistry::with_roots(topology.clone());
    registry
        .register(&id, CheckoutOptions(serde_json::json!({"scip":null})))
        .unwrap();
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    let selected = Store::open(topology, identity(&checkout))
        .unwrap()
        .recorded_index_options()
        .unwrap()
        .unwrap();
    assert!(
        selected.scip_path.is_none(),
        "explicit null clears prior SCIP selection"
    );
}

#[test]
fn browser_listing_does_not_offer_ambiguous_same_inode_git_transition() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    let old = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry
        .register(&old, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    fs::write(
        checkout.join(".git"),
        "not a git pointer
",
    )
    .unwrap();
    let listed = registry.browser_checkouts();
    assert_eq!(
        listed
            .iter()
            .find(|row| row["rootKey"] == old.root_key)
            .unwrap()["state"],
        "unavailable",
        "ambiguous same-inode Git metadata is not a proved replacement"
    );
    assert!(registry.browser_identity(&old.root_key).is_err());
    assert_eq!(
        fs::read(checkout.join(".git")).unwrap(),
        b"not a git pointer
"
    );
}

#[tokio::test]
async fn serve_preflight_rejects_busy_replaced_root_before_creating_token() {
    use trellis::daemon::BrowserProvisioner;
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    let moved = base.path().join("old");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function priorOwner() {}\n").unwrap();
    let old_id = identity(&checkout);
    let key = old_id.root_key.clone();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(CheckoutRegistry::with_roots(
        roots(base.path()),
    )));
    let old_options = CheckoutOptions(serde_json::json!({"maxFileBytes":8192}));
    let old = {
        let mut checked = registry.lock().await;
        checked.register(&old_id, old_options.clone()).unwrap();
        checked.attach_launch(1, &old_id).unwrap();
        checked.activate(&key).unwrap()
    };
    ready(&old).await;
    let (held_read, _) = old.evidence_response().unwrap();
    fs::rename(&checkout, &moved).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function newOwner() {}\n").unwrap();
    let next = identity(&checkout);
    registry.lock().await.disconnect(1);
    let token = base.path().join("new-token");
    let mut provisioner = BrowserProvisioner::new();
    let options = CheckoutOptions(serde_json::json!({}));
    assert!(!token.exists());
    assert!(
        provisioner
            .register_serve(
                &registry,
                &next,
                options.clone(),
                "127.0.0.1:0".parse().unwrap(),
                &token
            )
            .await
            .is_err(),
        "retained read must veto replacement serve"
    );
    assert!(
        !token.exists(),
        "failed first serve must not mint an unused token"
    );
    assert_eq!(registry.lock().await.registration(&key), Some(&old_options));
    assert!(old.has_active_resources());
    drop(held_read);
    // The recorded pending bit stays conservative until an actual release
    // refreshes it. A mere read drain does not authorize token creation.
    assert!(
        provisioner
            .register_serve(
                &registry,
                &next,
                options.clone(),
                "127.0.0.1:0".parse().unwrap(),
                &token,
            )
            .await
            .is_err()
    );
    assert!(!token.exists());
    tokio::time::timeout(Duration::from_secs(10), async {
        while !registry.lock().await.release(&key).unwrap() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("old root did not settle and release");
    let mut validated = |stage| {
        if stage == "validated" {
            assert!(
                !old.has_active_resources(),
                "old owner was not retired before token"
            );
        }
    };
    provisioner
        .register_serve_with_hook(
            &registry,
            &next,
            options,
            "127.0.0.1:0".parse().unwrap(),
            &token,
            Some(&mut validated),
        )
        .await
        .expect("new checkout can serve after old runtime drains");
    assert!(token.exists());
    let current = {
        let mut checked = registry.lock().await;
        checked
            .browser_request_at(&next, std::time::Instant::now())
            .unwrap();
        checked.activate(&key).unwrap()
    };
    ready(&current).await;
    let (answer, _) = current.evidence_response().unwrap();
    let (_, source) = answer.source_at("a.js", None).unwrap().unwrap();
    assert!(source.text.contains("newOwner"));
    answer.finish(()).unwrap();
    assert!(old.evidence_response().is_err());
}

async fn root_loss_restricted_ex_case(owner_mode: u8) {
    use std::sync::{Arc, atomic::AtomicU64};
    use trellis::{http, index_coordinator::establish_serving_session, indexer::IndexOptions};
    let base = tempfile::tempdir().unwrap();
    let work = base.path().join("work");
    let moved = base.path().join("moved");
    fs::create_dir(&work).unwrap();
    fs::write(work.join("a.js"), "function previous() {}\n").unwrap();
    let id = identity(&work);
    let topology = roots(base.path());
    let requests_db = topology.requests_db(&id);
    let store = Store::open(topology.clone(), identity(&work)).unwrap();
    let options = IndexOptions::new(work.clone());
    let first = establish_serving_session(
        &store,
        Some(&options),
        &Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    let prior = store.evidence_response().unwrap();
    let epoch = Arc::new(AtomicU64::new(1));
    store.bind_runtime_epoch(epoch.clone());
    store.remember_read_only_predecessor(&prior, epoch).unwrap();
    drop(prior);
    let stale_follower = store.follower_session().unwrap();
    drop(first);
    let restricted = store.leader_session().unwrap();
    assert!(store.restricted_owner_associated());
    let accepted = store.enqueue_request(&options, None).unwrap();
    let state = http::new(
        store.clone(),
        options,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    if owner_mode == 1 {
        state.retain_serving_session_without_tick_for_tests(restricted.clone());
    } else if owner_mode == 2 {
        // Retained stale follower must not shadow the separately live H EX.
        state.retain_serving_session_without_tick_for_tests(stale_follower.clone());
    }
    // In modes 0 and 2 only the H worker owns EX. The weak association must
    // let root loss borrow it until durable FIFO failure completes.
    let h_worker_owner = (owner_mode != 1).then(|| restricted.clone());
    drop(restricted);
    fs::rename(&work, &moved).unwrap();
    fs::create_dir(&work).unwrap();
    state.force_root_transition_tick_for_tests().unwrap();
    let db = rusqlite::Connection::open(requests_db).unwrap();
    let (result, reason): (String, Option<String>) = db
        .query_row(
            "SELECT state,error_code FROM requests WHERE id=?1",
            [&accepted.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (result.as_str(), reason.as_deref()),
        ("failed", Some("root_changed"))
    );
    drop(db);
    assert_eq!(
        state.root_loss_retirement_for_tests(),
        (false, false, false)
    );
    assert!(
        !store.restricted_owner_associated(),
        "root-loss retained old restricted EX"
    );
    drop(h_worker_owner);
    fs::remove_dir(&work).unwrap();
    fs::rename(&moved, &work).unwrap();
    let fresh = topology
        .leader(&id)
        .expect("returned old root can acquire EX promptly");
    assert!(fresh.verify().is_ok());
}

#[tokio::test]
async fn root_loss_drops_restricted_ex_before_old_path_returns() {
    for mode in [0, 1, 2] {
        root_loss_restricted_ex_case(mode).await;
    }
}

async fn failed_metadata_root_fifo_case(block_fifo_write: bool) {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    use trellis::{http, index_coordinator::establish_serving_session, indexer::IndexOptions};
    let base = tempfile::tempdir().unwrap();
    let work = base.path().join("work");
    let moved = base.path().join("old-work");
    fs::create_dir(&work).unwrap();
    fs::write(work.join("a.js"), "function old() {}\n").unwrap();
    let id = identity(&work);
    let topology = roots(base.path());
    let requests_db = topology.requests_db(&id);
    let store = Store::open(topology.clone(), identity(&work)).unwrap();
    let options = IndexOptions::new(work.clone());
    let initial =
        establish_serving_session(&store, Some(&options), &Arc::new(AtomicBool::new(false)))
            .unwrap();
    let prior = store.evidence_response().unwrap();
    let epoch = Arc::new(AtomicU64::new(1));
    store.bind_runtime_epoch(epoch.clone());
    store.remember_read_only_predecessor(&prior, epoch).unwrap();
    drop(prior);
    drop(initial);
    let accepted = store.enqueue_request(&options, None).unwrap();
    let state = http::new(
        store.clone(),
        options,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7332".parse().unwrap(),
    )
    .unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let mark = entered.clone();
    let to_move = work.clone();
    let destination = moved.clone();
    store.set_leader_before_metadata_hook_for_tests(move || {
        fs::rename(&to_move, &destination).unwrap();
        fs::create_dir(&to_move).unwrap();
        mark.store(true, Ordering::Release);
    });
    let external_writer = if block_fifo_write {
        let db = rusqlite::Connection::open(&requests_db).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        Some(db)
    } else {
        None
    };
    assert!(
        store.leader_session().is_err(),
        "metadata admission must fail on root drift"
    );
    assert!(
        entered.load(Ordering::Acquire),
        "post-EX metadata hook never ran"
    );
    if let Some(db) = external_writer {
        assert!(
            store.orphan_root_loss_owner().is_some(),
            "inconclusive write lost EX"
        );
        db.execute_batch("ROLLBACK").unwrap();
    }
    state.force_root_transition_tick_for_tests().unwrap();
    let db = rusqlite::Connection::open(requests_db).unwrap();
    let (result, reason): (String, String) = db
        .query_row(
            "SELECT state,error_code FROM requests WHERE id=?1",
            [&accepted.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (result.as_str(), reason.as_str()),
        ("failed", "root_changed")
    );
    drop(db);
    assert!(!store.restricted_owner_associated());
    assert!(store.orphan_root_loss_owner().is_none());
    fs::remove_dir(&work).unwrap();
    fs::rename(&moved, &work).unwrap();
    let owner = topology
        .leader(&id)
        .expect("old EX retired after durable root loss");
    assert!(owner.verify().is_ok());
}

#[tokio::test]
async fn failed_metadata_admission_disposes_old_root_fifo_before_ex_release() {
    failed_metadata_root_fifo_case(false).await;
    failed_metadata_root_fifo_case(true).await;
}

#[tokio::test]
async fn exceptional_recreate_releases_restricted_owner_before_ex_retry() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64},
    };
    use trellis::{http, index_coordinator::establish_serving_session, indexer::IndexOptions};
    let base = tempfile::tempdir().unwrap();
    let work = base.path().join("work");
    fs::create_dir(&work).unwrap();
    fs::write(work.join("a.js"), "function source() {}\n").unwrap();
    let topology = roots(base.path());
    let id = identity(&work);
    let requests_db = topology.requests_db(&id);
    let index_db = topology.index_db(&id);
    let store = Store::open(topology.clone(), identity(&work)).unwrap();
    let options = IndexOptions::new(work.clone());
    let first =
        establish_serving_session(&store, Some(&options), &Arc::new(AtomicBool::new(false)))
            .unwrap();
    let prior = store.evidence_response().unwrap();
    let epoch = Arc::new(AtomicU64::new(1));
    let permit = store.pre_h_read_permit(&prior, &first, epoch).unwrap();
    drop(prior);
    drop(first);
    let restricted = store.leader_session().unwrap();
    let accepted = store.enqueue_request(&options, None).unwrap();
    // A real obsolete on-disk marker is classified at a new Store admission.
    // Preserve only the H worker's weak restricted association across it.
    let db = rusqlite::Connection::open(&index_db).unwrap();
    db.execute_batch(
        "PRAGMA ignore_check_constraints=ON; UPDATE index_metadata SET schema_version=7;",
    )
    .unwrap();
    drop(db);
    drop(store);
    let recovering = Store::open(topology, identity(&work)).unwrap();
    recovering.associate_restricted_owner_for_tests(&restricted, permit);
    assert!(recovering.restricted_owner_associated());
    let state = http::new(
        recovering.clone(),
        options,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7333".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session_without_tick_for_tests(restricted.clone());
    drop(restricted);
    state
        .force_root_transition_tick_for_tests()
        .expect("exceptional recreate must finish");
    assert!(
        !recovering.restricted_owner_associated(),
        "exceptional path kept pre-H read slot"
    );
    let replacement = state
        .retained_serving_session()
        .expect("EX retry must become serving owner");
    assert!(replacement.is_leader());
    replacement
        .verify()
        .expect("recreated EX must remain verified");
    let db = rusqlite::Connection::open(requests_db).unwrap();
    let row_state: String = db
        .query_row(
            "SELECT state FROM requests WHERE id=?1",
            [&accepted.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        matches!(row_state.as_str(), "done" | "failed"),
        "FIFO row did not reach terminal state: {row_state}"
    );
}

#[tokio::test]
async fn queued_takeover_h_root_loss_fails_fifo_before_last_ex_drops() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64},
    };
    use trellis::{http, index_coordinator::establish_serving_session, indexer::IndexOptions};
    let base = tempfile::tempdir().unwrap();
    let work = base.path().join("work");
    let moved = base.path().join("old-work");
    fs::create_dir(&work).unwrap();
    fs::write(work.join("a.js"), "function initial() {}\n").unwrap();
    let id = identity(&work);
    let topology = roots(base.path());
    let requests_db = topology.requests_db(&id);
    let store = Store::open(topology.clone(), identity(&work)).unwrap();
    let options = IndexOptions::new(work.clone());
    let initial =
        establish_serving_session(&store, Some(&options), &Arc::new(AtomicBool::new(false)))
            .unwrap();
    let prior = store.evidence_response().unwrap();
    let epoch = Arc::new(AtomicU64::new(1));
    store.bind_runtime_epoch(epoch.clone());
    store.remember_read_only_predecessor(&prior, epoch).unwrap();
    drop(prior);
    drop(initial);
    let accepted = store.enqueue_request(&options, None).unwrap();
    let state = http::new(
        store.clone(),
        options,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7334".parse().unwrap(),
    )
    .unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    state.set_checkout_takeover_h_hook_for_tests(Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx.lock().unwrap().recv().unwrap();
    }));
    let worker = state.clone();
    let ticking = std::thread::spawn(move || worker.force_root_transition_tick_for_tests());
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("takeover never reached H hook");
    assert!(
        store.restricted_owner_associated(),
        "new EX was not associated at H hook"
    );
    fs::rename(&work, &moved).unwrap();
    fs::create_dir(&work).unwrap();
    resume_tx.send(()).unwrap();
    let _ = ticking.join().unwrap();
    let db = rusqlite::Connection::open(requests_db).unwrap();
    let (result, reason): (String, Option<String>) = db
        .query_row(
            "SELECT state,error_code FROM requests WHERE id=?1",
            [&accepted.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (result.as_str(), reason.as_deref()),
        ("failed", Some("root_changed"))
    );
    drop(db);
    assert!(!store.restricted_owner_associated());
    assert!(store.orphan_root_loss_owner().is_none());
    fs::remove_dir(&work).unwrap();
    fs::rename(&moved, &work).unwrap();
    let fresh = topology
        .leader(&id)
        .expect("old EX retires only after FIFO root_changed");
    assert!(fresh.verify().is_ok());
}
