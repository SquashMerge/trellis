use super::*;
use anyhow::Context;
use std::process::Command;

#[cfg(feature = "test-support")]
#[test]
fn cli_status_names_a_blocked_reconciliation_and_clears_after_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    // A standalone scheduler has no background worker; each fault and
    // recovery transition below is driven explicitly by this test.
    let store =
        crate::testing::retry_admission(|| Store::open_for_tests(&tmp.path().join("state"), &root));
    let scheduler = http::new(
        store.clone(),
        IndexOptions::new(root.clone()),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let resources = Arc::new(ActiveResources { store, scheduler });
    let runtime = CheckoutRuntime {
        resources: std::sync::Mutex::new(Some(resources.clone())),
        search: None,
        search_coverage: std::sync::Mutex::new(None),
        search_coverage_scans: std::sync::atomic::AtomicUsize::new(0),
        browser_status_projections: std::sync::atomic::AtomicUsize::new(0),
        root_lost: std::sync::atomic::AtomicBool::new(false),
        initial_indexing: true,
        rules_edit_owner: std::sync::Mutex::new(()),
        pickup: crate::producer::pickup::PickupOwner::default(),
        producer: crate::producer::run::ProducerOwner::default(),
        auto_run: crate::producer::auto::AutoRunOwner::default(),
        view_requests: Default::default(),
        mcp_clock_skew_ms: std::sync::atomic::AtomicU64::new(0),
        search_hash_scans: std::sync::atomic::AtomicUsize::new(0),
        mcp_call_clock_origin: std::sync::Mutex::new(None),
        mcp_hint_clock_origin: std::sync::Mutex::new(None),
        mcp_read_hook: std::sync::Mutex::new(None),
        mcp_read_gate: std::sync::Mutex::new(None),
        mcp_admitted_gate: std::sync::Mutex::new(None),
        evidence_sampled_hook: std::sync::Mutex::new(None),
    };
    let queued = resources
        .store
        .enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    for _ in 0..5 {
        runtime.set_startup_hook_for_tests(Arc::new(|| {
            panic!("persistent startup capture fault at /private/secret/a.js")
        }));
        // A new durable request wakes an attempt; no timing/polling is needed.
        resources
            .scheduler
            .enqueue_index(&IndexOptions::new(root.clone()), None)
            .unwrap();
        assert!(
            resources
                .scheduler
                .force_root_transition_tick_for_tests()
                .is_err()
        );
    }
    let blocker = runtime.cli_status().unwrap_err().to_string();
    // The reply and the row carry the typed cause, never the fault's path (#768).
    assert_eq!(blocker, "recovery_blocked: internal error");
    let row = resources.store.request_by_id(&queued.id).unwrap().unwrap();
    assert_eq!(row.state, "failed");
    assert_eq!(
        row.error_code.as_deref(),
        Some("recovery_blocked: internal error")
    );
    let next = resources
        .scheduler
        .enqueue_index(&IndexOptions::new(root), None)
        .unwrap();
    resources
        .scheduler
        .force_root_transition_tick_for_tests()
        .unwrap();
    assert!(runtime.cli_status().is_ok());
    assert_eq!(
        resources
            .store
            .request_by_id(&next.id)
            .unwrap()
            .unwrap()
            .state,
        "done"
    );
    assert_eq!(
        resources
            .store
            .request_by_id(&queued.id)
            .unwrap()
            .unwrap()
            .state,
        "failed"
    );
}

#[test]
fn cli_index_contention_retries_the_same_accepted_request() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store =
        crate::testing::retry_admission(|| Store::open_for_tests(state.path(), workspace.path()));
    let options = IndexOptions::new(workspace.path().to_owned());
    let accepted = store.enqueue_request(&options, None).unwrap();
    let mut attempts = 0;
    let row = cli_index_request(
        || {
            attempts += 1;
            if attempts == 1 {
                return Err(crate::store::topology::StorageBusy.into());
            }
            store.request_by_id(&accepted.id)
        },
        Instant::now() + crate::testing::HANG_BACKSTOP,
    )
    .unwrap()
    .unwrap();
    assert_eq!(attempts, 2);
    assert_eq!(row.id, accepted.id);
    assert_eq!(row.seq, accepted.seq);
    assert_eq!(row.state, "queued");
    assert_eq!(row.revision, None);
    assert_eq!(row.error_code, None);
    let queue = store.earliest_unfinished_request().unwrap().unwrap();
    assert_eq!(queue.id, accepted.id);
    assert_eq!(queue.seq, accepted.seq);
    let next = store.enqueue_request(&options, None).unwrap();
    assert_eq!(next.seq, accepted.seq + 1, "waiter must never re-enqueue");
}

#[test]
fn cli_index_expired_contention_is_typed_and_keeps_accepted_row_pending() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store =
        crate::testing::retry_admission(|| Store::open_for_tests(state.path(), workspace.path()));
    let options = IndexOptions::new(workspace.path().to_owned());
    let accepted = store.enqueue_request(&options, None).unwrap();
    let before = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(before.state, "queued");

    let busy = || -> anyhow::Error {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), None)
            .into()
    };
    let expired = Instant::now() - Duration::from_millis(1);
    let error = cli_index_contention(busy(), expired).unwrap_err();
    assert!(
        error
            .downcast_ref::<crate::store::topology::StorageBusy>()
            .is_some()
    );
    assert_eq!(error.to_string(), "storage_busy");
    // Neither an invariant-busy string nor a guarded SQLite cause is retried.
    let invariant = anyhow::anyhow!("storage_busy: cached claim changed");
    assert_eq!(
        cli_index_contention(invariant, expired)
            .unwrap_err()
            .to_string(),
        "storage_busy: cached claim changed"
    );
    let guarded = Err::<(), _>(busy())
        .context("root_changed: request root replaced")
        .unwrap_err();
    assert_eq!(
        cli_index_contention(guarded, expired)
            .unwrap_err()
            .to_string(),
        "root_changed: request root replaced"
    );
    let integrity = Err::<(), _>(busy())
        .context("incompatible_index: selected integrity")
        .unwrap_err();
    assert_eq!(
        cli_index_contention(integrity, expired)
            .unwrap_err()
            .to_string(),
        "incompatible_index: selected integrity"
    );

    let after = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(after.id, before.id);
    assert_eq!(after.seq, before.seq);
    assert_eq!(after.state, before.state);
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.error_code, before.error_code);
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn listing_probe_releases_registry_before_slow_work() {
    let temp = tempfile::tempdir().unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let registry = Arc::new(std::sync::Mutex::new(CheckoutRegistry::with_roots(roots)));
    let (captured_tx, captured_rx) = std::sync::mpsc::sync_channel(0);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
    let worker_registry = registry.clone();
    let worker = std::thread::spawn(move || {
        let probe = worker_registry.lock().unwrap().browser_checkouts_probe();
        captured_tx.send(()).unwrap();
        resume_rx.recv().unwrap();
        probe.check()
    });
    captured_rx
        .recv_timeout(crate::testing::HANG_BACKSTOP)
        .unwrap();
    assert_eq!(registry.try_lock().unwrap().active_count(), 0);
    resume_tx.send(()).unwrap();
    assert!(worker.join().unwrap().is_empty());
}

#[test]
fn selection_preparation_does_not_hold_registry() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    git(temp.path(), &["init", "--quiet", root.to_str().unwrap()]);
    let root = root.canonicalize().unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let registry = Arc::new(std::sync::Mutex::new(CheckoutRegistry::new()));
    let (captured_tx, captured_rx) = std::sync::mpsc::sync_channel(0);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
    let worker_registry = registry.clone();
    let worker = std::thread::spawn(move || {
        let probe = worker_registry.lock().unwrap().selection_probe();
        captured_tx.send(()).unwrap();
        resume_rx.recv().unwrap();
        probe.prepare(&identity, &root)
    });
    captured_rx
        .recv_timeout(crate::testing::HANG_BACKSTOP)
        .unwrap();
    assert_eq!(registry.try_lock().unwrap().active_count(), 0);
    resume_tx.send(()).unwrap();
    let prepared = worker.join().unwrap().unwrap();
    assert_eq!(registry.lock().unwrap().active_count(), 0);
    registry
        .lock()
        .unwrap()
        .select_prepared(1, prepared)
        .unwrap();
    assert_eq!(registry.lock().unwrap().active_count(), 1);
}

#[test]
fn prepared_selection_accepts_a_concurrent_same_identity_attachment() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    git(temp.path(), &["init", "--quiet", root.to_str().unwrap()]);
    let root = root.canonicalize().unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let mut registry = CheckoutRegistry::new();
    let prepared = registry
        .selection_probe()
        .prepare(&identity, &root)
        .unwrap();
    registry.attach_launch(2, &identity).unwrap();
    let selected = registry.select_prepared(1, prepared).unwrap();
    assert_eq!(selected.root_key, identity.root_key);
    assert_eq!(registry.active_count(), 1);
    assert_eq!(registry.entries.len(), 1);
    assert_eq!(
        registry.entries.get(&identity.root_key).unwrap().sessions,
        HashSet::from([1, 2])
    );
}

#[test]
fn prepared_selection_cannot_replace_a_successor_incarnation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    git(temp.path(), &["init", "--quiet", root.to_str().unwrap()]);
    let root = root.canonicalize().unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let mut registry = CheckoutRegistry::new();
    let prepared = registry
        .selection_probe()
        .prepare(&identity, &root)
        .unwrap();
    let mut successor = CheckoutMetadata::from_identity(&identity);
    successor.record_id = "successor".to_owned();
    registry.entries.insert(
        identity.root_key.clone(),
        Entry {
            identity: successor.clone(),
            registration: None,
            sessions: HashSet::from([2]),
            released: false,
            pending_work: false,
            external_work: false,
            browser_until: None,
            release_at: None,
            root_lost: false,
            lost_reads: None,
        },
    );
    assert!(matches!(
        registry.select_prepared(1, prepared),
        Err(SelectionError::IdentityChanged)
    ));
    let entry = registry.entries.get(&identity.root_key).unwrap();
    assert!(entry.identity == successor);
    assert_eq!(entry.sessions, HashSet::from([2]));
}

#[test]
fn unattached_git_wait_is_bounded_with_open_or_closed_stdout() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    git(temp.path(), &["init", "--quiet", root.to_str().unwrap()]);
    let root = root.canonicalize().unwrap();
    let identity = WorkspaceIdentity::discover_unattached(Some(&root), &root).unwrap();
    let executable = temp.path().join("stalled-git");
    for close_stdout in [false, true] {
        let close = if close_stdout { "exec 1>&-\n" } else { "" };
        fs::write(
            &executable,
            format!("#!/bin/sh\n{close}exec /bin/sleep 5\n"),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let start = Instant::now();
        assert_eq!(
            common_identity_unattached_with_executable(&identity, &executable),
            Err(SelectionError::Unavailable),
            "close_stdout={close_stdout}"
        );
        assert!(start.elapsed() < crate::testing::HANG_BACKSTOP);
    }
    assert!(!root.join(".git/trellis/workspace-id").exists());
}

#[test]
fn unattached_git_wait_preserves_common_identity_without_a_marker() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    git(temp.path(), &["init", "--quiet", root.to_str().unwrap()]);
    let root = root.canonicalize().unwrap();
    let identity = WorkspaceIdentity::discover_unattached(Some(&root), &root).unwrap();
    let common = fs::metadata(root.join(".git")).unwrap();
    assert_eq!(
        common_identity_unattached(&identity),
        Ok((common.dev(), common.ino()))
    );
    assert!(!root.join(".git/trellis/workspace-id").exists());
}

#[test]
fn rejected_selection_never_attaches_or_creates_a_foreign_marker() {
    let temp = tempfile::tempdir().unwrap();
    let launch = temp.path().join("launch");
    let foreign = temp.path().join("foreign");
    git(temp.path(), &["init", "--quiet", launch.to_str().unwrap()]);
    git(temp.path(), &["init", "--quiet", foreign.to_str().unwrap()]);
    let launch = launch.canonicalize().unwrap();
    let foreign = foreign.canonicalize().unwrap();
    let identity = WorkspaceIdentity::discover(Some(&launch), &launch).unwrap();
    let mut registry = CheckoutRegistry::new();
    registry.attach_launch(17, &identity).unwrap();
    assert_eq!(registry.entries.len(), 1);
    // A symlink alias is refused before any marker is written in its target.
    let alias = temp.path().canonicalize().unwrap().join("alias");
    std::os::unix::fs::symlink(&foreign, &alias).unwrap();
    assert_eq!(
        registry.select(17, &identity, &alias).unwrap_err(),
        SelectionError::NotCheckout
    );
    assert!(!foreign.join(".git/trellis/workspace-id").exists());
    assert_eq!(
        registry
            .select(17, &identity, Path::new("relative"))
            .unwrap_err(),
        SelectionError::NotAbsolute
    );
    assert_eq!(registry.entries.len(), 1);
    assert!(
        registry
            .entries
            .get(&identity.root_key)
            .unwrap()
            .sessions
            .contains(&17)
    );
    registry.disconnect(17);
    assert!(
        registry
            .entries
            .values()
            .all(|entry| entry.sessions.is_empty())
    );
}
#[test]
fn capacity_failure_retains_verified_root_without_reserving_a_slot() {
    let temp = tempfile::tempdir().unwrap();
    let launch = temp.path().join("launch");
    git(temp.path(), &["init", "--quiet", launch.to_str().unwrap()]);
    git(
        &launch,
        &["commit", "--allow-empty", "--quiet", "-m", "fixture"],
    );
    let selected = temp.path().join("linked");
    git(
        &launch,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            selected.to_str().unwrap(),
            "HEAD",
        ],
    );
    let launch = launch.canonicalize().unwrap();
    let selected = selected.canonicalize().unwrap();
    let identity = WorkspaceIdentity::discover(Some(&launch), &launch).unwrap();
    let mut registry = CheckoutRegistry::new();
    registry.attach_launch(17, &identity).unwrap();
    WorkspaceIdentity::discover(Some(&selected), &selected).unwrap();
    let common = launch.join(".git").canonicalize().unwrap();
    let fixture_common = |checked: &WorkspaceIdentity| -> Result<PathBuf, SelectionError> {
        assert!(checked.root == launch || checked.root == selected);
        checked.verify_readonly().unwrap();
        Ok(common.clone())
    };
    // Capacity owns this test; Git scheduling and membership have separate tests.
    let prepared = PreparedSelection {
        witness: CheckoutRegistry::capacity_witness_with_common_dirs(
            &identity,
            &selected,
            fixture_common,
        )
        .unwrap(),
        expected: None,
    };
    for index in 0..MAX_ACTIVE_CHECKOUTS - 1 {
        let key = format!("fixture-{index}");
        let mut metadata = CheckoutMetadata::from_identity(&identity);
        metadata.root_key = key.clone();
        registry.entries.insert(
            key,
            Entry {
                identity: metadata,
                registration: None,
                sessions: HashSet::from([18]),
                released: false,
                pending_work: false,
                external_work: false,
                browser_until: None,
                release_at: None,
                root_lost: false,
                lost_reads: None,
            },
        );
    }
    assert_eq!(registry.active_count(), MAX_ACTIVE_CHECKOUTS);
    assert_eq!(
        registry.select_prepared(17, prepared).unwrap_err(),
        SelectionError::CheckoutCapacity
    );
    assert_eq!(registry.entries.len(), MAX_ACTIVE_CHECKOUTS);
    assert_eq!(
        CheckoutRegistry::capacity_witness_with_common_dirs(&identity, &selected, fixture_common)
            .unwrap()
            .root,
        selected
    );
    let before: HashMap<_, _> = registry
        .entries
        .iter()
        .map(|(key, entry)| (key.clone(), entry.sessions.clone()))
        .collect();
    // Expire the last Git lookup through its real deadline guard, without sleeping.
    let mut lookups = 0;
    let unavailable =
        CheckoutRegistry::capacity_witness_with_common_dirs(&identity, &selected, |checked| {
            lookups += 1;
            if lookups == 4 {
                checked
                    .git_common_dir_by(Some(Instant::now()))
                    .map_err(|_| SelectionError::Unavailable)
            } else {
                fixture_common(checked)
            }
        })
        .unwrap_err();
    assert_eq!(lookups, 4);
    assert_eq!(unavailable, SelectionError::Unavailable);
    assert_eq!(registry.active_count(), MAX_ACTIVE_CHECKOUTS);
    assert_eq!(
        registry
            .entries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.sessions.clone()))
            .collect::<HashMap<_, _>>(),
        before
    );
    assert!(registry.entries.values().all(
        |entry| entry.sessions.contains(&17) == (entry.identity.root_key == identity.root_key)
    ));
}
