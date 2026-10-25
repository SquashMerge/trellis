use super::*;
use crate::capture::SourceOperations;
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, atomic::AtomicBool},
};

#[test]
fn acquired_leader_cannot_claim_its_own_row_at_precommit_capture_pause() {
    use crate::store::topology::IndexNotReady;
    use std::sync::mpsc;
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function old() {}\n").unwrap();
    let original = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let (_, old_owner) = reconcile_workspace(&original, &options, &cancel, |_| {}).unwrap();
    drop(old_owner);
    fs::write(workspace.path().join("a.js"), "function new() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let own = store.enqueue_request(&options, None).unwrap();
    let coordinator = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = coordinator.session();
    let (paused_tx, paused_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let work = scope.spawn(move || {
            coordinator.run_observed(
                &options,
                &cancel,
                |_| {},
                |_| {
                    paused_tx.send(()).unwrap();
                    release_rx
                        .recv_timeout(crate::testing::HANG_BACKSTOP)
                        .unwrap();
                },
            )
        });
        paused_rx
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .unwrap();
        session.verify().unwrap();
        let precommit = store.claim_request(&session).unwrap_err();
        assert!(
            precommit.downcast_ref::<IndexNotReady>().is_some(),
            "an acquired EX without its own committed startup reconciliation cannot claim: {precommit:#}"
        );
        let row = store.request_by_id(&own.id).unwrap().unwrap();
        assert_eq!(row.state, "queued");
        assert!(row.claim_incarnation.is_none());
        release_tx.send(()).unwrap();
        let committed = work.join().unwrap().unwrap();
        assert_eq!(store.status().unwrap().revision, committed);
        let claimed = store.claim_request(&session).unwrap().unwrap();
        assert_eq!(claimed.id, own.id);
        assert_eq!(claimed.state, "running");
        assert_eq!(
            claimed.claim_incarnation,
            Some(session.incarnation().to_string())
        );
    });
}

#[cfg(unix)]
#[test]
fn claimed_request_requeues_same_seq_after_input_alias_drift() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = workspace.path().join("a.js");
    let manifest = workspace.path().join("package.json");
    let alias = workspace.path().join("tsconfig.json");
    fs::write(&source, "function before() {}\n").unwrap();
    fs::write(&manifest, "{\"name\":\"before\"}\n").unwrap();
    fs::hard_link(&manifest, &alias).unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let (prior, owner) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    fs::write(&source, "function middle() {}\n").unwrap();
    let request = store.enqueue_request(&options, None).unwrap();
    let edited = Arc::new(AtomicBool::new(false));
    let edited_hook = edited.clone();
    let manifest_hook = fs::canonicalize(&manifest).unwrap();
    let source_hook = source.clone();
    let first = crate::capture::with_source_read_hook(
        move |path, point| {
            if path == manifest_hook
                && point == "alias"
                && !edited_hook.swap(true, Ordering::AcqRel)
            {
                let replacement = manifest_hook.with_extension("replacement");
                fs::write(&replacement, "{\"name\":\"after\"}\n").unwrap();
                fs::rename(replacement, &manifest_hook).unwrap();
                fs::write(&source_hook, "function after() {}\n").unwrap();
            }
        },
        || drain_requests_observed_with_native(&store, &owner, |_, _| {}, |_, _, _| {}),
    )
    .unwrap();
    assert!(edited.load(Ordering::Acquire));
    assert_eq!(first, 0);
    assert_eq!(store.status().unwrap().revision, prior);
    let pending = store.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(pending.seq, request.seq);
    assert_eq!(pending.state, "queued");
    assert!(pending.error_code.is_none() && pending.claim_incarnation.is_none());
    assert_eq!(
        drain_requests_observed_with_native(&store, &owner, |_, _| {}, |_, _, _| {}).unwrap(),
        1
    );
    let done = store.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(done.seq, request.seq);
    assert_eq!(done.state, "done");
    let response = store.evidence_response().unwrap();
    assert_eq!(
        response
            .source_at("a.js", done.revision)
            .unwrap()
            .unwrap()
            .1
            .text,
        "function after() {}\n"
    );
    response.finish(()).unwrap();
}

#[test]
fn claimed_request_requeues_after_source_input_open_and_read_edits() {
    for phase in ["open", "read"] {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("a.js");
        let manifest = workspace.path().join("package.json");
        fs::write(&source, "function before() {}\n").unwrap();
        fs::write(&manifest, "{\"name\":\"before\"}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let (prior, owner) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
        fs::write(&source, "function middle() {}\n").unwrap();
        let request = store.enqueue_request(&options, None).unwrap();
        let edited = Arc::new(AtomicBool::new(false));
        let edited_hook = edited.clone();
        let manifest_hook = fs::canonicalize(&manifest).unwrap();
        let source_hook = source.clone();
        let first = crate::capture::with_source_read_hook(
            move |path, point| {
                if path == manifest_hook
                    && point == phase
                    && !edited_hook.swap(true, Ordering::AcqRel)
                {
                    let replacement = manifest_hook.with_extension("replacement");
                    fs::write(&replacement, "{\"name\":\"after\"}\n").unwrap();
                    fs::rename(replacement, &manifest_hook).unwrap();
                    fs::write(&source_hook, "function after() {}\n").unwrap();
                }
            },
            || drain_requests_observed_with_native(&store, &owner, |_, _| {}, |_, _, _| {}),
        )
        .unwrap();
        assert!(edited.load(Ordering::Acquire), "hook did not fire: {phase}");
        assert_eq!(first, 0);
        assert_eq!(store.status().unwrap().revision, prior);
        let pending = store.request_by_id(&request.id).unwrap().unwrap();
        assert_eq!(pending.seq, request.seq);
        assert_eq!(pending.state, "queued");
        assert!(pending.claim_incarnation.is_none() && pending.error_code.is_none());
        assert_eq!(
            drain_requests_observed_with_native(&store, &owner, |_, _| {}, |_, _, _| {}).unwrap(),
            1
        );
        let done = store.request_by_id(&request.id).unwrap().unwrap();
        assert_eq!(done.seq, request.seq);
        assert_eq!(done.state, "done");
        let response = store.evidence_response().unwrap();
        assert_eq!(
            response
                .source_at("a.js", done.revision)
                .unwrap()
                .unwrap()
                .1
                .text,
            "function after() {}\n"
        );
        response.finish(()).unwrap();
    }
}

#[test]
fn claimed_request_requeues_same_fifo_row_after_capture_inventory_drift() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = workspace.path().join("a.js");
    fs::write(&source, "function before() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let (prior, owner) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    let request = store.enqueue_request(&options, None).unwrap();
    let edited = AtomicBool::new(false);
    let first = drain_requests_observed_with_native(
        &store,
        &owner,
        |id, phase| {
            if id == request.id && phase.phase == "scan" && !edited.swap(true, Ordering::AcqRel) {
                // Capture has the old source, but its final inventory check
                // must observe this edit and reject that snapshot.
                fs::write(&source, "function after() {}\n").unwrap();
            }
        },
        |_, _, _| {},
    )
    .unwrap();
    assert!(edited.load(Ordering::Acquire));
    assert_eq!(first, 0);
    assert_eq!(store.status().unwrap().revision, prior);
    let pending = store.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(pending.seq, request.seq);
    assert_eq!(pending.state, "queued");
    assert!(pending.claim_incarnation.is_none() && pending.error_code.is_none());
    owner.verify().unwrap();

    let second =
        drain_requests_observed_with_native(&store, &owner, |_, _| {}, |_, _, _| {}).unwrap();
    assert_eq!(second, 1);
    let done = store.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(done.seq, request.seq);
    assert_eq!(done.state, "done");
    let committed = done.revision.unwrap();
    assert_eq!(committed.index_generation, prior.index_generation);
    assert!(committed.index_revision > prior.index_revision);
    let response = store.evidence_response().unwrap();
    assert_eq!(
        response
            .source_at("a.js", Some(committed))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function after() {}\n"
    );
    response.finish(()).unwrap();
}

#[test]
fn takeover_watcher_switches_input_options_with_new_full_wake() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let head_options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let owner = establish_serving_session(&store, Some(&head_options), &cancel).unwrap();
    let mut work = WatcherWork::new(&store, &owner, &head_options).unwrap();
    let mut daemon_options = head_options.clone();
    daemon_options.max_file_bytes = 1024;
    let deadline = std::time::Instant::now() + crate::testing::HANG_BACKSTOP;
    while work.last_accounted_generation.is_none() {
        // Native watcher ingress can invalidate the cutoff of this first
        // inventory. Retry the normal tick until the replacement full wake
        // is acknowledged; never bypass the watch with force=true.
        work.reconcile_due(&store, &owner, &daemon_options, &cancel, false)
            .unwrap();
        assert!(
            std::time::Instant::now() < deadline,
            "new option watcher must finish its first full inventory"
        );
        if work.last_accounted_generation.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    assert_eq!(work.options.max_file_bytes, 1024);
    assert_eq!(
        store
            .recorded_index_options()
            .unwrap()
            .unwrap()
            .max_file_bytes,
        1024
    );
}

#[test]
fn degraded_watcher_reconciles_once_then_waits_for_periodic_scan() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let owner = establish_serving_session(&store, Some(&options), &cancel).unwrap();
    let mut work = WatcherWork::new(&store, &owner, &options).unwrap();
    // A registration failure is sticky. It still needs one verified full
    // inventory, but an accounted idle generation must not veto maintenance.
    let absent_watch_root = workspace.path().join("absent_watch_root");
    assert!(!absent_watch_root.exists());
    work.watch = crate::watch::WatchSignals::publishing(absent_watch_root, work.health.clone());
    assert!(work.watch.degraded());
    assert!(work.accepted_watch_intent(&options));
    assert!(
        work.reconcile_due(&store, &owner, &options, &cancel, false)
            .unwrap()
    );
    let pin = store.status().unwrap().revision;
    assert!(work.last_accounted_generation.is_some());
    assert!(
        !work.accepted_watch_intent(&options),
        "accounted degraded generation must admit maintenance before periodic scan"
    );
    assert!(
        !work
            .reconcile_due(&store, &owner, &options, &cancel, false)
            .unwrap()
    );
    assert_eq!(store.status().unwrap().revision, pin);

    // New accepted ingress preempts before channel drain or debounce.
    work.watch
        .submit_event(Err(notify::Error::generic("watch failed again")));
    assert!(work.accepted_watch_intent(&options));
    assert!(
        work.reconcile_due(&store, &owner, &options, &cancel, false)
            .unwrap()
    );
    assert!(!work.accepted_watch_intent(&options));
    work.watch.require_full();
    assert!(work.accepted_watch_intent(&options));
    assert!(
        work.reconcile_due(&store, &owner, &options, &cancel, false)
            .unwrap()
    );
    assert!(!work.accepted_watch_intent(&options));

    // Due periodic inventory is real work, but retry_after prevents its
    // maintenance veto until the retry deadline has elapsed.
    work.last_inventory -= std::time::Duration::from_secs(60);
    work.retry_after = Some(std::time::Instant::now() + crate::testing::HANG_BACKSTOP);
    assert!(!work.accepted_watch_intent(&options));
    assert!(
        !work
            .reconcile_due(&store, &owner, &options, &cancel, false)
            .unwrap()
    );
    work.retry_after = Some(std::time::Instant::now() - std::time::Duration::from_millis(1));
    assert!(work.accepted_watch_intent(&options));
    assert!(
        work.reconcile_due(&store, &owner, &options, &cancel, false)
            .unwrap()
    );
    assert!(!work.accepted_watch_intent(&options));
}

/// Per-source (opens, complete reads, hashes), keyed by path.
type Counts = BTreeMap<String, (usize, usize, usize)>;

fn counts(capture: &Capture) -> (Vec<String>, Counts) {
    let files = capture.files.iter().map(|f| f.path.clone()).collect();
    let ops = capture
        .source_operations
        .iter()
        .map(
            |(
                path,
                SourceOperations {
                    opens,
                    complete_reads,
                    hashes,
                },
            )| { (path.clone(), (*opens, *complete_reads, *hashes)) },
        )
        .collect();
    (files, ops)
}

#[test]
fn publish_commit_busy_once_must_keep_accepted_fifo_head_and_finish_same_seq() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let mut h = IndexOptions::new(workspace.path().canonicalize().unwrap());
    h.max_file_bytes = 128;
    let mut a_options = h.clone();
    a_options.max_file_bytes = 32;
    let mut b_options = h.clone();
    b_options.max_file_bytes = 512;
    let assert_selected_options = |expected: &IndexOptions| {
        let observed = store.recorded_index_options().unwrap().unwrap();
        assert_eq!(observed.workspace_root, expected.workspace_root);
        assert_eq!(
            crate::indexer::ReconcileOptions::from(&observed),
            crate::indexer::ReconcileOptions::from(expected),
        );
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let (_, old_owner) = reconcile_workspace(&store, &h, &cancel, |_| {}).unwrap();
    drop(old_owner);
    let browser = store.enqueue_request(&a_options, None).unwrap();
    let cli = store.enqueue_request(&b_options, None).unwrap();
    assert!(browser.seq < cli.seq && browser.id != cli.id);
    let leader = establish_serving_session(&store, None, &cancel).unwrap();
    let base = store.status().unwrap().revision;
    assert_selected_options(&h);
    assert_eq!(
        store.request_by_id(&browser.id).unwrap().unwrap().state,
        "queued"
    );
    assert_eq!(
        store.request_by_id(&cli.id).unwrap().unwrap().state,
        "queued"
    );
    let encoded_a =
        serde_json::to_string(&crate::indexer::ReconcileOptions::from(&a_options)).unwrap();
    let encoded_b =
        serde_json::to_string(&crate::indexer::ReconcileOptions::from(&b_options)).unwrap();
    assert_eq!(browser.options_json, encoded_a);
    assert_eq!(cli.options_json, encoded_b);
    let injection = AtomicBool::new(false);
    assert_eq!(
        drain_requests_observed(&store, &leader, |id, phase| {
            if id == cli.id
                && phase.phase == "timing:capture"
                && !injection.swap(true, Ordering::AcqRel)
            {
                store.fail_next_live_publish_commit_busy();
            }
        })
        .unwrap(),
        1
    );
    assert!(
        injection.load(Ordering::Acquire),
        "inject once at Q2's real commit boundary"
    );
    let a = store.request_by_id(&browser.id).unwrap().unwrap();
    let b = store.request_by_id(&cli.id).unwrap().unwrap();
    assert_eq!(a.state, "done");
    assert_eq!(
        (a.id.as_str(), a.seq, a.options_json.as_str()),
        (browser.id.as_str(), browser.seq, encoded_a.as_str())
    );
    assert_eq!(b.id, cli.id);
    assert_eq!(b.options_json, encoded_b);
    assert_selected_options(&a_options);
    assert_eq!(
        b.seq, cli.seq,
        "accepted CLI ACK cannot be deleted/re-admitted under a new seq"
    );
    assert_eq!(
        b.state, "queued",
        "pre-commit storage_busy must return same-seq accepted ACK to queued, not index_failed"
    );
    assert!(b.finished_at.is_none() && b.error_code.is_none() && b.revision.is_none());
    assert_eq!(
        store.index_baseline().unwrap().index_revision,
        base.index_revision + 1,
        "failed commit must leave only the prior Q1 publication"
    );
    let q1_pin = a.revision.unwrap();
    // Selected status is fenced until Q2 reconciliation succeeds.
    // Check Q1's durable DONE/pin and selected A options above; defer
    // the historical source read until after Q2 publishes and recovers.
    fs::write(workspace.path().join("a.js"), "function b() {}\n").unwrap();
    assert_eq!(
        drain_requests(&store, &leader).unwrap(),
        1,
        "same verified leader may reclaim requeued Q2 on next bounded tick"
    );
    let b = store.request_by_id(&cli.id).unwrap().unwrap();
    assert_eq!(
        b.state, "done",
        "retry must finish the original accepted same-seq Q2"
    );
    assert!(b.finished_at.is_some() && b.error_code.is_none() && b.revision.is_some());
    assert_eq!(
        (
            b.id.as_str(),
            b.seq,
            b.options_json.as_str(),
            b.state.as_str()
        ),
        (cli.id.as_str(), cli.seq, encoded_b.as_str(), "done")
    );
    let final_q1 = store.request_by_id(&browser.id).unwrap().unwrap();
    assert_eq!(
        (
            final_q1.id.as_str(),
            final_q1.seq,
            final_q1.options_json.as_str(),
            final_q1.state.as_str(),
            final_q1.revision,
        ),
        (
            browser.id.as_str(),
            browser.seq,
            encoded_a.as_str(),
            "done",
            Some(q1_pin)
        ),
        "Q2 retry cannot move Q1 out of its exact terminal DONE/pin",
    );
    assert!(final_q1.finished_at.is_some() && final_q1.error_code.is_none());
    assert_selected_options(&b_options);
    assert_eq!(a.revision.unwrap().index_generation, base.index_generation);
    assert_eq!(a.revision.unwrap().index_revision, base.index_revision + 1);
    assert_eq!(
        b.revision.unwrap().index_revision,
        a.revision.unwrap().index_revision + 1,
        "failed commit cannot leave a partially published revision"
    );
    assert_eq!(store.status().unwrap().revision, b.revision.unwrap());
    let read = store.evidence_response().unwrap();
    read.validate_pin(base).unwrap();
    read.validate_pin(q1_pin).unwrap();
    read.validate_pin(b.revision.unwrap()).unwrap();
    assert_eq!(
        read.source_at("a.js", Some(q1_pin))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function a() {}\n"
    );
    assert_eq!(
        read.source_at("a.js", Some(b.revision.unwrap()))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function b() {}\n"
    );
    read.finish(()).unwrap();
    assert!(leader.verify().is_ok());
}

#[test]
fn cached_completion_retry_only_accepts_real_lock_contention_not_claim_invariants() {
    assert!(retryable_completion_error(&anyhow::anyhow!(
        "storage_busy: SQLite lock contention"
    )));
    assert!(!retryable_completion_error(&anyhow::anyhow!(
        "storage_busy: cached claim changed"
    )));
    assert!(!retryable_completion_error(&anyhow::anyhow!(
        "storage_busy: unresolved FIFO completion"
    )));
}

#[test]
fn ambiguous_post_commit_busy_must_leave_running_for_verified_reconciliation() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let (_, old) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    drop(old);
    let mut options = options;
    options.max_file_bytes = 1024;
    let ack = store.enqueue_request(&options, None).unwrap();
    let (before, leader) = reconcile_workspace(
        &store,
        &IndexOptions::new(workspace.path().to_owned()),
        &cancel,
        |_| {},
    )
    .unwrap();
    store.fail_next_live_publish_post_commit_busy();
    let error = drain_requests(&store, &leader).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("publication changed across failed commit"),
        "{error:#}"
    );
    let row = store.request_by_id(&ack.id).unwrap().unwrap();
    assert_eq!(
        row.state, "running",
        "unknown result must not be requeued or failed"
    );
    assert_eq!(row.seq, ack.seq);
    assert!(row.error_code.is_none() && row.revision.is_none() && row.finished_at.is_none());
    let committed = store.index_baseline().unwrap();
    assert_eq!(committed.index_generation, before.index_generation);
    assert_eq!(
        committed.index_revision,
        before.index_revision + 1,
        "test seam committed selected pin before returning BUSY"
    );
    drop(leader);
    let successor = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let (_, new_owner) = reconcile_workspace(&successor, &options, &cancel, |_| {}).unwrap();
    assert_eq!(drain_requests(&successor, &new_owner).unwrap(), 1);
    let done = successor.request_by_id(&ack.id).unwrap().unwrap();
    assert_eq!(done.state, "done");
    assert_eq!(done.seq, ack.seq);
    assert!(done.revision.unwrap().index_revision > committed.index_revision);
}

#[test]
fn recovery_pin_must_not_complete_a_request_admitted_after_its_capture() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let (old_pin, session) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    // The owner published its source capture first; only then did a client
    // commit this new ACK. It cannot claim that earlier publication.
    let later = store.enqueue_request(&options, None).unwrap();
    assert_eq!(
        store.request_by_id(&later.id).unwrap().unwrap().state,
        "queued"
    );
    // An edit after the earlier capture: only the request's own capture sees
    // it, and an unchanged capture would publish nothing (Decision 0006 §3).
    fs::write(workspace.path().join("a.js"), "function b() {}\n").unwrap();
    let modes = std::sync::Mutex::new(Vec::new());
    assert_eq!(
        drain_requests_observed(&store, &session, |id, p| {
            if p.phase.starts_with("mode:") {
                modes.lock().unwrap().push(id.to_owned());
            }
        })
        .unwrap(),
        1
    );
    assert_eq!(modes.into_inner().unwrap(), vec![later.id.clone()]);
    let completed = store.request_by_id(&later.id).unwrap().unwrap();
    assert_eq!(completed.state, "done");
    assert!(completed.revision.unwrap().index_revision > old_pin.index_revision);
}

#[test]
fn explicit_reconcile_replaces_corruption_without_upgrading_a_reader() {
    use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("a.js"), "function a() {}\n").unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(state.path().join("cache"), state.path().join("data"));
    let identity = WorkspaceIdentity::discover(Some(work.path()), work.path()).unwrap();
    let path = roots.index_db(&identity);
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(work.path().to_owned());
    let (old, first_owner) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    assert_eq!(store.status().unwrap().revision, old);
    drop(first_owner);
    drop(store);
    fs::write(&path, b"short").unwrap();
    let corrupt = fs::read(&path).unwrap();
    let pending = Store::open_for_tests(state.path(), work.path()).unwrap();
    assert!(pending.is_recreate_pending());
    let queued = pending.enqueue_request(&options, None).unwrap();
    let queue_before = fs::read(pending.request_db_path()).unwrap();
    let shared = roots.index_use_existing(&identity).unwrap();
    let busy = reconcile_workspace(&pending, &options, &cancel, |_| {}).unwrap_err();
    assert!(busy.to_string().contains("storage_busy"), "{busy:#}");
    assert_eq!(fs::read(&path).unwrap(), corrupt);
    assert_eq!(fs::read(pending.request_db_path()).unwrap(), queue_before);
    assert_eq!(
        pending.request_by_id(&queued.id).unwrap().unwrap().state,
        "queued"
    );
    assert!(pending.is_recreate_pending());
    drop(shared);
    let mut configured = options.clone();
    configured.max_file_bytes = 2_097_152;
    let (new, owner) = reconcile_workspace(&pending, &configured, &cancel, |_| {}).unwrap();
    assert_ne!(new.index_generation, old.index_generation);
    assert_eq!(new.index_revision, 1);
    assert_eq!(pending.status().unwrap().revision, new);
    assert_eq!(
        pending
            .recorded_index_options()
            .unwrap()
            .unwrap()
            .max_file_bytes,
        2_097_152
    );
    owner.verify().unwrap();
}

#[test]
fn pending_read_refuses_unknown_options_and_daemon_config_does_not_fallback() {
    use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("a.js"), "function a() {}\n").unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(state.path().join("cache"), state.path().join("data"));
    let identity = WorkspaceIdentity::discover(Some(work.path()), work.path()).unwrap();
    let path = roots.index_db(&identity);
    let old = Store::open_for_tests(state.path(), work.path()).unwrap();
    // Retain an accepted queue before exceptional index corruption begins.
    let pending_row = old
        .enqueue_request(&IndexOptions::new(work.path().to_owned()), None)
        .unwrap();
    drop(old);
    fs::write(&path, b"not sqlite format header").unwrap();
    let original = fs::read(&path).unwrap();
    let pending = Store::open_for_tests(state.path(), work.path()).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let missing = establish_serving_session(&pending, None, &cancel).unwrap_err();
    assert!(
        missing.to_string().contains("explicit trellis index"),
        "{missing:#}"
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    let mut configured = IndexOptions::new(work.path().to_owned());
    configured.max_file_bytes = 2_097_152;
    let shared = roots.index_use_existing(&identity).unwrap();
    let busy = establish_serving_session(&pending, Some(&configured), &cancel).unwrap_err();
    assert!(busy.to_string().contains("storage_busy"), "{busy:#}");
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(pending.is_recreate_pending());
    drop(shared);
    let owner = establish_serving_session(&pending, Some(&configured), &cancel).unwrap();
    owner.verify().unwrap();
    assert_eq!(
        pending
            .request_by_id(&pending_row.id)
            .unwrap()
            .unwrap()
            .state,
        "queued"
    );
    assert_eq!(pending.status().unwrap().revision.index_revision, 1);
    assert_eq!(
        pending
            .recorded_index_options()
            .unwrap()
            .unwrap()
            .max_file_bytes,
        2_097_152
    );
}

#[test]
fn coordinator_capture_opens_reads_and_hashes_each_source_once() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("main.js"),
        "function a() { b(); } function b() {}\n",
    )
    .unwrap();
    fs::write(
        work.path().join("Lib.java"),
        "class Lib { void run() {} }\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let expected = |files: &[String]| {
        files
            .iter()
            .map(|path| (path.clone(), (1, 1, 1)))
            .collect::<BTreeMap<_, _>>()
    };

    let mut published = None;
    let first_job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let first_session = first_job.session();
    let first = first_job
        .run_observed(
            &options,
            &cancel,
            |_| {},
            |capture| published = Some(counts(capture)),
        )
        .unwrap();
    let (files, ops) = published.expect("capture observed");
    assert_eq!(files.len(), 2);
    assert_eq!(ops, expected(&files));
    assert_eq!(store.status().unwrap().revision, first);

    // A cancel after the single capture leaves the prior pair and the same counters.
    fs::write(work.path().join("main.js"), "function late() {}\n").unwrap();
    let mut cancelled = None;
    drop(first_session);
    let second_job = IndexJobCoordinator::prepare(&store, Some(first)).unwrap();
    let _second_session = second_job.session();
    let error = second_job
        .run_observed(
            &options,
            &cancel,
            |_| {},
            |capture| {
                cancelled = Some(counts(capture));
                cancel.store(true, Ordering::Release);
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    let (files, ops) = cancelled.expect("capture observed before cancel");
    assert_eq!(ops, expected(&files));
    assert_eq!(store.index_baseline().unwrap(), first);
    // Cancellation before COMMIT must not close the validated prior head
    // to daemon-local reads while this Store still awaits its new startup reconciliation.
    let prior = store.evidence_response_daemon_local().unwrap();
    assert_eq!(prior.status().unwrap().revision, first);
    assert_eq!(
        prior.source_at("main.js", Some(first)).unwrap().unwrap().0,
        first
    );
    prior.finish(()).unwrap();
}
#[test]
fn reused_leader_session_is_bound_to_its_store_before_capture() {
    let state = tempfile::tempdir().unwrap();
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();
    std::fs::write(
        first_root.path().join("a.js"),
        "function a() {}
",
    )
    .unwrap();
    std::fs::write(
        second_root.path().join("b.js"),
        "function b() {}
",
    )
    .unwrap();
    let first = Store::open_for_tests(&state.path().join("first"), first_root.path()).unwrap();
    let second = Store::open_for_tests(&state.path().join("second"), second_root.path()).unwrap();
    let session = first.writer_session().unwrap();
    let error = IndexJobCoordinator::prepare_with_session(&second, None, session)
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("another workspace")
            || error.to_string().contains("wrong leader guard"),
        "{error:#}"
    );
}

#[test]
fn captured_writer_is_reverified_after_root_replacement_before_publish() {
    use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
    let state = tempfile::tempdir().unwrap();
    let work = state.path().join("work");
    let moved = state.path().join("moved");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(work.join("a.js"), "function a() {}\n").unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(state.path().join("cache"), state.path().join("data"));
    let identity = WorkspaceIdentity::discover(Some(&work), &work)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = Store::open(roots, identity).unwrap();
    let job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let error = job
        .run_observed(
            &IndexOptions::new(work.clone()),
            &cancel,
            |_| {},
            |_| {
                std::fs::rename(&work, &moved).unwrap();
                std::fs::create_dir(&work).unwrap();
            },
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("root_changed"), "{error:#}");
    assert!(store.status().is_err());
}

#[test]
fn full_publication_never_rebuilds_its_own_fresh_artifact() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("main.js"),
        "function a() { b(); } function b() {}\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let _session = job.session();
    let before = crate::native_evidence::full_validations_for_tests();
    let pin = job.run_observed(&options, &cancel, |_| {}, |_| {}).unwrap();
    // The artifact is this process's own build of the capture (#199 I-4b):
    // measurement and publication keep the structural and bundle checks, and
    // neither rebuilds it to compare.
    assert_eq!(
        crate::native_evidence::full_validations_for_tests() - before,
        0
    );
    assert_eq!(store.status().unwrap().revision, pin);
}

#[test]
fn cold_full_publication_uses_bounded_document_stream() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(
        workspace.path().join("a.js"),
        "function a() { return b(); }\n",
    )
    .unwrap();
    fs::write(workspace.path().join("b.py"), "def b():\n    return 1\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let before = full_stream_runs_for_tests();
    let (pin, _owner) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    assert_eq!(
        full_stream_runs_for_tests() - before,
        1,
        "cold full publication must use the bounded per-document stream"
    );
    assert_eq!(store.status().unwrap().revision, pin);
}

#[test]
fn cancelled_full_document_stream_never_changes_selected_pin() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function before() {}\n").unwrap();
    fs::write(
        workspace.path().join("b.py"),
        "def before():\n    return 1\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let (prior, owner) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    drop(owner);
    fs::write(workspace.path().join("a.js"), "function after() {}\n").unwrap();
    fs::write(
        workspace.path().join("b.py"),
        "def after():\n    return 2\n",
    )
    .unwrap();
    let mut coordinator = IndexJobCoordinator::prepare(&store, Some(prior)).unwrap();
    let observed = AtomicBool::new(false);
    let attempt = coordinator.run_with_capture(
        &options,
        &cancel,
        |_| {},
        |_| {},
        |_, stage| {
            if stage == crate::native_evidence::FullNativeStage::Validated {
                observed.store(true, Ordering::Release);
                cancel.store(true, Ordering::Release);
            }
        },
        PublicationAdmission {
            capture: None,
            unchanged_fast: false,
            cutoff: || Ok(()),
        },
        || {},
    );
    assert!(
        observed.load(Ordering::Acquire),
        "first document must validate"
    );
    assert!(attempt.is_err(), "cancelled stream cannot publish");
    // A cancelled writer requires a new reconciliation before status serves
    // again. Inspect the durable selected pair without treating that temporary
    // gate as a lost or partially published revision.
    let identity = crate::store::topology::WorkspaceIdentity::discover(
        Some(workspace.path()),
        workspace.path(),
    )
    .unwrap();
    let db = rusqlite::Connection::open(
        state
            .path()
            .join("cache/indexes")
            .join(&identity.root_key)
            .join("index.db"),
    )
    .unwrap();
    let selected: (String, i64) = db
        .query_row(
            "SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        selected,
        (
            prior.index_generation.to_string(),
            prior.index_revision as i64
        )
    );
    drop(db);
    cancel.store(false, Ordering::Release);
    drop(coordinator);
    let next = IndexJobCoordinator::prepare(&store, Some(prior))
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    assert_eq!(store.status().unwrap().revision, next);
}
