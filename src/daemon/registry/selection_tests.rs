use super::*;
use anyhow::Context;
use std::process::Command;

/// A checkout runtime over a standalone scheduler in `tmp`, with its workspace root. Such a
/// scheduler has no background worker: the test drives every worker step itself.
#[cfg(feature = "test-support")]
fn standalone_runtime(tmp: &Path) -> (std::path::PathBuf, Arc<ActiveResources>, CheckoutRuntime) {
    let root = tmp.join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let store =
        crate::testing::retry_admission(|| Store::open_for_tests(&tmp.join("state"), &root));
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
        mcp_building_gate: std::sync::Mutex::new(None),
        evidence_sampled_hook: std::sync::Mutex::new(None),
    };
    (root, resources, runtime)
}

#[cfg(feature = "test-support")]
#[test]
fn cli_status_names_a_blocked_reconciliation_and_clears_after_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    // A standalone scheduler has no background worker; each fault and
    // recovery transition below is driven explicitly by this test.
    let (root, resources, runtime) = standalone_runtime(tmp.path());
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

/// #1082: an automatic producer run is admitted by the runtime that owns the phase and the
/// watcher, not by the decider's earlier sample of them.
#[cfg(feature = "test-support")]
mod auto_run_admission {
    use super::*;
    use crate::producer::approval::{Approval, ApprovalRead, ApprovalRequest};
    use crate::producer::auto::{self, AutoRunRuntime, AutoRunSources, LaunchRequest};
    use crate::producer::auto_config::{AutoConfig, ConfigState};
    use crate::producer::run::{AutoCancel, SupersedeToken};
    use crate::testing::HANG_BACKSTOP;
    use std::collections::BTreeMap;
    use std::sync::{Mutex, atomic::AtomicBool, atomic::Ordering, mpsc};

    const PRODUCER: &str = "alpha";
    const HEAD: &str = "h1";

    fn approval() -> Approval {
        Approval {
            producer: PRODUCER.to_owned(),
            trust_profile: crate::producer::TrustProfile::Trusted,
            request: ApprovalRequest {
                limits: crate::producer::plan::LimitsRequest {
                    timeout_secs: 1800,
                    memory_mib: 8192,
                },
                executable: None,
                environment: Default::default(),
            },
            template_sha256: "a".repeat(64),
            root_record_id: "record".into(),
            root_inode: 1,
            approved_at: 7,
        }
    }

    fn request() -> LaunchRequest {
        LaunchRequest {
            approval: approval(),
            expected_revision: HEAD.to_owned(),
            token: SupersedeToken::default(),
        }
    }

    /// One enabled, approved producer that is due at the head, with no gate.
    struct Due;

    impl AutoRunSources for Due {
        fn pin(&self) -> anyhow::Result<IndexPin> {
            Ok(IndexPin {
                index_generation: uuid::Uuid::from_u128(1),
                index_revision: 1,
            })
        }

        fn head(&self, producers: &[&str]) -> anyhow::Result<crate::store::AutoRunHead> {
            Ok(crate::store::AutoRunHead {
                pin: self.pin()?,
                native_revision_id: HEAD.to_owned(),
                due: producers
                    .iter()
                    .map(|name| ((*name).to_owned(), true))
                    .collect(),
            })
        }

        fn config(&self) -> ConfigState {
            ConfigState::Valid(BTreeMap::from([(
                PRODUCER.to_owned(),
                AutoConfig {
                    enabled: true,
                    quiet: Duration::ZERO,
                    min_interval: Duration::ZERO,
                    battery_gate: false,
                    load_gate: false,
                },
            )]))
        }

        fn approval(&self, _producer: &str) -> ApprovalRead {
            ApprovalRead::Valid(Box::new(approval()))
        }

        fn seed(&self) -> BTreeMap<String, auto::Seed> {
            BTreeMap::new()
        }
    }

    struct NoProbes;

    impl crate::producer::auto_gate::Probes for NoProbes {
        fn on_battery(&self) -> Option<bool> {
            None
        }

        fn load_saturated(&self) -> Option<bool> {
            None
        }
    }

    /// One-shot: tells the test the run thread reached `launch`, then waits to be released.
    type Hold = (mpsc::Sender<()>, mpsc::Receiver<()>);

    /// The checkout's real host. Its first launch is held before the runtime is asked to admit
    /// it, and every launch's outcome is reported.
    struct HeldLaunch {
        host: AutoHost,
        hold: Mutex<Option<Hold>>,
        outcomes: Mutex<mpsc::Sender<LaunchOutcome>>,
    }

    impl AutoRunRuntime for HeldLaunch {
        fn alive(&self) -> bool {
            self.host.alive()
        }

        fn phase_ready(&self) -> bool {
            self.host.phase_ready()
        }

        fn intent_pending(&self) -> bool {
            self.host.intent_pending()
        }

        fn active_origin(&self) -> Option<crate::producer::state::RunOrigin> {
            self.host.active_origin()
        }

        fn supersede(&self, token: &SupersedeToken) -> AutoCancel {
            self.host.supersede(token)
        }

        fn state_version(&self) -> u64 {
            self.host.state_version()
        }

        fn wait_state_change(&self, seen: u64, timeout: Duration) {
            self.host.wait_state_change(seen, timeout);
        }

        fn wake(&self) {
            self.host.wake();
        }

        fn launch(&self, request: LaunchRequest) -> LaunchOutcome {
            let hold = self.hold.lock().unwrap().take();
            if let Some((reached, release)) = hold {
                let _ = reached.send(());
                release
                    .recv_timeout(HANG_BACKSTOP)
                    .expect("the held launch was never released");
            }
            let outcome = self.host.launch(request);
            let _ = self.outcomes.lock().unwrap().send(outcome.clone());
            outcome
        }
    }

    /// A `Ready` checkout whose watcher has acknowledged its first inventory.
    fn settled_checkout(tmp: &Path) -> (Arc<ActiveResources>, Arc<CheckoutRuntime>) {
        let (root, resources, runtime) = standalone_runtime(tmp);
        resources
            .scheduler
            .enqueue_index(&IndexOptions::new(root), None)
            .unwrap();
        resources
            .scheduler
            .force_root_transition_tick_for_tests()
            .unwrap();
        let runtime = Arc::new(runtime);
        let host = AutoHost {
            runtime: Arc::downgrade(&runtime),
        };
        assert!(host.phase_ready());
        assert!(!host.intent_pending(), "the fixture has nothing pending");
        (resources, runtime)
    }

    /// The decider samples a settled `Ready` checkout and decides to start. Before its run
    /// thread reaches the runtime, the serving session is lost and startup reconciliation
    /// makes the checkout `Ready` again on a new session, whose watcher has not run its
    /// first inventory. The decider's sample said nothing was pending; the runtime must not
    /// admit the run on it.
    #[test]
    fn a_run_decided_before_a_session_replacement_is_not_admitted_before_its_first_inventory() {
        let tmp = tempfile::tempdir().unwrap();
        let (resources, runtime) = settled_checkout(tmp.path());
        let scheduler = resources.scheduler.clone();

        let (reached, launch_reached) = mpsc::channel();
        let (release_launch, release) = mpsc::channel();
        let (outcomes, launch_outcome) = mpsc::channel();
        let held = Arc::new(HeldLaunch {
            host: AutoHost {
                runtime: Arc::downgrade(&runtime),
            },
            hold: Mutex::new(Some((reached, release))),
            outcomes: Mutex::new(outcomes),
        });
        let closed = Arc::new(AtomicBool::new(false));
        let deps = auto::Deps {
            runtime: held.clone(),
            sources: Arc::new(Due),
            clock: Arc::new(auto::SystemClock),
            probes: Arc::new(NoProbes),
            intervals: auto::Intervals {
                quiet: Some(Duration::ZERO),
                min_interval: Some(Duration::ZERO),
                ..Default::default()
            },
            spawn: auto::thread_spawner(),
            closed: closed.clone(),
            published: Default::default(),
        };
        let decider = std::thread::spawn(move || auto::run_loop(deps));
        launch_reached
            .recv_timeout(HANG_BACKSTOP)
            .expect("the decider never started a run on the settled checkout");

        // The worker, on this thread: the session is lost, and the next step reconciles to a
        // new one. It stops between making the checkout `Ready` and its watcher's inventory.
        scheduler.lose_serving_session_for_tests();
        let (admitted, admission) = mpsc::channel();
        let paused = scheduler.clone();
        scheduler.set_after_startup_drain_hook_for_tests(move || {
            assert_eq!(paused.runtime_phase(), RuntimePhase::Ready);
            assert!(
                paused.checkout_watch_pending(),
                "the replacement watcher's first inventory is pending"
            );
            let _ = release_launch.send(());
            let outcome = launch_outcome
                .recv_timeout(HANG_BACKSTOP)
                .expect("the held launch never returned");
            let _ = admitted.send(outcome);
        });
        scheduler.force_root_transition_tick_for_tests().unwrap();
        let outcome = admission
            .recv_timeout(HANG_BACKSTOP)
            .expect("the worker never reached the replacement watcher");
        assert_eq!(
            outcome,
            LaunchOutcome::NotReady,
            "an automatic run was admitted while the replacement watcher's first inventory \
             was pending"
        );

        closed.store(true, Ordering::Release);
        held.wake();
        decider.join().unwrap();

        // Once that inventory is acknowledged the same request passes admission: it is
        // refused later, by the plan, for its unknown producer.
        assert!(!scheduler.checkout_watch_pending());
        assert_ne!(held.host.launch(request()), LaunchOutcome::NotReady);
    }

    /// With no serving session the checkout has its reconciliation and the next watcher's
    /// first inventory ahead of it. That is pending intent, not a quiet checkout.
    #[test]
    fn a_checkout_that_lost_its_serving_session_reports_pending_intent() {
        let tmp = tempfile::tempdir().unwrap();
        let (resources, runtime) = settled_checkout(tmp.path());
        let host = AutoHost {
            runtime: Arc::downgrade(&runtime),
        };
        // The decider's phase sample, then the loss, then its intent sample.
        assert!(host.phase_ready());
        resources.scheduler.lose_serving_session_for_tests();
        assert!(
            host.intent_pending(),
            "a checkout with no serving session read as having nothing pending"
        );
    }
}
