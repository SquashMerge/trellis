//! #122: a daemon process that is really killed (SIGKILL) at a hooked point
//! leaves only durable state; a restarted daemon runtime recovers from it
//! (Decision 0008 §3).
#![cfg(feature = "test-support")]
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use trellis::{
    daemon::registry::CheckoutRegistry,
    indexer::IndexOptions,
    store::{
        Store,
        requests::Request,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

const STATE_ENV: &str = "TRELLIS_TEST_CRASH_RECOVERY_STATE";
const ROOT_ENV: &str = "TRELLIS_TEST_CRASH_RECOVERY_ROOT";

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
    let mut child: Child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", child_test, "--nocapture", "--test-threads=1"])
        .env(STATE_ENV, state)
        .env(ROOT_ENV, root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    // libtest may print "test <name> ... " on the same line before the hook.
    while output.read_line(&mut line).unwrap() != 0 && !line.contains("HOOKED ") {
        line.clear();
    }
    assert!(
        line.contains("HOOKED "),
        "child exited before its hook: {:?}",
        child.wait()
    );
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

/// Activate a fresh daemon checkout runtime on the durable state and wait,
/// bounded only by the hang backstop, until `last` reaches a terminal row.
async fn restart_daemon_until_terminal(
    state: &Path,
    root: &Path,
    last: &str,
) -> (
    CheckoutRegistry,
    Arc<trellis::daemon::registry::CheckoutRuntime>,
) {
    let id = identity(root);
    let mut registry = CheckoutRegistry::with_roots(roots(state));
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
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

    let (_registry, _runtime) = restart_daemon_until_terminal(&state, &root, &accepted[3].id).await;

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
    // Terminal results follow FIFO (seq) order: each ID's single row names
    // its own later committed revision.
    let revisions: Vec<u64> = drained
        .iter()
        .map(|r| r.revision.unwrap().index_revision)
        .collect();
    assert!(
        revisions.windows(2).all(|pair| pair[0] < pair[1]),
        "terminal results out of FIFO order: {revisions:?}"
    );
    assert!(store.earliest_unfinished_request().unwrap().is_none());
}
