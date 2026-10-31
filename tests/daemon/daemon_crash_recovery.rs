//! #122: a process that is really killed (SIGKILL) at a hooked point leaves
//! only durable state; a restarted daemon runtime recovers from it (Decision
//! 0008 §3).
//!
//! Coverage, precisely: the killed child runs the Store / index-coordinator
//! path the daemon worker calls (startup reconciliation, claim, drain and
//! publication), not the daemon's worker thread itself. The restart side is
//! the real daemon: `CheckoutRegistry::activate` and its checkout worker.
#![cfg(feature = "test-support")]
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};
use trellis::{
    daemon::registry::{CheckoutRegistry, CheckoutRuntime},
    indexer::IndexOptions,
    store::{
        Store,
        requests::Request,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

const STATE_ENV: &str = "TRELLIS_TEST_CRASH_RECOVERY_STATE";
const ROOT_ENV: &str = "TRELLIS_TEST_CRASH_RECOVERY_ROOT";
const SQLITE_JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

fn identity(root: &Path) -> WorkspaceIdentity {
    WorkspaceIdentity::discover(Some(root), root)
        .unwrap()
        .attach_marker()
        .unwrap()
}
fn roots(state: &Path) -> TopologyRoots {
    TopologyRoots::isolated_for_tests(state.join("cache"), state.join("data"))
}
fn child_paths() -> Option<(PathBuf, PathBuf)> {
    Some((
        std::env::var_os(STATE_ENV)?.into(),
        std::env::var_os(ROOT_ENV)?.into(),
    ))
}
fn cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// Report `line` to the parent, then hold every lock and open transaction
/// until the parent kills this process.
fn announce_and_wait_for_kill(line: &str) -> ! {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").unwrap();
    out.flush().unwrap();
    drop(out);
    loop {
        std::thread::park();
    }
}

/// Run `child_test` in a fresh test-binary process, wait for its hooked
/// announcement, then SIGKILL it and reap it. Returns the announcement.
fn run_child_until_hook_then_kill(child_test: &str, state: &Path, root: &Path) -> String {
    let child_test = child_test_name(child_test);
    let mut child: Child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &child_test, "--nocapture", "--test-threads=1"])
        .env(STATE_ENV, state)
        .env(ROOT_ENV, root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let output = child.stdout.take().unwrap();
    let (found_tx, found) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = BufReader::new(output);
        let mut line = String::new();
        // libtest may print "test <name> ... " on the same line before the hook.
        while output.read_line(&mut line).unwrap_or(0) != 0 && !line.contains("HOOKED ") {
            line.clear();
        }
        let _ = found_tx.send(line);
    });
    // Bounded: a child that never reaches its hook fails the test, never hangs it.
    let line = match found.recv_timeout(trellis::testing::HANG_BACKSTOP) {
        Ok(line) => line,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not reach its hook within the hang backstop");
        }
    };
    if !line.contains("HOOKED ") {
        let _ = child.kill();
        panic!("child exited before its hook: {:?}", child.wait());
    }
    child.kill().unwrap();
    let status = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "child was not killed: {status:?} {line}"
    );
    let at = line.find("HOOKED ").unwrap() + "HOOKED ".len();
    line.trim_end()[at..].to_owned()
}

/// Records every tracked row's state at each FIFO claim, then re-arms for the
/// next claim. The worker runs the after-claim hook after the claim COMMITs and
/// before it starts the row's work, so each entry is an event-ordered snapshot
/// of the claim boundary. Unlike millisecond timestamps, it cannot pass a
/// reversed or overlapping claim order.
fn record_claims(
    runtime: Arc<CheckoutRuntime>,
    store: Store,
    tracked: Arc<Vec<String>>,
    log: Arc<Mutex<Vec<Vec<String>>>>,
) {
    let next = runtime.clone();
    runtime.set_after_claim_hook_for_tests(move || {
        let states = tracked
            .iter()
            .map(|id| store.request_by_id(id).unwrap().unwrap().state)
            .collect();
        log.lock().unwrap().push(states);
        record_claims(next, store, tracked, log);
    });
}

/// Activate a fresh daemon checkout runtime on the durable state and wait,
/// bounded only by the hang backstop, until `last` reaches a terminal row.
/// `on_activate` runs while startup reconciliation is held, so before any claim.
async fn restart_daemon_until_terminal(
    state: &Path,
    root: &Path,
    last: &str,
    on_activate: impl FnOnce(&Arc<CheckoutRuntime>),
) -> (CheckoutRegistry, Arc<CheckoutRuntime>) {
    let id = identity(root);
    let mut registry = CheckoutRegistry::with_roots(roots(state));
    registry.attach_launch(1, &id).unwrap();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    registry.set_next_startup_hook_for_tests(Arc::new(move || {
        // Returns at once on a retry, after the sender is dropped.
        let _ = release_rx.lock().unwrap().recv();
    }));
    let runtime = registry.activate(&id.root_key).unwrap();
    on_activate(&runtime);
    drop(release_tx);
    // Until the runtime has recovered the checkout, a reader's open or read may
    // be a typed refusal; keep polling, bounded only by the hang backstop.
    let finished = || -> anyhow::Result<bool> {
        let observed = Store::open(roots(state), identity(root))?;
        let row = observed.request_by_id(last)?;
        Ok(row.expect("accepted ID was lost").finished_at.is_some())
    };
    let mut last_error = None;
    let drained = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            match finished() {
                Ok(true) => break,
                Ok(false) => {}
                Err(error) => last_error = Some(format!("{error:#}")),
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(
        drained.is_ok(),
        "restarted daemon did not drain the queue: {:?}; last read error: {last_error:?}",
        runtime.reconciliation_error()
    );
    (registry, runtime)
}

#[test]
fn queued_requests_daemon_child() {
    let Some((state, root)) = child_paths() else {
        return;
    };
    let store = Store::open(roots(&state), identity(&root)).unwrap();
    let options = IndexOptions::new(root.clone());
    let (_, session) =
        trellis::index_coordinator::reconcile_workspace(&store, &options, &cancel(), |_| {})
            .unwrap();
    // The FIFO head reaches its one terminal result before the crash.
    assert_eq!(
        trellis::index_coordinator::drain_one_request_observed(&store, &session, |_, _| {})
            .unwrap(),
        1
    );
    // The next claim is durable (running); the daemon dies before publishing it.
    store.set_after_claim_hook_for_tests(|| announce_and_wait_for_kill("HOOKED claimed"));
    let _ = trellis::index_coordinator::drain_requests(&store, &session);
    unreachable!("the after-claim hook never returns");
}

#[tokio::test]
async fn killed_daemon_with_accepted_queue_restarts_to_one_fifo_terminal_result_per_id() {
    let base = tempfile::tempdir().unwrap();
    let state = base.path().join("state");
    let root = base.path().join("work");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function queued() {}\n").unwrap();
    let store = Store::open(roots(&state), identity(&root)).unwrap();
    let options = IndexOptions::new(root.clone());
    let accepted: Vec<Request> = (0..4)
        .map(|_| store.enqueue_request(&options, None).unwrap())
        .collect();
    drop(store);

    assert_eq!(
        run_child_until_hook_then_kill("queued_requests_daemon_child", &state, &root),
        "claimed"
    );

    // Durable state at the instant of death: one done, one running, two only queued.
    let store = Store::open(roots(&state), identity(&root)).unwrap();
    let at_death: Vec<Request> = accepted
        .iter()
        .map(|r| store.request_by_id(&r.id).unwrap().unwrap())
        .collect();
    let states: Vec<&str> = at_death.iter().map(|r| r.state.as_str()).collect();
    assert_eq!(states, ["done", "running", "queued", "queued"]);
    let dead_claim = at_death[1].claim_incarnation.clone().unwrap();

    let tracked = Arc::new(accepted.iter().map(|r| r.id.clone()).collect::<Vec<_>>());
    let claims = Arc::new(Mutex::new(Vec::new()));
    let (_registry, _runtime) =
        restart_daemon_until_terminal(&state, &root, &accepted[3].id, |runtime| {
            record_claims(runtime.clone(), store.clone(), tracked, claims.clone())
        })
        .await;

    let drained: Vec<Request> = accepted
        .iter()
        .map(|r| store.request_by_id(&r.id).unwrap().unwrap())
        .collect();
    for request in &drained {
        assert_eq!(request.state, "done", "{request:?}");
        assert!(request.revision.is_some() && request.finished_at.is_some());
    }
    // The pre-crash terminal result is the only one for its ID: never rewritten.
    assert_eq!(drained[0].revision, at_death[0].revision);
    assert_eq!(drained[0].finished_at, at_death[0].finished_at);
    assert_eq!(drained[0].claim_incarnation, at_death[0].claim_incarnation);
    // The dead daemon's running claim was reclaimed by the new incarnation.
    assert_ne!(drained[1].claim_incarnation.as_deref(), Some(&*dead_claim));
    // These unchanged rows complete at the same head (Decision 0006 §3).
    assert!(drained.iter().all(|r| r.revision == drained[0].revision));
    // FIFO: the restarted daemon reclaims the running row first, and claims
    // each later row only once every earlier row is terminal.
    let row = |states: [&str; 4]| states.map(str::to_owned).to_vec();
    assert_eq!(
        *claims.lock().unwrap(),
        vec![
            row(["done", "running", "queued", "queued"]),
            row(["done", "done", "running", "queued"]),
            row(["done", "done", "done", "running"]),
        ],
        "claim-boundary snapshots"
    );
    assert!(store.earliest_unfinished_request().unwrap().is_none());
}

#[test]
fn index_commit_hot_journal_daemon_child() {
    let Some((state, root)) = child_paths() else {
        return;
    };
    let store = Store::open(roots(&state), identity(&root)).unwrap();
    let options = IndexOptions::new(root.clone());
    let (head, session) =
        trellis::index_coordinator::reconcile_workspace(&store, &options, &cancel(), |_| {})
            .unwrap();
    // Change every file so the request's index publication rewrites enough
    // pages that SQLite spills them into index.db before COMMIT. A small
    // publication cache stands in for a publication larger than the real one.
    rewrite_workspace(&root, "after");
    store.set_publication_cache_kib_for_tests(2000);
    let journal = index_db(&state, &root).with_file_name("index.db-journal");
    store.set_publication_before_commit_hook_for_tests(move || {
        let header = fs::read(&journal).map(|bytes| bytes[..8].to_vec());
        announce_and_wait_for_kill(&format!(
            "HOOKED {} {} {}",
            head.index_generation,
            head.index_revision,
            matches!(header, Ok(bytes) if bytes == SQLITE_JOURNAL_MAGIC)
        ));
    });
    let _ = trellis::index_coordinator::drain_requests(&store, &session);
    unreachable!("the publication hook never returns");
}

fn index_db(state: &Path, root: &Path) -> PathBuf {
    roots(state).index_db(&identity(root))
}

fn rewrite_workspace(root: &Path, tag: &str) {
    for file in 0..200 {
        let body: String = (0..40)
            .map(|f| {
                format!(
                    "function {tag}_{file}_{f}(x) {{ return {tag}_{file}_{}(x); }}\n",
                    f + 1
                )
            })
            .collect();
        fs::write(root.join(format!("m{file}.js")), body).unwrap();
    }
}

#[tokio::test]
async fn killed_mid_index_commit_rolls_back_hot_journal_then_queue_resumes() {
    killed_mid_index_commit_recovers(false).await;
}

/// The killed COMMIT may also have torn page 1 of index.db. Rollback restores
/// it from the journal; the header check must not see it first and recreate.
#[tokio::test]
async fn killed_mid_index_commit_with_torn_header_rolls_back_then_queue_resumes() {
    killed_mid_index_commit_recovers(true).await;
}

async fn killed_mid_index_commit_recovers(torn_header: bool) {
    let base = tempfile::tempdir().unwrap();
    let state = base.path().join("state");
    let root = base.path().join("work");
    fs::create_dir(&root).unwrap();
    rewrite_workspace(&root, "before");
    let store = Store::open(roots(&state), identity(&root)).unwrap();
    let accepted = store
        .enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    drop(store);

    let announced =
        run_child_until_hook_then_kill("index_commit_hot_journal_daemon_child", &state, &root);
    let fields: Vec<&str> = announced.split(' ').collect();
    assert_eq!(fields.len(), 3, "{announced}");
    assert_eq!(
        fields[2], "true",
        "journal was not hot inside the transaction"
    );

    // The killed COMMIT left SQLite's genuine hot rollback journal behind.
    let index = index_db(&state, &root);
    let journal = index.with_file_name("index.db-journal");
    assert_eq!(
        fs::read(&journal).unwrap()[..8],
        SQLITE_JOURNAL_MAGIC,
        "the killed daemon must leave a hot journal"
    );
    // Until the daemon recovers it, a read-only reader cannot roll back: its
    // status is a typed busy refusal, never a view of the killed publication.
    let refused = Store::status_existing_readonly(roots(&state), identity(&root)).unwrap_err();
    assert!(
        refused.to_string().starts_with("storage_busy"),
        "{refused:#}"
    );
    let prior = trellis::model::IndexPin {
        index_generation: fields[0].parse().unwrap(),
        index_revision: fields[1].parse().unwrap(),
    };

    if torn_header {
        // Damage page 1's header in place (same inode); the journal holds
        // its original, since every write transaction changes page 1.
        use std::os::unix::fs::FileExt;
        let file = fs::OpenOptions::new().write(true).open(&index).unwrap();
        file.write_all_at(&[0u8; 100], 0).unwrap();
        file.sync_all().unwrap();
    }
    // A failure during recovery defers it: a typed error, the same index.db
    // inode and the journal kept, never a recreated index.
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let inode = fs::metadata(&index).unwrap().ino();
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o000)).unwrap();
    let deferred = Store::open(roots(&state), identity(&root))
        .map(|_| ())
        .unwrap_err();
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        deferred.to_string().starts_with("recovery_deferred"),
        "an I/O failure defers recovery: {deferred:#}"
    );
    assert_eq!(fs::metadata(&index).unwrap().ino(), inode, "{deferred:#}");
    assert_eq!(
        fs::read(&journal).unwrap()[..8],
        SQLITE_JOURNAL_MAGIC,
        "a deferred recovery keeps the hot journal: {deferred:#}"
    );

    // The queue resumes: the restarted daemon reclaims and finishes the request.
    let (_registry, _runtime) =
        restart_daemon_until_terminal(&state, &root, &accepted.id, |_| {}).await;
    let store = Store::open(roots(&state), identity(&root)).unwrap();
    let done = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(done.state, "done", "{done:?}");
    // Recovery rolled the hot journal back to exactly the prior committed head.
    assert!(
        !journal.exists(),
        "the restarted daemon recovered the hot journal"
    );
    let (pin, source) = store
        .source_at("m0.js", Some(prior))
        .unwrap()
        .expect("the prior head is intact after rollback");
    assert_eq!(pin, prior);
    assert!(source.text.contains("before_0_0") && !source.text.contains("after_0_0"));
    let committed = done.revision.unwrap();
    assert!(committed.index_revision > prior.index_revision);
    let (served, after) = store.symbols_at("after_0_0", 5).unwrap();
    assert!(served.index_revision >= committed.index_revision);
    assert!(
        after.iter().any(|symbol| symbol.name == "after_0_0"),
        "the resumed publication indexed the rewritten tree"
    );
}

#[test]
fn index_write_cold_journal_daemon_child() {
    let Some((state, root)) = child_paths() else {
        return;
    };
    let store = Store::open(roots(&state), identity(&root)).unwrap();
    let options = IndexOptions::new(root.clone());
    let (head, session) =
        trellis::index_coordinator::reconcile_workspace(&store, &options, &cancel(), |_| {})
            .unwrap();
    // The default publication cache holds this rewrite, so SQLite writes no
    // page into index.db and its FULL-sync journal header stays zeroed.
    rewrite_workspace(&root, "after");
    let journal = index_db(&state, &root).with_file_name("index.db-journal");
    store.set_publication_before_commit_hook_for_tests(move || {
        let header = fs::read(&journal).map(|bytes| bytes[..8].to_vec());
        announce_and_wait_for_kill(&format!(
            "HOOKED {} {} {}",
            head.index_generation,
            head.index_revision,
            matches!(header, Ok(bytes) if bytes == [0u8; 8])
        ));
    });
    let _ = trellis::index_coordinator::drain_requests(&store, &session);
    unreachable!("the publication hook never returns");
}

/// #220: a writer killed inside an index write transaction, before its commit
/// phase, leaves a cold journal (zeroed header, index.db untouched). SQLite
/// ignores it; the restarted daemon must reconcile and resume the queue, not
/// refuse every writable open with `recovery_required`.
#[tokio::test]
async fn killed_before_index_commit_leaves_cold_journal_then_queue_resumes() {
    let base = tempfile::tempdir().unwrap();
    let state = base.path().join("state");
    let root = base.path().join("work");
    fs::create_dir(&root).unwrap();
    rewrite_workspace(&root, "before");
    let store = Store::open(roots(&state), identity(&root)).unwrap();
    let accepted = store
        .enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    drop(store);

    let announced =
        run_child_until_hook_then_kill("index_write_cold_journal_daemon_child", &state, &root);
    let fields: Vec<&str> = announced.split(' ').collect();
    assert_eq!(fields.len(), 3, "{announced}");
    assert_eq!(
        fields[2], "true",
        "journal header was not zeroed inside the transaction"
    );
    let index = index_db(&state, &root);
    let journal = index.with_file_name("index.db-journal");
    let left = fs::read(&journal).expect("the killed writer must leave its journal");
    assert!(left.len() > 8, "journal holds page images: {}", left.len());
    assert_eq!(left[..8], [0u8; 8], "the killed writer left a cold journal");
    let prior = trellis::model::IndexPin {
        index_generation: fields[0].parse().unwrap(),
        index_revision: fields[1].parse().unwrap(),
    };

    let (_registry, _runtime) =
        restart_daemon_until_terminal(&state, &root, &accepted.id, |_| {}).await;
    let store = Store::open(roots(&state), identity(&root)).unwrap();
    let done = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(done.state, "done", "{done:?}");
    assert!(
        !journal.exists(),
        "the resumed write transaction consumed the cold journal"
    );
    let (pin, source) = store
        .source_at("m0.js", Some(prior))
        .unwrap()
        .expect("the prior head is intact");
    assert_eq!(pin, prior);
    assert!(source.text.contains("before_0_0") && !source.text.contains("after_0_0"));
    let committed = done.revision.unwrap();
    assert!(committed.index_revision > prior.index_revision);
    let (served, after) = store.symbols_at("after_0_0", 5).unwrap();
    assert!(served.index_revision >= committed.index_revision);
    assert!(
        after.iter().any(|symbol| symbol.name == "after_0_0"),
        "the resumed publication indexed the rewritten tree"
    );
}

/// A journal shorter than SQLite's magic is cold, never a hot journal to roll
/// back: first activation of an index with a corrupt header takes the same
/// path as with no journal at all, never `recovery_deferred`.
#[test]
fn short_cold_journal_does_not_defer_first_activation() {
    use std::os::unix::fs::FileExt;
    for journal_len in [None, Some(1), Some(7)] {
        let base = tempfile::tempdir().unwrap();
        let state = base.path().join("state");
        let root = base.path().join("work");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        drop(Store::open(roots(&state), identity(&root)).unwrap());
        let index = index_db(&state, &root);
        let file = fs::OpenOptions::new().write(true).open(&index).unwrap();
        file.write_all_at(&[0u8; 100], 0).unwrap();
        file.sync_all().unwrap();
        if let Some(len) = journal_len {
            fs::write(index.with_file_name("index.db-journal"), vec![0xd9; len]).unwrap();
        }
        // The corrupt header goes to the existing recreation path: the store
        // opens (recreate pending), exactly as with no journal.
        let opened = Store::open(roots(&state), identity(&root)).map(|_| ());
        assert!(opened.is_ok(), "{journal_len:?}: {opened:?}");
    }
}

/// #197: a reader outside the writer lease sees a live publication's journal
/// with SQLite's magic once the publication has spilled (and so synced the
/// journal) before COMMIT. That is a live writer, not a crash leftover: the
/// read path reports typed contention while the writer holds the index, then
/// reads the committed head. Only a writable open outside the lease, which
/// the test-support SQL helper is, refuses it as a hot journal.
#[test]
fn live_publication_journal_is_contention_for_a_reader_outside_the_lease() {
    let base = tempfile::tempdir().unwrap();
    let state = base.path().join("state");
    let root = base.path().join("work");
    fs::create_dir(&root).unwrap();
    rewrite_workspace(&root, "before");
    let store = Store::open(roots(&state), identity(&root)).unwrap();
    let accepted = store
        .enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    let (_, session) = trellis::index_coordinator::reconcile_workspace(
        &store,
        &IndexOptions::new(root.clone()),
        &cancel(),
        |_| {},
    )
    .unwrap();
    // A second Store stands in for another Trellis process: its own
    // publication gate, no writer lease.
    let reader = Store::open(roots(&state), identity(&root)).unwrap();
    rewrite_workspace(&root, "after");
    store.set_publication_cache_kib_for_tests(2000);
    let journal = index_db(&state, &root).with_file_name("index.db-journal");
    let (paused_tx, paused) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    store.set_publication_before_commit_hook_for_tests(move || {
        let header = fs::read(&journal).map(|bytes| bytes[..8].to_vec());
        paused_tx
            .send(matches!(header, Ok(bytes) if bytes == SQLITE_JOURNAL_MAGIC))
            .unwrap();
        release_rx.recv().unwrap();
    });
    let writer = std::thread::spawn(move || {
        trellis::index_coordinator::drain_requests(&store, &session).unwrap();
        store
    });
    assert!(
        paused.recv().unwrap(),
        "the live publication's journal did not carry SQLite's magic"
    );
    let count = |db: &rusqlite::Connection| -> anyhow::Result<i64> {
        Ok(
            db.query_row("SELECT count(*) FROM native_revisions", [], |row| {
                row.get(0)
            })?,
        )
    };
    // The flake's signature: a writable open refuses the live journal.
    let refused = reader.with_index_sql_for_tests(count).unwrap_err();
    assert!(
        refused
            .to_string()
            .starts_with("recovery_required: hot index journal"),
        "{refused:#}"
    );
    // The read path treats the same journal as the live writer it is: typed
    // contention (`rebuild_in_progress` here, since the Stores share this
    // process's publication gate; `storage_busy` from another process).
    let busy = reader.with_index_read_for_tests(count).unwrap_err();
    assert!(
        ["storage_busy", "rebuild_in_progress"]
            .iter()
            .any(|code| busy.to_string().starts_with(code)),
        "{busy:#}"
    );
    release.send(()).unwrap();
    let store = writer.join().unwrap();
    let done = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(done.state, "done", "{done:?}");
    assert!(reader.with_index_read_for_tests(count).unwrap() > 0);
}

/// A child test's libtest name in the grouped test binary, which names this
/// module's tests `<module>::<test>`.
fn child_test_name(test: &str) -> String {
    format!("{}::{test}", module_path!().split_once("::").unwrap().1)
}
