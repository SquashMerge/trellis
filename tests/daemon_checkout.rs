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
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
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
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while !registry.release(key).unwrap() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("checkout did not release after startup reconciliation and watcher quiescence");
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

#[test]
fn browser_listing_without_an_index_does_not_activate_or_create_state() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    let id = identity(&checkout);
    let topology = roots(base.path());
    let mut registry = CheckoutRegistry::with_roots(topology.clone());
    registry
        .register(&id, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    let listed = registry.browser_checkouts();
    let row = listed
        .iter()
        .find(|row| row["rootKey"] == id.root_key)
        .unwrap();
    assert_eq!(row["state"], "available");
    assert_eq!(row["active"], false);
    assert!(row["revision"].is_null());
    assert!(row["catchingUp"].is_null());
    assert_eq!(row["indexError"], "index_not_ready");
    assert!(registry.runtime(&id.root_key).is_none());
    assert!(!topology.cache.exists());
}

#[test]
fn browser_listing_cold_index_reports_pin_without_claiming_freshness() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function coldHead() {}\n").unwrap();
    let id = identity(&checkout);
    let topology = roots(base.path());
    let store = Store::open(topology.clone(), identity(&checkout)).unwrap();
    let owner = trellis::index_coordinator::establish_serving_session(
        &store,
        None,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    let first = store.status().unwrap().revision;
    drop(owner);
    drop(store);
    // A cold head does not establish equality with the current disk.
    fs::write(checkout.join("a.js"), "function newerDisk() {}\n").unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry
        .register(&id, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    let listed = registry.browser_checkouts();
    let row = listed
        .iter()
        .find(|row| row["rootKey"] == id.root_key)
        .unwrap();
    assert_eq!(row["state"], "available");
    assert_eq!(row["revision"], serde_json::json!(first));
    assert!(row["catchingUp"].is_null());
    assert!(row["indexError"].is_null());
    assert!(registry.runtime(&id.root_key).is_none());
    assert_eq!(
        file_descriptors(
            &base
                .path()
                .join("cache/indexes")
                .join(&id.root_key)
                .join("index.db")
        ),
        0
    );
}

#[test]
fn browser_listing_cold_index_distinguishes_busy_and_corrupt_from_no_head() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function coldHead() {}\n").unwrap();
    let id = identity(&checkout);
    let topology = roots(base.path());
    let store = Store::open(topology.clone(), identity(&checkout)).unwrap();
    let owner = trellis::index_coordinator::establish_serving_session(
        &store,
        None,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    drop(owner);
    drop(store);
    let index = topology.index_db(&id);
    let journal = index.with_file_name("index.db-journal");
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry
        .register(&id, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    fs::write(&journal, []).unwrap();
    let listed = registry.browser_checkouts();
    let row = listed
        .iter()
        .find(|row| row["rootKey"] == id.root_key)
        .unwrap();
    assert_eq!(row["state"], "storage_busy");
    assert_eq!(row["indexError"], "storage_busy");
    assert!(row["revision"].is_null());
    assert!(row["catchingUp"].is_null());
    fs::remove_file(journal).unwrap();
    let db = rusqlite::Connection::open(&index).unwrap();
    db.execute(
        "UPDATE index_metadata SET index_generation='invalid' WHERE singleton=1",
        [],
    )
    .unwrap();
    drop(db);
    let listed = registry.browser_checkouts();
    let row = listed
        .iter()
        .find(|row| row["rootKey"] == id.root_key)
        .unwrap();
    assert_eq!(row["state"], "corrupt");
    assert_eq!(row["indexError"], "corrupt");
    assert!(row["revision"].is_null());
    assert!(row["catchingUp"].is_null());
    assert!(registry.runtime(&id.root_key).is_none());
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn browser_listing_active_without_a_head_reports_catching_up() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function firstHead() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry.attach_launch(1, &id).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx.lock().unwrap().recv().unwrap();
    }));
    let runtime = registry.activate(&id.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while entered_rx.try_recv().is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let listed = registry.browser_checkouts();
    let row = listed
        .iter()
        .find(|row| row["rootKey"] == id.root_key)
        .unwrap();
    assert_eq!(row["state"], "available");
    assert!(row["revision"].is_null());
    assert_eq!(row["catchingUp"], true);
    assert_eq!(row["indexError"], "index_not_ready");
    resume_tx.send(()).unwrap();
    ready(&runtime).await;
}

#[tokio::test]
async fn separate_store_writer_does_not_downgrade_daemon_read() {
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
    leader.verify().unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    assert!(leader.verify().is_ok());
    let (response, catching_up) = runtime.evidence_response().unwrap();
    assert!(!catching_up);
    response.status().unwrap();
    response.finish(()).unwrap();
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn reattach_reports_catching_up_and_keeps_complete_head_until_startup() {
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
    assert!(
        runtime.catching_up(),
        "mandatory startup reconciliation has not run yet"
    );
    ready(&runtime).await;
    // A watcher hint can arrive after ready() without invalidating this head.
    let (old, _catching_up) = runtime.evidence_response().unwrap();
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
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pre-startup reconciliation worker did not reach pause");
    assert!(resumed.catching_up());
    let (prior, catching_up) = resumed
        .evidence_response()
        .expect("validated old head while startup reconciliation paused");
    assert!(catching_up);
    assert_eq!(prior.status().unwrap().revision, first);
    let (_, old_source) = prior.source_at("a.js", None).unwrap().unwrap();
    assert!(old_source.text.contains("oldHead"));
    prior.finish(()).unwrap();
    drop(prior);
    let listed = registry.browser_checkouts();
    let row = listed
        .iter()
        .find(|row| row["rootKey"] == id.root_key)
        .unwrap();
    assert_eq!(row["revision"], serde_json::json!(first));
    assert_eq!(row["catchingUp"], true);
    assert!(row["indexError"].is_null());
    assert!(row.get("pendingFileCount").is_none());
    resume_tx.send(()).unwrap();
    ready(&resumed).await;
    let (current, catching_up) = resumed.evidence_response().unwrap();
    assert!(!catching_up);
    let new_revision = current.status().unwrap().revision;
    assert!(new_revision.index_revision > first.index_revision);
    current.finish(()).unwrap();
    let listed = registry.browser_checkouts();
    let row = listed
        .iter()
        .find(|row| row["rootKey"] == id.root_key)
        .unwrap();
    assert_eq!(row["revision"], serde_json::json!(new_revision));
    assert_eq!(row["catchingUp"], false);
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn fresh_registry_restart_serves_prior_head_without_startup_mutation_or_fifo_authority() {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tower::ServiceExt;
    use trellis::{daemon::registry::CheckoutRuntime, http::RuntimePhase};

    async fn startup_committed(runtime: Arc<CheckoutRuntime>) {
        tokio::task::spawn_blocking(move || {
            let scheduler = runtime.browser_scheduler().unwrap();
            let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
            loop {
                let seen = runtime.state_version();
                if scheduler.runtime_phase() == RuntimePhase::Ready {
                    break;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "startup reconciliation did not commit"
                );
                runtime.wait_state_change(seen, remaining);
            }
        })
        .await
        .unwrap();
    }

    async fn release(registry: &mut CheckoutRegistry, key: &str) {
        let signal = registry.lifecycle_signal();
        tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            while !registry.release(key).unwrap() {
                signal.notified().await;
            }
        })
        .await
        .expect("checkout did not retire");
    }

    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function oldHead() {}\n").unwrap();
    let id = identity(&checkout);
    let topology = roots(base.path());
    let mut original = CheckoutRegistry::with_roots(topology.clone());
    original.attach_launch(1, &id).unwrap();
    let runtime = original.activate(&id.root_key).unwrap();
    startup_committed(runtime.clone()).await;
    let (unhold, holder) = hold_worker(runtime.browser_scheduler().unwrap()).await;
    let (prior, _) = runtime.evidence_response().unwrap();
    let first = prior.status().unwrap().revision;
    assert_eq!(
        prior.source_at("a.js", None).unwrap().unwrap().1.text,
        "function oldHead() {}\n"
    );
    prior.finish(()).unwrap();
    drop(prior);
    drop(unhold);
    holder.join().unwrap();
    original.disconnect(1);
    release(&mut original, &id.root_key).await;
    assert!(!runtime.has_active_resources());
    drop(runtime);
    drop(original);

    fs::write(checkout.join("a.js"), "function newHead() {}\n").unwrap();
    let mut restarted = CheckoutRegistry::with_roots(topology.clone());
    restarted.attach_launch(2, &id).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    restarted.set_next_startup_hook_for_tests(Arc::new(move || {
        entered_tx.send(()).unwrap();
        // A failed assertion drops the sender and releases the worker too.
        let _ = resume_rx.lock().unwrap().recv();
    }));
    let resumed = restarted.activate(&id.root_key).unwrap();
    entered(entered_rx, "fresh-registry startup reconciliation").await;
    let (prior, catching_up) = resumed.evidence_response().unwrap();
    assert!(catching_up);
    assert_eq!(prior.status().unwrap().revision, first);
    let (pin, source) = prior.source_at("a.js", None).unwrap().unwrap();
    assert_eq!(pin, first);
    assert_eq!(source.text, "function oldHead() {}\n");
    prior.finish(()).unwrap();
    drop(prior);

    let store = Store::open(topology, identity(&checkout)).unwrap();
    let queued = store
        .enqueue_request(&trellis::indexer::IndexOptions::new(checkout.clone()), None)
        .unwrap();
    let terminal = Arc::new(tokio::sync::Notify::new());
    resumed
        .notify_on_terminal_request_for_tests(&queued.id, terminal.clone())
        .unwrap();
    let registry = Arc::new(Mutex::new(restarted));
    let token = "0123456789abcdef".repeat(4);
    let app = trellis::http::ProvisionedBrowser::new(
        registry.clone(),
        token.clone(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap()
    .router();
    let refused = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/checkouts/{}/dependencies/refresh",
                    id.root_key
                ))
                .header("host", "127.0.0.1:7331")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.headers()["X-Trellis-Catching-Up"], "true");
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(refused.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "index_not_ready");
    let pending = store.request_by_id(&queued.id).unwrap().unwrap();
    assert_eq!(
        pending.state, "queued",
        "FIFO cannot claim before startup commits"
    );
    assert!(pending.revision.is_none());
    assert_eq!(
        resumed
            .browser_scheduler()
            .unwrap()
            .startup_captures_for_tests(),
        0
    );

    resume_tx.send(()).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, terminal.notified())
        .await
        .expect("queued request did not finish after restarted startup");
    let done = store.request_by_id(&queued.id).unwrap().unwrap();
    assert_eq!(done.state, "done");
    let published = done.revision.unwrap();
    assert_eq!(published.index_generation, first.index_generation);
    assert!(published.index_revision > first.index_revision);
    let scheduler = resumed.browser_scheduler().unwrap();
    let (unhold, holder) = hold_worker(scheduler.clone()).await;
    assert_eq!(scheduler.runtime_phase(), RuntimePhase::Ready);
    // A terminal FIFO commit does not drain later native watcher hints.
    let (current, _catching_up) = resumed.evidence_response().unwrap();
    assert!(current.status().unwrap().revision.index_revision >= published.index_revision);
    assert_eq!(
        current.source_at("a.js", None).unwrap().unwrap().1.text,
        "function newHead() {}\n"
    );
    current.finish(()).unwrap();
    drop(current);
    assert_eq!(
        resumed
            .browser_scheduler()
            .unwrap()
            .startup_captures_for_tests(),
        1
    );
    let allowed = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/checkouts/{}/dependencies/refresh",
                    id.root_key
                ))
                .header("host", "127.0.0.1:7331")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::ACCEPTED);
    drop(allowed);
    drop(unhold);
    holder.join().unwrap();
    drop(store);
    let mut restarted = registry.lock().await;
    restarted.disconnect(2);
    restarted
        .advance(std::time::Instant::now() + Duration::from_secs(30 * 60))
        .unwrap();
    release(&mut restarted, &id.root_key).await;
    assert!(!resumed.has_active_resources());
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn symbols_and_graph_reads_use_the_admitted_prior_head_during_startup() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function oldHead() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    registry.disconnect(1);
    release_when_idle(&mut registry, &id.root_key).await;
    fs::write(checkout.join("a.js"), "function newHead() {}\n").unwrap();
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
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("H did not pause");
    let symbols = resumed.cli_symbols("oldHead", 10).unwrap();
    assert_eq!(symbols["catchingUp"], true);
    assert_eq!(symbols["data"]["items"].as_array().unwrap().len(), 1);
    let (response, catching_up) = resumed.evidence_response().unwrap();
    assert!(catching_up);
    let graph = response.graph().unwrap();
    assert!(graph.files[0].text.contains("oldHead"));
    response.finish(graph).unwrap();
    drop(response);
    resume_tx.send(()).unwrap();
    ready(&resumed).await;
    let symbols = resumed.cli_symbols("newHead", 10).unwrap();
    assert_eq!(symbols["catchingUp"], false);
    assert_eq!(symbols["data"]["items"].as_array().unwrap().len(), 1);
    let (response, catching_up) = resumed.evidence_response().unwrap();
    assert!(!catching_up);
    let graph = response.graph().unwrap();
    assert!(graph.files[0].text.contains("newHead"));
    response.finish(graph).unwrap();
    drop(response);
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
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
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

#[cfg(feature = "test-support")]
#[tokio::test]
async fn replacement_settles_old_queue_before_startup_then_claims_distinct_fifo_options() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function oldRoot() {}\n").unwrap();
    let old_id = identity(&checkout);
    let topology = roots(base.path());
    let mut registry = CheckoutRegistry::with_roots(topology.clone());
    registry.attach_launch(1, &old_id).unwrap();
    let old_runtime = registry.activate(&old_id.root_key).unwrap();
    ready(&old_runtime).await;
    registry.disconnect(1);
    release_when_idle(&mut registry, &old_id.root_key).await;
    let old_store = Store::open(topology.clone(), identity(&checkout)).unwrap();
    let old = old_store
        .enqueue_request(&trellis::indexer::IndexOptions::new(checkout.clone()), None)
        .unwrap();
    drop(old_store);
    fs::rename(&checkout, base.path().join("moved")).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(
        checkout.join("a.js"),
        format!("function newRoot() {{}}\n{}", " ".repeat(6000)),
    )
    .unwrap();
    let new_id = identity(&checkout);
    let new_store = Store::open(topology.clone(), identity(&checkout)).unwrap();
    let mut a_options = trellis::indexer::IndexOptions::new(checkout.clone());
    a_options.max_file_bytes = 7000;
    let mut b_options = a_options.clone();
    b_options.max_file_bytes = 8192;
    let a = new_store.enqueue_request(&a_options, None).unwrap();
    let b = new_store.enqueue_request(&b_options, None).unwrap();
    assert!(old.seq < a.seq && a.seq < b.seq);
    registry.attach_launch(2, &new_id).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx.lock().unwrap().recv().unwrap();
    }));
    let runtime = registry.activate(&new_id.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while entered_rx.try_recv().is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("new-root startup reconciliation did not reach barrier");
    let old_row = new_store
        .request_any_root_for_tests(&old.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        (old_row.state.as_str(), old_row.error_code.as_deref()),
        ("failed", Some("root_changed"))
    );
    assert!(
        old_row.finished_at.is_some(),
        "old-root settlement must be durable before H"
    );
    for row in [&a, &b] {
        let current = new_store.request_by_id(&row.id).unwrap().unwrap();
        assert_eq!(current.state, "queued", "new-root claims must wait for H");
    }
    let (claim_tx, claim_rx) = std::sync::mpsc::sync_channel(1);
    let (continue_tx, continue_rx) = std::sync::mpsc::sync_channel(1);
    let continue_rx = std::sync::Mutex::new(continue_rx);
    runtime.set_after_claim_hook_for_tests(move || {
        claim_tx.send(()).unwrap();
        continue_rx.lock().unwrap().recv().unwrap();
    });
    resume_tx.send(()).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while claim_rx.try_recv().is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first FIFO claim did not reach barrier");
    let (published_h, _) = runtime
        .evidence_response()
        .expect("H must commit before A claim");
    let (_, h_source) = published_h.source_at("a.js", None).unwrap().unwrap();
    assert!(h_source.text.contains("newRoot"));
    published_h.finish(()).unwrap();
    drop(published_h);
    assert_eq!(
        new_store.request_by_id(&a.id).unwrap().unwrap().state,
        "running"
    );
    assert_eq!(
        new_store.request_by_id(&b.id).unwrap().unwrap().state,
        "queued"
    );
    continue_tx.send(()).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if [&a, &b].iter().all(|row| {
                new_store
                    .request_by_id(&row.id)
                    .unwrap()
                    .unwrap()
                    .finished_at
                    .is_some()
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("FIFO A/B did not finish");
    let a_row = new_store.request_by_id(&a.id).unwrap().unwrap();
    let b_row = new_store.request_by_id(&b.id).unwrap().unwrap();
    assert_eq!(
        (a_row.state.as_str(), b_row.state.as_str()),
        ("done", "done")
    );
    let a_pin = a_row.revision.unwrap();
    let b_pin = b_row.revision.unwrap();
    assert_eq!(a_pin.index_generation, b_pin.index_generation);
    assert!(
        a_pin.index_revision < b_pin.index_revision,
        "FIFO ACK pins must differ"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&a_row.options_json).unwrap()["maxFileBytes"],
        7000
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&b_row.options_json).unwrap()["maxFileBytes"],
        8192
    );
    let selected_store = Store::open(topology, identity(&checkout)).unwrap();
    for pin in [a_pin, b_pin] {
        assert!(
            selected_store
                .source_at("a.js", Some(pin))
                .unwrap()
                .unwrap()
                .1
                .text
                .contains("newRoot")
        );
    }
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn ambiguous_root_settlement_blocks_startup_and_fifo_until_queue_reread() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function oldRoot() {}\n").unwrap();
    let old_id = identity(&checkout);
    let topology = roots(base.path());
    let old_store = Store::open(topology.clone(), old_id).unwrap();
    let old = old_store
        .enqueue_request(&trellis::indexer::IndexOptions::new(checkout.clone()), None)
        .unwrap();
    drop(old_store);
    fs::rename(&checkout, base.path().join("moved")).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function newRoot() {}\n").unwrap();
    let new_id = identity(&checkout);
    let new_store = Store::open(topology.clone(), identity(&checkout)).unwrap();
    let accepted = new_store
        .enqueue_request(&trellis::indexer::IndexOptions::new(checkout.clone()), None)
        .unwrap();
    let (ambiguous_tx, ambiguous_rx) = std::sync::mpsc::sync_channel(1);
    let (confirm_tx, confirm_rx) = std::sync::mpsc::sync_channel(1);
    let confirm_rx = std::sync::Mutex::new(confirm_rx);
    new_store.set_ambiguous_settlement_hook_for_tests(move || {
        ambiguous_tx.send(()).unwrap();
        confirm_rx.lock().unwrap().recv().unwrap();
    });
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(1, &new_id).unwrap();
    let (h_tx, h_rx) = std::sync::mpsc::sync_channel(1);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        h_tx.send(()).unwrap();
    }));
    let _runtime = registry.activate(&new_id.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while ambiguous_rx.try_recv().is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("ambiguous COMMIT barrier not reached");
    assert!(
        h_rx.try_recv().is_err(),
        "H began before queue COMMIT confirmation"
    );
    assert_eq!(
        new_store
            .request_by_id(&accepted.id)
            .unwrap()
            .unwrap()
            .state,
        "queued"
    );
    assert_eq!(
        new_store
            .request_any_root_for_tests(&old.id)
            .unwrap()
            .unwrap()
            .error_code
            .as_deref(),
        Some("root_changed")
    );
    confirm_tx.send(()).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while h_rx.try_recv().is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("H did not start after confirmed queue reread");
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            match new_store.request_by_id(&accepted.id) {
                Ok(Some(row)) if row.finished_at.is_some() => break,
                Ok(Some(_)) => {} // Accepted FIFO row still waits for startup reconciliation or its claim.
                Err(error) if error.to_string().starts_with("storage_busy") => {
                    // Nonblocking index-use admission is bounded while startup reconciliation owns EX.
                }
                other => panic!(
                    "unexpected accepted-row result during startup reconciliation: {other:?}"
                ),
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("queued request did not complete after confirmed H");
    let row = new_store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(row.state, "done");
    let selected = Store::open(roots(base.path()), identity(&checkout)).unwrap();
    let pin = row.revision.unwrap();
    assert!(
        selected
            .source_at("a.js", Some(pin))
            .unwrap()
            .unwrap()
            .1
            .text
            .contains("newRoot")
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
    let lock_path = topology.index_use_lock(&id);
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
        "active Store holds checkout index-use lock"
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
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
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
    let (response, _) = resumed.evidence_response().unwrap();
    response.finish(()).unwrap();
}

#[cfg(feature = "test-support")]
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
    let leader_path = topology.index_use_lock(&id);
    let mut registry = CheckoutRegistry::with_roots(topology.clone());
    registry.attach_launch(1, &id).unwrap();
    drop(id);
    let old = registry.activate(&key).unwrap();
    ready(&old).await;
    // The row is still accepted, not yet claimed, when the root moves.
    let scheduler = old.browser_scheduler().unwrap();
    let held = scheduler.hold_worker_for_tests();
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
    drop(held);
    // A published pending-work flag can be briefly false before this worker
    // processes the enqueue. Retired is the owner's post-COMMIT root-loss
    // transition, not a sampled queue-empty hint.
    retired(&scheduler).await;
    assert!(scheduler.checkout_root_loss_retired());
    drop(scheduler);
    assert!(!registry.refresh_pending_work(&key).unwrap());
    let db = rusqlite::Connection::open(db_path).unwrap();
    let (state, error): (String, Option<String>) = db
        .query_row(
            "SELECT state,error_code FROM requests WHERE id=?1",
            [&row.id],
            |record| Ok((record.get(0)?, record.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (state.as_str(), error.as_deref()),
        ("failed", Some("root_changed"))
    );
    drop(db);
    release_when_idle(&mut registry, &key).await;
    assert_eq!(
        registry.browser_identity(&key).unwrap().inode,
        replacement.inode
    );
    assert!(!old.has_active_resources());
    assert!(old.evidence_response().is_err());
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while file_descriptors(&moved) != 0 || file_descriptors(&leader_path) != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("old runtime retained root/index-use descriptors after release");
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn root_loss_during_paused_reattach_startup_waits_for_worker_then_releases() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    let moved = base.path().join("moved");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function beforeRelease() {}\n").unwrap();
    let id = identity(&checkout);
    let key = id.root_key.clone();
    let topology = roots(base.path());
    let leader_path = topology.index_use_lock(&id);
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(1, &id).unwrap();
    let original = registry.activate(&key).unwrap();
    ready(&original).await;
    registry.disconnect(1);
    release_when_idle(&mut registry, &key).await;
    registry.attach_launch(2, &id).unwrap();
    drop(id);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx.lock().unwrap().recv().unwrap();
    }));
    let paused = registry.activate(&key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while entered_rx.try_recv().is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("idle reattach did not elect before startup reconciliation pause");
    fs::rename(&checkout, &moved).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function replacement() {}\n").unwrap();
    let replacement = identity(&checkout);
    registry.disconnect(2);
    assert!(
        registry.browser_identity(&key).is_err(),
        "paused startup reconciliation keeps old incarnation even after client disconnect"
    );
    assert_eq!(
        registry
            .browser_checkouts()
            .iter()
            .find(|row| row["rootKey"] == key)
            .unwrap()["state"],
        "unavailable",
        "paused startup reconciliation prevents replacement listing"
    );
    assert!(
        paused.catching_up(),
        "paused startup reconciliation is pending despite an empty old-root queue"
    );
    assert!(
        !registry.release(&key).unwrap(),
        "live startup reconciliation owner must block idle release"
    );
    assert!(paused.has_active_resources());
    assert!(file_descriptors(&moved) > 0);
    resume_tx.send(()).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while !registry.release(&key).unwrap() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("root-lost startup reconciliation did not retire and release");
    assert!(!paused.has_active_resources());
    assert!(paused.evidence_response().is_err());
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while file_descriptors(&moved) != 0 || file_descriptors(&leader_path) != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("paused startup reconciliation leaked old-root or index-use descriptors");
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
    owner.verify().unwrap();
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
    assert!(!runtime.has_active_resources());
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
    assert!(!explicit_default.has_active_resources());
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

#[cfg(feature = "test-support")]
#[tokio::test]
async fn failed_startup_backs_off_and_empty_idle_checkout_can_release() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry.attach_launch(1, &id).unwrap();
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(|| {
        panic!("injected startup reconciliation failure")
    }));
    let runtime = registry.activate(&id.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
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
    .expect("idle failed startup reconciliation may release during backoff");
    assert!(!runtime.has_active_resources());
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn failed_startup_backoff_never_releases_accepted_fifo_work() {
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
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(|| {
        panic!("injected queued startup reconciliation failure")
    }));
    let runtime = registry.activate(&id.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
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

#[cfg(feature = "test-support")]
fn init_selection_checkout(root: &Path, source: &str) -> WorkspaceIdentity {
    fs::create_dir(root).unwrap();
    let output = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .arg(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git init: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(root.join("a.js"), source).unwrap();
    identity(root)
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn prepared_selection_waits_for_live_same_key_replacement_retirement() {
    use trellis::{daemon::registry::SelectionError, http::RuntimePhase};
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().canonicalize().unwrap().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function oldSelection() {}\n").unwrap();
    let old_id = identity(&checkout);
    let key = old_id.root_key.clone();
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry
        .register(&old_id, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    registry.attach_launch(1, &old_id).unwrap();
    let old = registry.activate(&key).unwrap();
    ready(&old).await;
    let scheduler = old.browser_scheduler().unwrap();
    let (release_worker, holder) = hold_worker(scheduler.clone()).await;
    let (held_read, _) = old.evidence_response().unwrap();
    // Git initialization replaces the identity without invalidating the old
    // root's queue reads. Post-step FIFO publication is outside the worker hold,
    // so a pathname replacement here could conservatively publish pending work.
    let output = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .arg(&checkout)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git init: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(checkout.join("a.js"), "function newSelection() {}\n").unwrap();
    let next = identity(&checkout);
    assert_eq!(old_id.root_key, next.root_key);
    assert_eq!((old_id.device, old_id.inode), (next.device, next.inode));
    assert_ne!(old_id.record_id, next.record_id);
    let prepared = registry
        .selection_probe()
        .prepare(&next, &checkout)
        .unwrap();
    assert!(matches!(
        registry.select_prepared(2, prepared),
        Err(SelectionError::IdentityChanged)
    ));
    drop(held_read);
    assert!(!registry.refresh_pending_work(&key).unwrap());
    registry.disconnect(1);
    let prepared = registry
        .selection_probe()
        .prepare(&next, &checkout)
        .unwrap();
    let (closing_tx, closing_rx) = std::sync::mpsc::sync_channel(1);
    let closing_old = old.clone();
    let closing_scheduler = scheduler.clone();
    scheduler.set_closing_hook_for_tests(move || {
        closing_tx
            .send((
                closing_scheduler.runtime_phase(),
                closing_old.has_active_resources(),
            ))
            .unwrap();
    });
    // Selection must request retirement while the old worker is still held.
    // This observer releases it without ever acquiring the registry.
    let observer_scheduler = scheduler.clone();
    let selection_finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observer_finished = selection_finished.clone();
    let observer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            let seen = observer_scheduler.state_version();
            if observer_scheduler.runtime_phase() == RuntimePhase::RetireRequested {
                break;
            }
            if observer_finished.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "selection never requested retirement");
            // State signals prove retirement; this short cap only lets an
            // early selection error stop the observer instead of masking it.
            observer_scheduler.wait_state_change(seen, remaining.min(Duration::from_millis(20)));
        }
        drop(release_worker);
    });
    let selected = registry.select_prepared(2, prepared);
    selection_finished.store(true, std::sync::atomic::Ordering::Release);
    observer.join().unwrap();
    holder.join().unwrap();
    let selected = selected.expect("selection waits for the requested retirement");
    assert_eq!(closing_rx.recv().unwrap(), (RuntimePhase::Closing, true));
    selected.before_answer().unwrap();
    assert_eq!(selected.record_id, next.record_id);
    assert_eq!((selected.device, selected.inode), (next.device, next.inode));
    assert!(!old.has_active_resources());
    assert!(old.evidence_response().is_err());
    assert!(registry.runtime(&key).is_none());
    assert!(registry.registration(&key).is_none());
    assert_eq!(registry.active_count(), 1);
    let fresh = registry.activate(&key).unwrap();
    assert!(!std::sync::Arc::ptr_eq(&old, &fresh));
    ready(&fresh).await;
    let (answer, _) = fresh.evidence_response().unwrap();
    let (_, source) = answer.source_at("a.js", None).unwrap().unwrap();
    assert!(source.text.contains("newSelection"));
    assert!(!source.text.contains("oldSelection"));
    answer.finish(()).unwrap();
    drop(answer);
    registry.disconnect(2);
    release_when_idle(&mut registry, &key).await;
    assert_eq!(registry.active_count(), 0);
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn prepared_selection_rechecks_root_lost_replacement_and_resets_stale_clients() {
    use trellis::daemon::registry::SelectionError;
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().canonicalize().unwrap().join("work");
    let old_id = init_selection_checkout(&checkout, "function oldSelection() {}\n");
    let key = old_id.root_key.clone();
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry
        .register(&old_id, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    registry.attach_launch(1, &old_id).unwrap();
    registry
        .browser_request_at(&old_id, std::time::Instant::now())
        .unwrap();
    let old = registry.activate(&key).unwrap();
    ready(&old).await;
    let scheduler = old.browser_scheduler().unwrap();
    let (held_read, _) = old.evidence_response().unwrap();
    fs::rename(&checkout, base.path().join("old")).unwrap();
    let next = init_selection_checkout(&checkout, "function intermediateSelection() {}\n");
    let _ = scheduler.submit_watch_event_for_tests(Ok(notify::Event::new(
        notify::EventKind::Remove(notify::event::RemoveKind::Any),
    )));
    retired(&scheduler).await;
    assert!(scheduler.checkout_root_loss_retired());
    let prepared = registry
        .selection_probe()
        .prepare(&next, &checkout)
        .unwrap();
    assert!(matches!(
        registry.select_prepared(2, prepared),
        Err(SelectionError::IdentityChanged)
    ));
    drop(held_read);
    // The prepared replacement itself can change before commit, even when the
    // registry still contains exactly the old metadata captured by prepare.
    let stale = registry
        .selection_probe()
        .prepare(&next, &checkout)
        .unwrap();
    fs::rename(&checkout, base.path().join("intermediate")).unwrap();
    let latest = init_selection_checkout(&checkout, "function latestSelection() {}\n");
    assert!(matches!(
        registry.select_prepared(2, stale),
        Err(SelectionError::IdentityChanged)
    ));
    assert!(registry.registration(&key).is_some());
    let prepared = registry
        .selection_probe()
        .prepare(&latest, &checkout)
        .unwrap();
    let selected = registry.select_prepared(2, prepared).unwrap();
    selected.before_answer().unwrap();
    assert_eq!(selected.record_id, latest.record_id);
    assert_eq!(
        (selected.device, selected.inode),
        (latest.device, latest.inode)
    );
    assert!(!old.has_active_resources());
    assert!(old.evidence_response().is_err());
    assert!(registry.runtime(&key).is_none());
    assert!(registry.registration(&key).is_none());
    assert_eq!(registry.active_count(), 1);
    let fresh = registry.activate(&key).unwrap();
    assert!(!std::sync::Arc::ptr_eq(&old, &fresh));
    ready(&fresh).await;
    let (answer, _) = fresh.evidence_response().unwrap();
    let (_, source) = answer.source_at("a.js", None).unwrap().unwrap();
    assert!(source.text.contains("latestSelection"));
    assert!(!source.text.contains("oldSelection"));
    answer.finish(()).unwrap();
    drop(answer);
    // Only the new MCP session owns this entry: neither the stale session nor
    // the stale browser lease may keep the replacement alive.
    registry.disconnect(2);
    release_when_idle(&mut registry, &key).await;
    assert_eq!(registry.active_count(), 0);
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
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
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
    assert!(
        row["revision"].is_null(),
        "replacement must not inherit old pin"
    );
    assert!(row["catchingUp"].is_null());
    assert_eq!(row["indexError"], "index_not_ready");
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
    owner.verify().unwrap();
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

#[cfg(feature = "test-support")]
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
    // The worker notices the loss only after the checks below. Otherwise the
    // root-loss rule (a stale client never blocks a verified replacement
    // once no read is held) applies instead, as inotify can make it on Linux.
    let (release_worker, holder) = hold_worker(old.browser_scheduler().unwrap()).await;
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
    release_worker.send(()).unwrap();
    holder.join().unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
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

/// Decision 0009 §6: `serve` registers on a CLI-activated runtime in place,
/// without waiting for its accepted index. That request keeps its own
/// options; later work uses the new configuration; the runtime is not replaced.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn serve_registers_in_place_while_an_accepted_index_runs() {
    use trellis::daemon::BrowserProvisioner;
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let identity = identity(&checkout);
    let key = identity.root_key.clone();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(CheckoutRegistry::with_roots(
        roots(base.path()),
    )));
    let runtime = {
        let mut checked = registry.lock().await;
        checked.attach_launch(41, &identity).unwrap();
        checked.activate(&key).unwrap()
    };
    ready(&runtime).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let (claimed_tx, claimed_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel::<()>(1);
    runtime.set_after_claim_hook_for_tests(move || {
        claimed_tx.send(()).unwrap();
        let _ = resume_rx.recv();
    });
    let store = Store::open(roots(base.path()), crate::identity(&checkout)).unwrap();
    let accepted = store
        .enqueue_request(&trellis::indexer::IndexOptions::new(checkout.clone()), None)
        .unwrap();
    entered(claimed_rx, "accepted index claim").await;
    // The index is held mid-drain: serve completes anyway.
    let options = CheckoutOptions(serde_json::json!({"maxFileBytes": 4096}));
    let mut provisioner = BrowserProvisioner::new();
    tokio::time::timeout(
        trellis::testing::HANG_BACKSTOP,
        provisioner.register_serve(
            &registry,
            &identity,
            options.clone(),
            "127.0.0.1:0".parse().unwrap(),
            &base.path().join("token"),
        ),
    )
    .await
    .expect("serve waited for accepted index work")
    .unwrap();
    assert_eq!(registry.lock().await.registration(&key), Some(&options));
    assert_eq!(scheduler.config().options.max_file_bytes, 4096);
    assert!(std::sync::Arc::ptr_eq(
        &registry.lock().await.runtime(&key).unwrap(),
        &runtime
    ));
    // Reads continue while the index is held.
    let (read, _) = runtime.evidence_response().unwrap();
    read.finish(()).unwrap();
    drop(read);
    resume_tx.send(()).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if store.request_by_id(&accepted.id).unwrap().unwrap().state == "done"
                && store
                    .recorded_index_options()
                    .unwrap()
                    .is_some_and(|recorded| recorded.max_file_bytes == 4096)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("accepted index and the new options did not both apply");
    let row = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&row.options_json).unwrap()["maxFileBytes"],
        2_097_152,
        "the accepted request keeps the options recorded with it"
    );
}

/// Decision 0009 §6: a configuration that fails to build refuses the
/// registration and publishes nothing; the previous configuration stays.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn failed_serve_configuration_publishes_nothing() {
    use trellis::daemon::BrowserProvisioner;
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let identity = identity(&checkout);
    let key = identity.root_key.clone();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(CheckoutRegistry::with_roots(
        roots(base.path()),
    )));
    let runtime = {
        let mut checked = registry.lock().await;
        checked.attach_launch(41, &identity).unwrap();
        checked.activate(&key).unwrap()
    };
    ready(&runtime).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    // The directory exists for validation, but the build cannot open it.
    let read_only = base.path().join("read-only");
    fs::create_dir(&read_only).unwrap();
    std::fs::set_permissions(
        &read_only,
        std::os::unix::fs::PermissionsExt::from_mode(0o000),
    )
    .unwrap();
    let options = CheckoutOptions(serde_json::json!({
        "maxFileBytes": 4096,
        "browseRoot": read_only,
    }));
    let token = base.path().join("token");
    let error = BrowserProvisioner::new()
        .register_serve(
            &registry,
            &identity,
            options,
            "127.0.0.1:0".parse().unwrap(),
            &token,
        )
        .await
        .expect_err("an unbuildable configuration must not register");
    assert!(!error.to_string().is_empty());
    assert!(registry.lock().await.registration(&key).is_none());
    assert_eq!(scheduler.config().options.max_file_bytes, 2_097_152);
    assert!(!token.exists(), "a refused registration mints no token");
    std::fs::set_permissions(
        &read_only,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
}

/// Inventory item 6 (Decision 0009 §1): the worker is the only owner of startup
/// reconciliation. Work accepted while it is paused waits for it and never
/// starts a second reconciliation.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn work_accepted_during_startup_reconciliation_never_starts_a_second_one() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function first() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry.attach_launch(1, &id).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        // A dropped sender (test failure) ends the hold; it never times out.
        let _ = resume_rx.lock().unwrap().recv();
    }));
    let runtime = registry.activate(&id.root_key).unwrap();
    let scheduler = runtime.browser_scheduler().unwrap();
    let entered = tokio::task::spawn_blocking(move || entered_rx.recv());
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, entered)
        .await
        .expect("startup reconciliation did not reach its hook")
        .unwrap()
        .unwrap();
    // Accept explicit work while startup reconciliation is held.
    let indexer = runtime.clone();
    let options = trellis::indexer::IndexOptions::new(checkout.clone());
    let index = tokio::task::spawn_blocking(move || indexer.cli_index(&options));
    let queued = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok(Some(row)) = Store::open(roots(base.path()), identity(&checkout))
                .unwrap()
                .current_request()
            {
                break row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("index request was not accepted");
    assert_eq!(queued.state, "queued", "no claim before startup commits");
    assert_eq!(scheduler.startup_captures_for_tests(), 0);
    resume_tx.send(()).unwrap();
    let answer = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, index)
        .await
        .expect("index request did not complete")
        .unwrap()
        .unwrap();
    assert!(answer["publishedRevision"].is_object());
    assert_eq!(
        scheduler.startup_captures_for_tests(),
        1,
        "exactly one startup reconciliation ran"
    );
}

/// A settled, idle checkout whose periodic inventory is out of reach: from here
/// only a wake can make its worker do anything (#138 items 1, 9 and 10).
#[cfg(feature = "test-support")]
async fn idle_without_periodic_inventory(
    registry: &mut CheckoutRegistry,
    id: &WorkspaceIdentity,
) -> std::sync::Arc<trellis::daemon::registry::CheckoutRuntime> {
    registry.attach_launch(1, id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    assert!(scheduler.delay_timed_work_for_tests());
    // Synchronize on the worker blocking with no wake pending and no deadline
    // within reach of this test.
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Some(deadline) = scheduler.worker_asleep_until_for_tests()
                && deadline.is_none_or(|at| {
                    at > std::time::Instant::now() + trellis::testing::HANG_BACKSTOP * 2
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("worker did not go idle");
    runtime
}

/// Decision 0009 §8: a durable enqueue through any Store instance wakes the idle
/// worker; it never waits for a poll or the periodic inventory.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn enqueue_from_another_store_wakes_the_idle_worker() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    let _runtime = idle_without_periodic_inventory(&mut registry, &id).await;
    let other = Store::open(roots(base.path()), identity(&checkout)).unwrap();
    let accepted = other
        .enqueue_request(&trellis::indexer::IndexOptions::new(checkout.clone()), None)
        .unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while other
            .request_by_id(&accepted.id)
            .unwrap()
            .unwrap()
            .finished_at
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("enqueue did not wake the idle worker");
    assert_eq!(
        other.request_by_id(&accepted.id).unwrap().unwrap().state,
        "done"
    );
}

/// Decision 0009 §8: watcher ingress wakes the idle worker, which reconciles the
/// change at its debounce deadline, not at the next periodic inventory.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn watcher_signal_wakes_the_idle_worker() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function before() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    let runtime = idle_without_periodic_inventory(&mut registry, &id).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    fs::write(checkout.join("a.js"), "function after() {}\n").unwrap();
    // The same bounded ingress as the OS callback (which may also deliver).
    assert!(
        scheduler.submit_watch_event_for_tests(Ok(notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Any
        ))
        .add_path(checkout.join("a.js"))))
    );
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((response, _)) = runtime.evidence_response() {
                let source = response.source_at("a.js", None).unwrap().unwrap().1;
                response.finish(()).unwrap();
                if source.text.contains("after") {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher signal did not wake the idle worker");
}

/// A settled checkout with no clients: ready to be released.
#[cfg(feature = "test-support")]
async fn idle_detached(
    registry: &mut CheckoutRegistry,
    id: &WorkspaceIdentity,
) -> std::sync::Arc<trellis::daemon::registry::CheckoutRuntime> {
    registry.attach_launch(1, id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    registry.disconnect(1);
    runtime
}

/// Wait for a test hook on the worker thread without blocking the runtime.
#[cfg(feature = "test-support")]
async fn entered(rx: std::sync::mpsc::Receiver<()>, what: &str) {
    tokio::time::timeout(
        trellis::testing::HANG_BACKSTOP,
        tokio::task::spawn_blocking(move || rx.recv()),
    )
    .await
    .unwrap_or_else(|_| panic!("{what} not reached"))
    .unwrap()
    .unwrap();
}

/// Decision 0009 §5: release stamps a watcher cutoff. Changes accepted after
/// it never hold the retirement; the next activation reconciles them.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn release_is_not_held_by_watcher_changes_after_its_cutoff() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    let runtime = idle_detached(&mut registry, &id).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let (held_tx, held_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel::<()>(1);
    scheduler.set_retire_hook_for_tests(move || {
        held_tx.send(()).unwrap();
        let _ = resume_rx.recv();
    });
    assert!(!registry.release(&id.root_key).unwrap());
    entered(held_rx, "retirement hook").await;
    // Changes after the cutoff, while the retirement is held.
    fs::write(checkout.join("late.js"), "function late() {}\n").unwrap();
    assert!(
        scheduler.submit_watch_event_for_tests(Ok(notify::Event::new(notify::EventKind::Create(
            notify::event::CreateKind::File
        ))
        .add_path(checkout.join("late.js"))))
    );
    assert!(scheduler.checkout_watch_pending());
    resume_tx.send(()).unwrap();
    release_when_idle(&mut registry, &id.root_key).await;
    assert!(!runtime.has_active_resources());
}

/// Accepted FIFO work is never skipped: a row queued while another runs keeps
/// the checkout until it completes (Decision 0009 §5).
#[cfg(feature = "test-support")]
#[tokio::test]
async fn release_waits_for_accepted_fifo_work() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    let runtime = idle_detached(&mut registry, &id).await;
    let (claimed_tx, claimed_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel::<()>(1);
    runtime.set_after_claim_hook_for_tests(move || {
        claimed_tx.send(()).unwrap();
        let _ = resume_rx.recv();
    });
    let store = Store::open(roots(base.path()), identity(&checkout)).unwrap();
    let options = trellis::indexer::IndexOptions::new(checkout.clone());
    let first = store.enqueue_request(&options, None).unwrap();
    entered(claimed_rx, "first claim").await;
    let second = store.enqueue_request(&options, None).unwrap();
    assert!(
        !registry.release(&id.root_key).unwrap(),
        "accepted FIFO work keeps the checkout"
    );
    resume_tx.send(()).unwrap();
    release_when_idle(&mut registry, &id.root_key).await;
    for row in [first, second] {
        assert_eq!(store.request_by_id(&row.id).unwrap().unwrap().state, "done");
    }
}

/// A lease taken while retirement is requested cancels it with the runtime's
/// resources still live (Decision 0009 §5).
#[cfg(feature = "test-support")]
#[tokio::test]
async fn read_lease_cancels_requested_retirement() {
    use trellis::http::RuntimePhase;
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    let runtime = idle_detached(&mut registry, &id).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let (held_tx, held_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel::<()>(1);
    scheduler.set_retire_hook_for_tests(move || {
        held_tx.send(()).unwrap();
        let _ = resume_rx.recv();
    });
    assert!(!registry.release(&id.root_key).unwrap());
    entered(held_rx, "retirement hook").await;
    assert_eq!(scheduler.runtime_phase(), RuntimePhase::RetireRequested);
    // A read during the requested retirement is served from the live head.
    let (read, _) = runtime.evidence_response().unwrap();
    assert!(read.source_at("a.js", None).unwrap().is_some());
    read.finish(()).unwrap();
    drop(read);
    assert_eq!(scheduler.runtime_phase(), RuntimePhase::Ready);
    resume_tx.send(()).unwrap();
    // The worker abandons the cancelled retirement and goes idle.
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while scheduler.worker_asleep_until_for_tests().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(scheduler.runtime_phase(), RuntimePhase::Ready);
    assert!(runtime.has_active_resources());
    assert!(runtime.evidence_response().is_ok());
}

/// From `Closing` on nothing is admitted on the old runtime; a reattach waits
/// for `Retired` and gets a fresh runtime (Decision 0009 §5).
#[cfg(feature = "test-support")]
#[tokio::test]
async fn closing_runtime_is_never_revived() {
    use trellis::http::RuntimePhase;
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    let runtime = idle_detached(&mut registry, &id).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let (held_tx, held_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel::<()>(1);
    scheduler.set_closing_hook_for_tests(move || {
        held_tx.send(()).unwrap();
        let _ = resume_rx.recv();
    });
    assert!(!registry.release(&id.root_key).unwrap());
    entered(held_rx, "closing hook").await;
    assert_eq!(scheduler.runtime_phase(), RuntimePhase::Closing);
    let refused = runtime
        .evidence_response()
        .err()
        .expect("closing admits nothing");
    assert!(refused.to_string().contains("released"), "{refused:#}");
    assert_eq!(scheduler.runtime_phase(), RuntimePhase::Closing);
    resume_tx.send(()).unwrap();
    registry.attach_launch(2, &id).unwrap();
    let fresh = registry.activate(&id.root_key).unwrap();
    assert!(!std::sync::Arc::ptr_eq(&fresh, &runtime));
    assert!(!runtime.has_active_resources());
    ready(&fresh).await;
}

/// Maintenance is the lowest class (Decision 0009 §4): when the activated
/// checkout worker has both a due release and an accepted FIFO row, the
/// terminal FIFO COMMIT precedes its first maintenance unit.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn maintenance_never_runs_ahead_of_accepted_fifo_work() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let store = Store::open(roots(base.path()), identity(&checkout)).unwrap();
    let options = trellis::indexer::IndexOptions::new(checkout.clone());
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Keep the activated worker at its gate while creating genuine retention
    // debt. This store's injected clock dates only these two publications;
    // the checkout worker still owns the real queue and maintenance lanes.
    let held = scheduler.hold_worker_for_tests();
    store.set_retention_clock_for_tests(1_000, 0);
    let owner = scheduler.checkout_serving_owner().unwrap();
    let publish = |expected| {
        let (graph, native, capture) =
            trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
                .unwrap();
        store
            .publish_native(
                &graph,
                &capture,
                &native,
                owner.writer_guard().unwrap(),
                expected,
                &cancel,
            )
            .unwrap()
    };
    let old = publish(store.index_baseline().unwrap());
    publish(old);
    let revision_id = format!("pin:v1:{}:{}", old.index_generation, old.index_revision);
    let state = || store.retention_state_for_tests(&revision_id).unwrap();
    let (before, pending_before) = state();
    assert_eq!(pending_before, (0, 0));
    assert_eq!(before.0, "retained");
    assert_eq!(before.1, 0);
    assert!(before.2 > 0);

    let terminal_before = store.terminal_generation();
    let row = store.enqueue_request(&options, None).unwrap();
    let (seen_tx, seen_rx) = std::sync::mpsc::sync_channel(1);
    let observer = store.clone();
    let row_id = row.id.clone();
    let revision_for_hook = revision_id.clone();
    scheduler.on_next_maintenance_unit_for_tests(move || {
        seen_tx
            .send((
                observer.request_by_id(&row_id).unwrap().unwrap().state,
                observer.terminal_generation(),
                observer
                    .retention_state_for_tests(&revision_for_hook)
                    .unwrap(),
            ))
            .unwrap();
    });
    assert_eq!(
        store.request_by_id(&row.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(state(), (before.clone(), pending_before));
    drop(held);

    let (request_state, terminal_at_unit, state_at_unit) = tokio::time::timeout(
        trellis::testing::HANG_BACKSTOP,
        tokio::task::spawn_blocking(move || seen_rx.recv()),
    )
    .await
    .expect("activated worker did not reach maintenance")
    .unwrap()
    .unwrap();
    assert_eq!(request_state, "done", "maintenance preceded FIFO COMMIT");
    assert!(
        terminal_at_unit > terminal_before,
        "FIFO terminal COMMIT was not notified"
    );
    assert_eq!(state_at_unit, (before, pending_before));
    // Wait for the terminal state of this debt, not a transient one. Two due
    // revisions (the activation head and `old`, both stamped at 1_000) take two
    // units each, and the worker may finish all four between two polls, so the
    // global (1, 1) pending state is never guaranteed to be observable. Poll at
    // a modest interval: maintenance defers on any SQLite contention, and
    // back-to-back reads from this process could starve its COMMIT.
    let released = (("released".to_string(), 0, 0), (0, 0));
    let mut last = state();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while last != released {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            last = state();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("due retention did not complete after FIFO drained: {last:?}"));
    assert_eq!(store.request_by_id(&row.id).unwrap().unwrap().state, "done");
}

/// Decision 0009 §8: an index waiter records the terminal-commit count, re-reads
/// its row and sleeps until a confirmed terminal COMMIT, never polling.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn cli_index_wakes_on_its_terminal_commit() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    // The request is accepted while the worker is held, so the waiter is
    // already asleep on the terminal count when the worker completes it.
    let (held_tx, held_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let holder = std::thread::spawn(move || {
        let _held = scheduler.hold_worker_for_tests();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv();
    });
    entered(held_rx, "worker hold").await;
    let indexer = runtime.clone();
    let options = trellis::indexer::IndexOptions::new(checkout.clone());
    let index = tokio::task::spawn_blocking(move || indexer.cli_index(&options));
    let store = Store::open(roots(base.path()), identity(&checkout)).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while store.current_request().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("request was not accepted");
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    let answer = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, index)
        .await
        .expect("index waiter was not woken by its terminal commit")
        .unwrap()
        .unwrap();
    assert!(answer["publishedRevision"].is_object());
}

/// #138 item 16: a reader waiting for startup reconciliation is woken by the
/// runtime state change when it commits, not by a poll.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn read_waiter_wakes_when_startup_reconciliation_commits() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry.attach_launch(1, &id).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        let _ = resume_rx.lock().unwrap().recv();
    }));
    let runtime = registry.activate(&id.root_key).unwrap();
    entered(entered_rx, "startup reconciliation").await;
    let seen = runtime.state_version();
    let waiter = runtime.clone();
    let woken = tokio::task::spawn_blocking(move || {
        waiter.wait_state_change(seen, trellis::testing::HANG_BACKSTOP * 2);
        waiter.state_version() != seen
    });
    resume_tx.send(()).unwrap();
    assert!(
        tokio::time::timeout(trellis::testing::HANG_BACKSTOP, woken)
            .await
            .expect("state waiter was not woken")
            .unwrap()
    );
    ready(&runtime).await;
}

/// Hold the worker at its next step on a separate thread (a guard cannot be
/// held across an await); dropping the returned sender releases it.
#[cfg(feature = "test-support")]
async fn hold_worker(
    scheduler: std::sync::Arc<trellis::http::DaemonState>,
) -> (std::sync::mpsc::SyncSender<()>, std::thread::JoinHandle<()>) {
    let (held_tx, held_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let holder = std::thread::spawn(move || {
        let _held = scheduler.hold_worker_for_tests();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv();
    });
    entered(held_rx, "worker hold").await;
    (release_tx, holder)
}

/// Wait until a runtime is fully retired.
#[cfg(feature = "test-support")]
async fn retired(scheduler: &trellis::http::DaemonState) {
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while scheduler.runtime_phase() != trellis::http::RuntimePhase::Retired {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("runtime did not retire");
}

/// Root loss detected before the replacement is tried (inotify's usual
/// order): a stale browser lease no longer blocks the replacement, but a read
/// still in flight on the old root does, until it finishes.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn detected_root_loss_with_a_held_read_fences_browser_replacement() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function oldBrowser() {}\n").unwrap();
    let old_id = identity(&checkout);
    let key = old_id.root_key.clone();
    let now = std::time::Instant::now();
    let mut registry = CheckoutRegistry::with_roots_at(roots(base.path()), now);
    registry
        .register(&old_id, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    registry.browser_request_at(&old_id, now).unwrap();
    let old = registry.activate(&key).unwrap();
    ready(&old).await;
    let scheduler = old.browser_scheduler().unwrap();
    let (held, _) = old.evidence_response().unwrap();
    fs::rename(&checkout, base.path().join("old")).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function newBrowser() {}\n").unwrap();
    // Either the OS watcher or this synthetic event reports the loss first;
    // if the watcher already retired, the event has nowhere to go.
    let _ = scheduler.submit_watch_event_for_tests(Ok(notify::Event::new(
        notify::EventKind::Remove(notify::event::RemoveKind::Any),
    )));
    retired(&scheduler).await;
    assert!(
        registry.browser_identity(&key).is_err(),
        "a held old-root read fences replacement"
    );
    drop(held);
    let discovered = registry
        .browser_identity(&key)
        .expect("the stale browser lease does not block a replacement root");
    let new_id = identity(&checkout);
    assert_eq!(
        (discovered.device, discovered.inode),
        (new_id.device, new_id.inode)
    );
}

/// A stale client's activation after root-loss retirement drops the old
/// runtime, but not its read fence: a read admitted before the loss still
/// blocks a replacement root until it finishes.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn a_stale_activation_keeps_the_held_read_fence() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function oldBrowser() {}\n").unwrap();
    let old_id = identity(&checkout);
    let key = old_id.root_key.clone();
    let now = std::time::Instant::now();
    let mut registry = CheckoutRegistry::with_roots_at(roots(base.path()), now);
    registry
        .register(&old_id, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    registry.browser_request_at(&old_id, now).unwrap();
    let old = registry.activate(&key).unwrap();
    ready(&old).await;
    let scheduler = old.browser_scheduler().unwrap();
    let (held, _) = old.evidence_response().unwrap();
    fs::rename(&checkout, base.path().join("old")).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function newBrowser() {}\n").unwrap();
    // Either the OS watcher or this synthetic event reports the loss first;
    // if the watcher already retired, the event has nowhere to go.
    let _ = scheduler.submit_watch_event_for_tests(Ok(notify::Event::new(
        notify::EventKind::Remove(notify::event::RemoveKind::Any),
    )));
    retired(&scheduler).await;
    // The stale browser client activates again: the retired runtime is
    // dropped (the activation itself cannot succeed on the lost root).
    let _ = registry.activate(&key);
    assert!(
        registry.browser_identity(&key).is_err(),
        "the held old-root read no longer fences replacement"
    );
    assert_ne!(
        registry
            .browser_checkouts()
            .iter()
            .find(|row| row["rootKey"] == key)
            .unwrap()["state"],
        "available",
        "a replacement is listed while the old read is held"
    );
    drop(held);
    let discovered = registry
        .browser_identity(&key)
        .expect("the replacement attaches once the read finished");
    let new_id = identity(&checkout);
    assert_eq!(
        (discovered.device, discovered.inode),
        (new_id.device, new_id.inode)
    );
}

/// Decision 0009 §2: after old-root settlement the runtime closes at once,
/// even with a client attached; its reads never serve old-root evidence, and
/// the path gets a fresh runtime for the replacement root.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn settled_root_loss_closes_with_a_client_attached() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function oldRoot() {}\n").unwrap();
    let old_id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry.attach_launch(1, &old_id).unwrap();
    let old = registry.activate(&old_id.root_key).unwrap();
    ready(&old).await;
    let scheduler = old.browser_scheduler().unwrap();
    fs::rename(&checkout, base.path().join("moved")).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function newRoot() {}\n").unwrap();
    // A watcher signal wakes the worker, which observes the loss.
    // Either the OS watcher or this synthetic event reports the loss first;
    // if the watcher already retired, the event has nowhere to go.
    let _ = scheduler.submit_watch_event_for_tests(Ok(notify::Event::new(
        notify::EventKind::Remove(notify::event::RemoveKind::Any),
    )));
    retired(&scheduler).await;
    let refused = old
        .evidence_response()
        .err()
        .expect("old root serves nothing");
    assert!(refused.to_string().contains("root_changed"), "{refused:#}");
    // The stale client stays attached; the replacement root attaches anyway.
    let new_id = identity(&checkout);
    registry.attach_launch(2, &new_id).unwrap();
    let fresh = registry.activate(&new_id.root_key).unwrap();
    assert!(!std::sync::Arc::ptr_eq(&fresh, &old));
    ready(&fresh).await;
    let (read, _) = fresh.evidence_response().unwrap();
    let (_, source) = read.source_at("a.js", None).unwrap().unwrap();
    assert!(source.text.contains("newRoot"));
    read.finish(()).unwrap();
    // The stale handle still serves no old-root evidence.
    assert!(
        old.evidence_response()
            .err()
            .is_some_and(|error| error.to_string().contains("root_changed"))
    );
}

/// Decision 0009 §5: an enqueue COMMIT through another Store after
/// retirement's FIFO check, before Closing, cancels the retirement.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn enqueue_from_another_store_before_closing_cancels_retirement() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    let runtime = idle_detached(&mut registry, &id).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let store = Store::open(roots(base.path()), identity(&checkout)).unwrap();
    let (row_tx, row_rx) = std::sync::mpsc::sync_channel(1);
    let enqueuer = store.clone();
    let options = trellis::indexer::IndexOptions::new(checkout.clone());
    scheduler.set_retire_hook_for_tests(move || {
        row_tx
            .send(enqueuer.enqueue_request(&options, None).unwrap())
            .unwrap();
    });
    assert!(!registry.release(&id.root_key).unwrap());
    let row = tokio::task::spawn_blocking(move || row_rx.recv().unwrap())
        .await
        .unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while store.request_by_id(&row.id).unwrap().unwrap().state != "done" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the accepted row was stranded by retirement");
    assert!(runtime.has_active_resources());
}

/// Decision 0009 §2: a root replaced after the worker step's own root check,
/// as retirement begins, still settles the old root's accepted rows with a
/// terminal COMMIT and marks the loss before the runtime closes.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn root_replaced_as_retirement_begins_settles_before_closing() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let db_path = roots(base.path()).requests_db(&id);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    let runtime = idle_detached(&mut registry, &id).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let enqueuer = Store::open(roots(base.path()), identity(&checkout)).unwrap();
    let (row_tx, row_rx) = std::sync::mpsc::sync_channel(1);
    let options = trellis::indexer::IndexOptions::new(checkout.clone());
    let (work, moved) = (checkout.clone(), base.path().join("moved"));
    // On the worker, past the step's root check: accept an old-root row,
    // then replace the root before retirement checks it.
    scheduler.set_retirement_entered_hook_for_tests(move || {
        let row = enqueuer.enqueue_request(&options, None).unwrap();
        fs::rename(&work, &moved).unwrap();
        fs::create_dir(&work).unwrap();
        fs::write(work.join("a.js"), "function replacement() {}\n").unwrap();
        row_tx.send(row).unwrap();
    });
    assert!(!registry.release(&id.root_key).unwrap());
    let row = tokio::task::spawn_blocking(move || row_rx.recv().unwrap())
        .await
        .unwrap();
    retired(&scheduler).await;
    // The old-root Store refuses reads once its root changed; read the queue.
    let (state, error): (String, Option<String>) = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT state,error_code FROM requests WHERE id=?1",
            [&row.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (state.as_str(), error.as_deref()),
        ("failed", Some("root_changed")),
        "the old-root row was not settled before closing"
    );
    assert!(scheduler.checkout_root_loss_retired());
}

/// Decision 0009 §4: the index keeps its use lock until Closing completes, so
/// GC cannot take it exclusively while the watcher and witnesses close.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn closing_runtime_keeps_index_use_until_closed() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let topology = roots(base.path());
    let mut registry = CheckoutRegistry::with_roots(topology.clone());
    let runtime = idle_detached(&mut registry, &id).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let (seen_tx, seen_rx) = std::sync::mpsc::sync_channel(1);
    let probe_roots = topology.clone();
    let probe_id = identity(&checkout);
    scheduler.set_closing_hook_for_tests(move || {
        let exclusive = probe_roots.index_use_exclusive_existing(&probe_id).is_ok();
        seen_tx.send(exclusive).unwrap();
    });
    drop(runtime);
    assert!(!registry.release(&id.root_key).unwrap());
    let exclusive = tokio::task::spawn_blocking(move || seen_rx.recv().unwrap())
        .await
        .unwrap();
    assert!(!exclusive, "GC could take the index exclusively mid-close");
    retired(&scheduler).await;
}

/// Decision 0009 §5 (owner-approved one attempt): a change accepted after the
/// cutoff, before the attempt, is neither captured nor acknowledged by it.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn retirement_attempt_never_captures_past_its_cutoff() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    let runtime = idle_detached(&mut registry, &id).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    // Intent pending at the cutoff, so the attempt has work to do.
    let (release_worker, holder) = hold_worker(scheduler.clone()).await;
    fs::write(checkout.join("a.js"), "function a2() {}\n").unwrap();
    assert!(
        scheduler.submit_watch_event_for_tests(Ok(notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Any
        ))
        .add_path(checkout.join("a.js"))))
    );
    assert!(!registry.release(&id.root_key).unwrap());
    let late_path = checkout.join("late.js");
    let late_scheduler = scheduler.clone();
    scheduler.set_retire_hook_for_tests(move || {
        fs::write(&late_path, "function late() {}\n").unwrap();
        assert!(
            late_scheduler.submit_watch_event_for_tests(Ok(notify::Event::new(
                notify::EventKind::Create(notify::event::CreateKind::File)
            )
            .add_path(late_path.clone())))
        );
    });
    release_worker.send(()).unwrap();
    holder.join().unwrap();
    retired(&scheduler).await;
    drop(runtime);
    let store = Store::open(roots(base.path()), identity(&checkout)).unwrap();
    assert!(
        store.source_at("late.js", None).unwrap().is_none(),
        "the retirement attempt captured a change after its cutoff"
    );
}

/// Decision 0009 §6: with no live runtime, serve still builds the whole
/// configuration before acknowledging; one that cannot build is refused.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn serve_without_a_live_runtime_refuses_an_unbuildable_configuration() {
    use trellis::daemon::BrowserProvisioner;
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    let identity = identity(&checkout);
    let key = identity.root_key.clone();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(CheckoutRegistry::with_roots(
        roots(base.path()),
    )));
    let read_only = base.path().join("read-only");
    fs::create_dir(&read_only).unwrap();
    fs::set_permissions(
        &read_only,
        std::os::unix::fs::PermissionsExt::from_mode(0o000),
    )
    .unwrap();
    let token = base.path().join("token");
    let refused = BrowserProvisioner::new()
        .register_serve(
            &registry,
            &identity,
            CheckoutOptions(serde_json::json!({
                "browseRoot": read_only,
            })),
            "127.0.0.1:0".parse().unwrap(),
            &token,
        )
        .await;
    fs::set_permissions(
        &read_only,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    assert!(
        refused.is_err(),
        "an unbuildable configuration was acknowledged"
    );
    assert!(registry.lock().await.registration(&key).is_none());
    assert!(!token.exists());
}

/// Decision 0009 §8: a completion COMMIT that is durable but reported as an
/// error is confirmed later by a re-read, and that confirmation wakes the
/// index waiter.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn ambiguous_completion_confirmed_later_wakes_the_index_waiter() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let store = Store::open(roots(base.path()), identity(&checkout)).unwrap();
    runtime.fail_next_completion_after_commit_for_tests();
    let (release_worker, holder) = hold_worker(scheduler).await;
    let indexer = runtime.clone();
    let options = trellis::indexer::IndexOptions::new(checkout.clone());
    let index = tokio::task::spawn_blocking(move || indexer.cli_index(&options));
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while store.current_request().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("request was not accepted");
    release_worker.send(()).unwrap();
    holder.join().unwrap();
    let answer = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, index)
        .await
        .expect("the waiter was stranded by an ambiguous completion COMMIT")
        .unwrap()
        .unwrap();
    assert!(answer["publishedRevision"].is_object());
}

/// Decision 0009 §5: an enqueue that has COMMITted but not yet run its wake
/// when retirement reaches Closing still cancels the retirement.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn enqueue_committed_before_its_wake_cancels_retirement() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    let runtime = idle_detached(&mut registry, &id).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let store = Store::open(roots(base.path()), identity(&checkout)).unwrap();
    let (committed_tx, committed_rx) = std::sync::mpsc::sync_channel(1);
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel::<()>(1);
    store.set_after_enqueue_commit_hook_for_tests(move || {
        committed_tx.send(()).unwrap();
        let _ = wake_rx.recv();
    });
    let (row_tx, row_rx) = std::sync::mpsc::sync_channel(1);
    let enqueuer = store.clone();
    let options = trellis::indexer::IndexOptions::new(checkout.clone());
    scheduler.set_retire_hook_for_tests(move || {
        // COMMIT now, after the FIFO check; the wake waits past Closing.
        std::thread::spawn(move || {
            row_tx
                .send(enqueuer.enqueue_request(&options, None).unwrap())
                .unwrap();
        });
        committed_rx.recv().unwrap();
    });
    assert!(!registry.release(&id.root_key).unwrap());
    // Once the worker's next step is free, it has passed its Closing gate.
    let (release_worker, holder) = hold_worker(scheduler.clone()).await;
    release_worker.send(()).unwrap();
    holder.join().unwrap();
    wake_tx.send(()).unwrap();
    let row = tokio::task::spawn_blocking(move || row_rx.recv().unwrap())
        .await
        .unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while store.request_by_id(&row.id).unwrap().unwrap().state != "done" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a committed row was stranded by retirement");
    assert!(runtime.has_active_resources());
}

/// Decision 0009 §6: a serve whose options are already recorded still builds
/// the configuration before acknowledging.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn serve_with_already_recorded_options_still_builds() {
    use trellis::daemon::BrowserProvisioner;
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    let identity = identity(&checkout);
    let read_only = base.path().join("read-only");
    fs::create_dir(&read_only).unwrap();
    fs::set_permissions(
        &read_only,
        std::os::unix::fs::PermissionsExt::from_mode(0o000),
    )
    .unwrap();
    let options = CheckoutOptions(serde_json::json!({
        "browseRoot": read_only,
    }));
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    registry.register(&identity, options.clone()).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let token = base.path().join("token");
    let refused = BrowserProvisioner::new()
        .register_serve(
            &registry,
            &identity,
            options,
            "127.0.0.1:0".parse().unwrap(),
            &token,
        )
        .await;
    fs::set_permissions(
        &read_only,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    assert!(
        refused.is_err(),
        "an unbuildable configuration was acknowledged"
    );
    assert!(!token.exists());
}

/// Decision 0009 §6: explicit serve options are applied once. After their
/// full inventory, a FIFO request with other options becomes the recorded
/// head, and later watcher inventories follow it, not the serve options.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn explicit_serve_options_apply_once() {
    use trellis::daemon::BrowserProvisioner;
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let identity = identity(&checkout);
    let key = identity.root_key.clone();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(CheckoutRegistry::with_roots(
        roots(base.path()),
    )));
    let runtime = {
        let mut checked = registry.lock().await;
        checked.attach_launch(41, &identity).unwrap();
        checked.activate(&key).unwrap()
    };
    ready(&runtime).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let store = Store::open(roots(base.path()), crate::identity(&checkout)).unwrap();
    let recorded = |bytes: u64| {
        let store = store.clone();
        async move {
            tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
                while !store
                    .recorded_index_options()
                    .unwrap()
                    .is_some_and(|options| options.max_file_bytes == bytes)
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("recorded options never became {bytes}"));
        }
    };
    BrowserProvisioner::new()
        .register_serve(
            &registry,
            &identity,
            CheckoutOptions(serde_json::json!({"maxFileBytes": 4096})),
            "127.0.0.1:0".parse().unwrap(),
            &base.path().join("token"),
        )
        .await
        .unwrap();
    recorded(4096).await;
    let mut fifo = trellis::indexer::IndexOptions::new(checkout.clone());
    fifo.max_file_bytes = 8192;
    let row = store.enqueue_request(&fifo, None).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while store.request_by_id(&row.id).unwrap().unwrap().state != "done" {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    recorded(8192).await;
    // A later watcher inventory follows the recorded head.
    fs::write(checkout.join("b.js"), "function b() {}\n").unwrap();
    assert!(
        scheduler.submit_watch_event_for_tests(Ok(notify::Event::new(notify::EventKind::Create(
            notify::event::CreateKind::File
        ))
        .add_path(checkout.join("b.js"))))
    );
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while store.source_at("b.js", None).unwrap().is_none() || scheduler.checkout_watch_pending()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the watcher did not publish b.js");
    assert_eq!(
        store
            .recorded_index_options()
            .unwrap()
            .unwrap()
            .max_file_bytes,
        8192,
        "the watcher reverted to the earlier serve options"
    );
}

/// Decision 0009 §5 (owner-approved): an enqueue through another Store that
/// starts after Closing does not stop the close; it is durable and runs at
/// the checkout's next activation.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn enqueue_after_closing_runs_at_next_activation() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function a() {}\n").unwrap();
    let id = identity(&checkout);
    let mut registry = CheckoutRegistry::with_roots(roots(base.path()));
    let runtime = idle_detached(&mut registry, &id).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let store = Store::open(roots(base.path()), identity(&checkout)).unwrap();
    let (row_tx, row_rx) = std::sync::mpsc::sync_channel(1);
    let enqueuer = store.clone();
    let options = trellis::indexer::IndexOptions::new(checkout.clone());
    scheduler.set_closing_hook_for_tests(move || {
        row_tx
            .send(enqueuer.enqueue_request(&options, None).unwrap())
            .unwrap();
    });
    assert!(!registry.release(&id.root_key).unwrap());
    let row = tokio::task::spawn_blocking(move || row_rx.recv().unwrap())
        .await
        .unwrap();
    retired(&scheduler).await;
    assert_eq!(
        store.request_by_id(&row.id).unwrap().unwrap().state,
        "queued"
    );
    release_when_idle(&mut registry, &id.root_key).await;
    registry.attach_launch(2, &id).unwrap();
    let fresh = registry.activate(&id.root_key).unwrap();
    assert!(!std::sync::Arc::ptr_eq(&fresh, &runtime));
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while store.request_by_id(&row.id).unwrap().unwrap().state != "done" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the durable row did not run at the next activation");
}

/// Every request row of a queue as `id:state/error_code`, ordered by seq.
/// Read from requests.db directly: the old-root Store refuses reads once its
/// root changed.
#[cfg(feature = "test-support")]
fn queue_rows(db: &Path) -> Vec<String> {
    let db = rusqlite::Connection::open(db).unwrap();
    let mut statement = db
        .prepare("SELECT id,state,error_code FROM requests ORDER BY seq")
        .unwrap();
    statement
        .query_map([], |row| {
            Ok(format!(
                "{}:{}/{}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?.unwrap_or_default()
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// #122 live replacement: with the daemon's runtime running, the root is
/// replaced at the same path while old-root rows are accepted. The old
/// runtime's confirmed `root_changed` settlement is observed (durably, from a
/// separate connection) at its Closing linearization, strictly before the new
/// root's startup reconciliation (H) begins and before its first FIFO claim.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn live_same_path_replacement_settles_old_rows_before_new_root_startup_and_claim() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function oldRoot() {}\n").unwrap();
    let old_id = identity(&checkout);
    let key = old_id.root_key.clone();
    let topology = roots(base.path());
    let db_path = topology.requests_db(&old_id);
    let mut registry = CheckoutRegistry::with_roots(topology.clone());
    registry.attach_launch(1, &old_id).unwrap();
    let old = registry.activate(&key).unwrap();
    ready(&old).await;
    let old_scheduler = old.browser_scheduler().unwrap();
    // Hold the running worker so both old-root rows are accepted, not
    // claimed, when the root is replaced.
    let (release_hold, holder) = hold_worker(old_scheduler.clone()).await;
    let enqueuer = Store::open(topology.clone(), identity(&checkout)).unwrap();
    let options = trellis::indexer::IndexOptions::new(checkout.clone());
    let old_rows = [
        enqueuer.enqueue_request(&options, None).unwrap().id,
        enqueuer.enqueue_request(&options, None).unwrap().id,
    ];
    drop(enqueuer);
    fs::rename(&checkout, base.path().join("moved")).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("a.js"), "function newRoot() {}\n").unwrap();
    let new_id = identity(&checkout);
    // The old runtime is live and has not settled: the new root cannot
    // attach, so no new-root runtime (and no H) can start yet.
    assert!(registry.attach_launch(2, &new_id).is_err());
    // Ordering witness: each hook records what a separate connection reads
    // from requests.db at that point.
    let witness = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(&str, Vec<String>)>::new()));
    let record = |at: &'static str| {
        let (witness, db_path) = (witness.clone(), db_path.clone());
        move || witness.lock().unwrap().push((at, queue_rows(&db_path)))
    };
    let closing = record("old_closing");
    old_scheduler.set_closing_hook_for_tests(closing);
    drop(release_hold);
    holder.join().unwrap();
    retired(&old_scheduler).await;
    assert!(old_scheduler.checkout_root_loss_retired());
    // The replacement attaches once the old runtime closed for root loss.
    registry.attach_launch(2, &new_id).unwrap();
    let startup = record("new_startup");
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        startup();
        entered_tx.send(()).unwrap();
        let _ = resume_rx.lock().unwrap().recv();
    }));
    let fresh = registry.activate(&key).unwrap();
    assert!(!std::sync::Arc::ptr_eq(&fresh, &old));
    entered(entered_rx, "new-root startup reconciliation").await;
    fresh.set_after_claim_hook_for_tests(record("first_claim"));
    // A new-root client is already waiting while H is held.
    let indexer = fresh.clone();
    let new_options = trellis::indexer::IndexOptions::new(checkout.clone());
    let index = tokio::task::spawn_blocking(move || indexer.cli_index(&new_options));
    let new_row = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let rows = queue_rows(&db_path);
            if rows.len() == 3 {
                break rows[2].split(':').next().unwrap().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("new-root request was not accepted");
    resume_tx.send(()).unwrap();
    let answer = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, index)
        .await
        .expect("new-root request did not complete")
        .unwrap()
        .unwrap();
    assert!(answer["publishedRevision"].is_object());
    let settled: Vec<String> = old_rows
        .iter()
        .map(|id| format!("{id}:failed/root_changed"))
        .collect();
    let witness = witness.lock().unwrap().clone();
    assert_eq!(
        witness,
        vec![
            ("old_closing", settled.clone()),
            ("new_startup", settled.clone()),
            (
                "first_claim",
                [settled, vec![format!("{new_row}:running/")]].concat(),
            ),
        ],
        "old-root settlement must be durable before new-root H and claim"
    );
}

#[cfg(feature = "test-support")]
async fn selected_get(
    app: &axum::Router,
    path: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("host", "127.0.0.1:7331")
                .header(
                    "authorization",
                    "Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// The daemon's browser surface over a shared registry.
#[cfg(feature = "test-support")]
fn browser(registry: &std::sync::Arc<tokio::sync::Mutex<CheckoutRegistry>>) -> axum::Router {
    trellis::http::ProvisionedBrowser::new(
        registry.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap()
    .router()
}

/// Run the daemon's GC lane on `runtime` until the `gone` queue is deleted.
#[cfg(feature = "test-support")]
async fn gc_until_deleted(runtime: &trellis::daemon::registry::CheckoutRuntime, gone: &Path) {
    let scheduler = runtime.browser_scheduler().unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while gone.exists() {
            scheduler.force_retention_idle_tick_for_tests().unwrap();
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("daemon GC did not delete the eligible index");
}

/// Decision 0008 §3, T04: a queue no client activates keeps its accepted row
/// pending until eligible GC deletes the index. After the daemon's GC removes
/// the replaced root's index, a lookup of that ID through the daemon is a
/// typed `not_found`, while the route still answers IDs the new root accepted.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn gc_abandons_unserviced_pending_request_and_later_lookup_is_not_found() {
    let base = tempfile::tempdir().unwrap();
    let (current, lost) = (base.path().join("current"), base.path().join("lost"));
    for (root, text) in [
        (&current, "function current() {}\n"),
        (&lost, "function lost() {}\n"),
    ] {
        fs::create_dir(root).unwrap();
        fs::write(root.join("a.js"), text).unwrap();
    }
    let topology = roots(base.path());
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(CheckoutRegistry::with_roots(
        topology.clone(),
    )));
    let app = browser(&registry);
    let lost_id = identity(&lost);
    let key = lost_id.root_key.clone();
    let queue = topology.requests_db(&lost_id);
    {
        let mut registry = registry.lock().await;
        let runtime = idle_detached(&mut registry, &lost_id).await;
        release_when_idle(&mut registry, &key).await;
        assert!(!runtime.has_active_resources());
    }
    // Accepted durably (as before a daemon crash); no client activates it.
    let store = Store::open(topology.clone(), identity(&lost)).unwrap();
    let pending = store
        .enqueue_request(&trellis::indexer::IndexOptions::new(lost.clone()), None)
        .unwrap();
    drop(store);
    fs::rename(&lost, base.path().join("moved")).unwrap();
    fs::create_dir(&lost).unwrap();
    fs::write(lost.join("a.js"), "function replacement() {}\n").unwrap();
    assert_eq!(queue_rows(&queue), vec![format!("{}:queued/", pending.id)]);
    let gc = {
        let mut registry = registry.lock().await;
        registry.attach_launch(1, &identity(&current)).unwrap();
        registry.activate(&identity(&current).root_key).unwrap()
    };
    ready(&gc).await;
    gc_until_deleted(&gc, &queue).await;
    assert!(!topology.index_dir(&lost_id).exists());
    let (status, body) =
        selected_get(&app, &format!("/api/checkouts/{key}/jobs/{}", pending.id)).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (axum::http::StatusCode::NOT_FOUND, Some("not_found")),
        "{body}"
    );
    // The same route answers an ID accepted by the replacement root.
    let replacement = registry.lock().await.runtime(&key).unwrap();
    let options = trellis::indexer::IndexOptions::new(lost.clone());
    let indexer = replacement.clone();
    tokio::time::timeout(
        trellis::testing::HANG_BACKSTOP,
        tokio::task::spawn_blocking(move || indexer.cli_index(&options)),
    )
    .await
    .expect("replacement index did not complete")
    .unwrap()
    .unwrap();
    let accepted = queue_rows(&queue);
    assert_eq!(accepted.len(), 1, "the abandoned row must not reappear");
    let id = accepted[0].split(':').next().unwrap();
    let (status, body) = selected_get(&app, &format!("/api/checkouts/{key}/jobs/{id}")).await;
    assert_eq!(
        (status, body["state"].as_str()),
        (axum::http::StatusCode::OK, Some("done")),
        "{body}"
    );
}

/// #131: a queue read while the root is missing (moved, not yet replaced) is
/// that root's change. The root's own NotFound is not a vanished requests.db.
#[test]
fn queue_read_during_root_loss_is_root_changed_not_a_vanished_queue() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("lost");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function lost() {}\n").unwrap();
    let store = trellis::store::Store::open_for_tests(&base.path().join("state"), &root).unwrap();
    let accepted = store
        .enqueue_request(&trellis::indexer::IndexOptions::new(root.clone()), None)
        .unwrap();
    assert!(store.request_by_id(&accepted.id).unwrap().is_some());
    fs::rename(&root, base.path().join("moved")).unwrap();
    let error = store
        .request_by_id(&accepted.id)
        .expect_err("a lost root cannot answer its queue");
    assert!(error.to_string().starts_with("root_changed"), "{error:#}");
}

/// Decision 0008 §3, T04: a client already waiting on a request whose root is
/// then lost gets `root_changed`, never `not_found`, even though eligible GC
/// later deletes the index; a fresh lookup after GC gets `not_found`.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn waiting_client_gets_root_changed_for_lost_root_then_gc_lookup_is_not_found() {
    let base = tempfile::tempdir().unwrap();
    let (current, lost) = (base.path().join("current"), base.path().join("lost"));
    for (root, text) in [
        (&current, "function current() {}\n"),
        (&lost, "function lost() {}\n"),
    ] {
        fs::create_dir(root).unwrap();
        fs::write(root.join("a.js"), text).unwrap();
    }
    let topology = roots(base.path());
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(CheckoutRegistry::with_roots(
        topology.clone(),
    )));
    let app = browser(&registry);
    let lost_id = identity(&lost);
    let key = lost_id.root_key.clone();
    let queue = topology.requests_db(&lost_id);
    let runtime = {
        let mut registry = registry.lock().await;
        registry.attach_launch(1, &lost_id).unwrap();
        registry.activate(&key).unwrap()
    };
    ready(&runtime).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let (release_hold, holder) = hold_worker(scheduler.clone()).await;
    let indexer = runtime.clone();
    let options = trellis::indexer::IndexOptions::new(lost.clone());
    let waiter = tokio::task::spawn_blocking(move || indexer.cli_index(&options));
    let id = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let [row] = queue_rows(&queue).as_slice() {
                break row.split(':').next().unwrap().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("request was not accepted");
    assert_eq!(queue_rows(&queue), vec![format!("{id}:queued/")]);
    // The client is waiting on its accepted row when the root is lost.
    fs::rename(&lost, base.path().join("moved")).unwrap();
    fs::create_dir(&lost).unwrap();
    fs::write(lost.join("a.js"), "function replacement() {}\n").unwrap();
    drop(release_hold);
    holder.join().unwrap();
    let error = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, waiter)
        .await
        .expect("waiting client was not answered")
        .unwrap()
        .expect_err("a lost root cannot complete its request");
    assert!(error.to_string().starts_with("root_changed"), "{error:#}");
    retired(&scheduler).await;
    assert_eq!(
        queue_rows(&queue),
        vec![format!("{id}:failed/root_changed")]
    );
    let gc = {
        let mut registry = registry.lock().await;
        registry.disconnect(1);
        release_when_idle(&mut registry, &key).await;
        registry.attach_launch(2, &identity(&current)).unwrap();
        registry.activate(&identity(&current).root_key).unwrap()
    };
    ready(&gc).await;
    gc_until_deleted(&gc, &queue).await;
    let (status, body) = selected_get(&app, &format!("/api/checkouts/{key}/jobs/{id}")).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (axum::http::StatusCode::NOT_FOUND, Some("not_found")),
        "{body}"
    );
}

/// A CLI queue read may pass its first root check before rename, then lose
/// the pathname to legitimate GC before it acquires SH. The protected second
/// root check must preserve the accepted request's root_changed outcome.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn root_loss_between_cli_queue_check_and_shared_use_returns_root_changed() {
    let base = tempfile::tempdir().unwrap();
    let (current, lost) = (base.path().join("current"), base.path().join("lost"));
    for root in [&current, &lost] {
        fs::create_dir(root).unwrap();
        fs::write(root.join("a.js"), "function source() {}\n").unwrap();
    }
    let topology = roots(base.path());
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(CheckoutRegistry::with_roots(
        topology.clone(),
    )));
    let lost_id = identity(&lost);
    let key = lost_id.root_key.clone();
    let queue = topology.requests_db(&lost_id);
    let runtime = {
        let mut registry = registry.lock().await;
        registry.attach_launch(1, &lost_id).unwrap();
        registry.activate(&key).unwrap()
    };
    ready(&runtime).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let (release_hold, holder) = hold_worker(scheduler.clone()).await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_read, held_read) = std::sync::mpsc::sync_channel::<()>(1);
    scheduler.set_queue_root_verify_after_enqueue_for_tests(move || {
        let _ = entered_tx.send(());
        held_read.recv().unwrap();
    });
    let options = trellis::indexer::IndexOptions::new(lost.clone());
    let indexer = runtime.clone();
    let waiter = tokio::task::spawn_blocking(move || indexer.cli_index(&options));
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, entered_rx)
        .await
        .expect("CLI read did not pass its first root check")
        .unwrap();
    let accepted = queue_rows(&queue);
    assert_eq!(accepted.len(), 1);
    let id = accepted[0].split(':').next().unwrap().to_owned();
    assert_eq!(accepted, vec![format!("{id}:queued/")]);
    fs::rename(&lost, base.path().join("moved")).unwrap();
    fs::create_dir(&lost).unwrap();
    fs::write(lost.join("a.js"), "function replacement() {}\n").unwrap();
    drop(release_hold);
    holder.join().unwrap();
    retired(&scheduler).await;
    assert_eq!(
        queue_rows(&queue),
        vec![format!("{id}:failed/root_changed")]
    );
    assert!(
        !waiter.is_finished(),
        "CLI read must remain paused before SH"
    );
    let gc = {
        let mut registry = registry.lock().await;
        registry.disconnect(1);
        release_when_idle(&mut registry, &key).await;
        registry.attach_launch(2, &identity(&current)).unwrap();
        registry.activate(&identity(&current).root_key).unwrap()
    };
    ready(&gc).await;
    gc_until_deleted(&gc, &queue).await;
    release_read.send(()).unwrap();
    let error = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, waiter)
        .await
        .expect("CLI waiter did not leave protected queue read")
        .unwrap()
        .expect_err("old-root request cannot succeed");
    assert!(error.to_string().starts_with("root_changed"), "{error:#}");
}
