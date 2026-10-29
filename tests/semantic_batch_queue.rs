//! The batch request path end to end: CLI, durable queue, claim, staging, join and publication.
#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::{Builder, TempDir};
use trellis::{model::IndexPin, store::Store, store::topology::TopologyRoots};

#[path = "support/semantic_batch.rs"]
mod semantic_batch_support;
use semantic_batch_support::{
    BatchFixture, DocSpec, edit_descriptor, edit_scip, long_path_workspace, uniform_workspace,
};

const LANGUAGES: [&str; 4] = ["java", "rust", "python", "javascript"];

fn cli(home: &Path) -> Command {
    let mut cmd = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
    cmd.env("HOME", home)
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("daemon.pid"))
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null());
    cmd
}

fn data_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/dev.squashmerge.trellis")
    } else {
        home.join(".local/share/trellis")
    }
}

fn cache_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Caches/dev.squashmerge.trellis")
    } else {
        home.join(".cache/trellis")
    }
}

fn succeeded(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// One checkout with its published native revision, a running daemon and an open store.
struct World {
    _temp: TempDir,
    home: PathBuf,
    root: PathBuf,
    store: Store,
    index_db: PathBuf,
    batches: std::cell::Cell<usize>,
}

impl World {
    /// `env` applies to the daemon, which the first `index` call starts.
    fn new(setup: impl FnOnce(&Path), env: &[(&str, &Path)]) -> Self {
        let temp = Builder::new()
            .prefix("semantic-batch-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        setup(&root);
        fs::write(root.join(".gitignore"), ".trellis-semantic-batch*/\n").unwrap();
        let mut index = cli(&home);
        for (key, value) in env {
            index.env(key, value);
        }
        let indexed = succeeded(
            &index
                .args(["index", "--workspace", root.to_str().unwrap()])
                .output()
                .unwrap(),
        );
        assert!(indexed["publishedRevision"].is_object());
        let roots = TopologyRoots::isolated_for_tests(cache_dir(&home), data_dir(&home));
        let identity =
            trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let index_db = roots.index_db(&identity);
        let store = Store::open(roots, identity).unwrap();
        Self {
            _temp: temp,
            home,
            root,
            store,
            index_db,
            batches: std::cell::Cell::new(0),
        }
    }

    fn fixture(&self) -> BatchFixture {
        BatchFixture::from_published(&self.index_db, &self.root)
    }

    /// Writes `fixture` into a fresh directory under the checkout.
    fn write(&self, fixture: &BatchFixture) -> (PathBuf, String) {
        let number = self.batches.get();
        self.batches.set(number + 1);
        let before = self.head();
        let written = fixture.write(&self.root.join(format!(".trellis-semantic-batch-{number}")));
        self.settle(before);
        written
    }

    /// Writes while no daemon watches the checkout.
    fn write_unwatched(&self, fixture: &BatchFixture) -> (PathBuf, String) {
        let number = self.batches.get();
        self.batches.set(number + 1);
        fixture.write(&self.root.join(format!(".trellis-semantic-batch-{number}")))
    }

    /// The watcher publishes the new batch directory as a native revision. Wait for that and
    /// for the head to stop moving, so tests start from a quiet checkout.
    fn settle(&self, before: IndexPin) {
        trellis::testing::wait_until("native revision for the batch directory", || {
            self.head() != before
        });
        self.quiet();
    }

    fn quiet(&self) {
        let mut stable = 0;
        let mut last = self.head();
        while stable < 3 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let head = self.head();
            if head == last {
                stable += 1;
            } else {
                stable = 0;
                last = head;
            }
        }
    }

    fn import_command(&self, descriptor: &Path, sha: &str) -> Command {
        let mut command = cli(&self.home);
        command.args([
            "semantic",
            "import",
            "--workspace",
            self.root.to_str().unwrap(),
            "--batch",
            descriptor.to_str().unwrap(),
            "--batch-sha256",
            sha,
        ]);
        command
    }

    fn import(&self, descriptor: &Path, sha: &str) -> Output {
        self.import_command(descriptor, sha).output().unwrap()
    }

    fn head(&self) -> IndexPin {
        self.store.index_baseline().unwrap()
    }

    /// `(id, state, error_code)` of every queue row in submission order.
    fn queue(&self) -> Vec<(String, String, Option<String>)> {
        let db = rusqlite::Connection::open_with_flags(
            self.store.request_db_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        db.prepare("SELECT id,state,error_code FROM requests ORDER BY seq")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn count(&self, sql: &str) -> i64 {
        self.store
            .with_index_sql_for_tests(|db| Ok(db.query_row(sql, [], |row| row.get(0))?))
            .unwrap()
    }

    fn stop_daemon(&self) {
        let pid: i32 = fs::read_to_string(self.home.join("daemon.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        trellis::testing::wait_until("daemon exit", || !trellis::testing::process_running(pid));
        wait_for_daemon_election_free(&self.home);
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(self.home.join("daemon.pid"))
            && let Ok(pid) = pid.parse::<i32>()
        {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

fn four_language_workspace(root: &Path) {
    uniform_workspace(root, &LANGUAGES, 2);
}

/// A synthetic batch with one artifact per language, each covering every document of it.
fn four_language_batch(world: &World) -> (PathBuf, String) {
    let mut fixture = world.fixture().synthetic();
    for language in LANGUAGES {
        let docs = fixture.paths(language);
        fixture.artifact(language, docs);
    }
    world.write(&fixture)
}

fn wait_for(path: &Path, what: &str) {
    trellis::testing::wait_until(what, || path.exists());
}

#[test]
fn batch_import_of_1600_documents_with_over_one_mebibyte_of_pages_uses_one_queue_row() {
    let world = World::new(|root| long_path_workspace(root, &LANGUAGES, 400, 240), &[]);
    let (descriptor, sha) = four_language_batch(&world);
    let parsed: Value = serde_json::from_slice(&fs::read(&descriptor).unwrap()).unwrap();
    assert_eq!(parsed["totals"]["sourceDocuments"], 1600);
    assert!(
        parsed["totals"]["pageBytes"].as_u64().unwrap() > 1024 * 1024,
        "{}",
        parsed["totals"]
    );
    assert!(parsed["pages"].as_array().unwrap().len() > 2);
    let rows_before = world.queue().len();
    let imported = succeeded(&world.import(&descriptor, &sha));
    let queue = world.queue();
    assert_eq!(
        queue.len(),
        rows_before + 1,
        "one CLI call is one queue row"
    );
    assert_eq!(queue.last().unwrap().1, "done");
    let report = &imported["report"];
    assert_eq!(report["descriptorSha256"], sha);
    for language in report["languages"].as_array().unwrap() {
        assert_eq!(language["documents"], 400, "{language}");
    }
    assert_eq!(report["languages"].as_array().unwrap().len(), 4);
    assert!(serde_json::to_vec(report).unwrap().len() < 1024 * 1024);
}

#[test]
fn semantic_import_cli_rejects_invalid_batch_options_synchronously() {
    let world = World::new(four_language_workspace, &[]);
    let (descriptor, sha) = four_language_batch(&world);
    let rows = world.queue();
    let root = world.root.to_str().unwrap();
    let descriptor_text = descriptor.to_str().unwrap();
    let outside = world._temp.path().join("outside.json");
    fs::write(&outside, b"{}").unwrap();
    // The flags are both required.
    let missing_digest = cli(&world.home)
        .args([
            "semantic",
            "import",
            "--workspace",
            root,
            "--batch",
            descriptor_text,
        ])
        .output()
        .unwrap();
    assert!(!missing_digest.status.success());
    let missing_batch = cli(&world.home)
        .args([
            "semantic",
            "import",
            "--workspace",
            root,
            "--batch-sha256",
            &sha,
        ])
        .output()
        .unwrap();
    assert!(!missing_batch.status.success());
    // The removed version-one flags are no longer accepted.
    let removed = cli(&world.home)
        .args([
            "semantic",
            "import",
            "--workspace",
            root,
            "--scip",
            descriptor_text,
            "--manifest",
            descriptor_text,
            "--profile",
            "p",
            "--producer",
            "scip-typescript",
            "--manifest-sha256",
            &sha,
        ])
        .output()
        .unwrap();
    assert!(!removed.status.success());
    for (batch, digest) in [
        (descriptor_text.to_owned(), sha.to_uppercase()),
        (descriptor_text.to_owned(), sha[..63].to_owned()),
        (
            descriptor
                .with_file_name("missing.json")
                .display()
                .to_string(),
            sha.clone(),
        ),
        (outside.display().to_string(), sha.clone()),
    ] {
        let output = world
            .import_command(Path::new(&batch), &digest)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{batch} {digest}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("invalid_request"),
            "{batch} {digest}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // The daemon refuses a malformed frame the same way, without queueing it.
    for args in [
        json!({"version":2,"descriptor":descriptor_text}),
        json!({"version":2,"descriptor":descriptor_text,"descriptorSha256":sha,"extra":true}),
        json!({"version":3,"descriptor":descriptor_text,"descriptorSha256":sha}),
        json!({"version":1,"scip":descriptor_text,"manifest":descriptor_text,
            "profileId":"p","producerName":"scip-typescript","manifestSha256":sha}),
    ] {
        use std::os::unix::net::UnixStream;
        use trellis::daemon::protocol;
        let socket = trellis::daemon::SocketPaths::new(&data_dir(&world.home)).socket;
        let mut stream = UnixStream::connect(socket).unwrap();
        protocol::write_frame(
            &mut stream,
            &protocol::Request {
                id: 77,
                operation: "semantic_import".into(),
                payload: json!({"workspace":world.root,"args":args}),
            },
        )
        .unwrap();
        let reply: protocol::Reply = protocol::read_frame(&mut stream).unwrap();
        assert!(
            reply.payload["error"]
                .as_str()
                .is_some_and(|error| error.contains("invalid_request")),
            "{args}: {reply:?}"
        );
    }
    assert_eq!(world.queue(), rows, "no refused call created a queue row");
    assert!(world.store.earliest_unfinished_request().unwrap().is_none());
}

#[test]
fn four_language_synthetic_batch_publishes_one_combined_revision() {
    let world = World::new(four_language_workspace, &[]);
    let (descriptor, sha) = four_language_batch(&world);
    let before = world.head();
    let imported = succeeded(&world.import(&descriptor, &sha));
    let published: IndexPin =
        serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
    assert_eq!(published.index_revision, before.index_revision + 1);
    assert_eq!(world.head(), published);
    let batches = world.store.selected_raw_inputs_at(published).unwrap();
    assert_eq!(batches.len(), 1, "one producer, one batch");
    assert_eq!(batches[0].producer_id, "trellis-synthetic-scip");
    assert_eq!(batches[0].captures.len(), 4);
    let report = &imported["report"];
    assert_eq!(report["identityKind"], "generatorSource");
    assert_eq!(report["artifacts"].as_array().unwrap().len(), 4);
    assert_eq!(report["languages"].as_array().unwrap().len(), 4);
    assert!(report.get("documents").is_none());
    let snapshot = world.store.semantic_at(published).unwrap();
    assert_eq!(snapshot.coverage.len(), 8);
    assert!(snapshot.coverage.iter().all(|row| row.selected
        && row.requested
        && row.state == trellis::semantic_evidence::CoverageState::Complete));
    assert_eq!(
        world
            .store
            .semantic_batch_report_at(published, &sha)
            .unwrap()
            .unwrap(),
        *report
    );
}

#[test]
fn reader_sees_old_pin_until_batch_commit_then_complete_new_pin() {
    use std::os::unix::net::UnixStream;
    let home_marker = Builder::new()
        .prefix("semantic-pause-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = home_marker.path().join("pause.sock");
    let world = World::new(
        four_language_workspace,
        &[("TRELLIS_TEST_SEMANTIC_PAUSE_BEFORE_COMMIT_SOCKET", &socket)],
    );
    let (descriptor, sha) = four_language_batch(&world);
    let before = world.head();
    let prior = world.store.semantic_at(before).unwrap();
    let child = world
        .import_command(&descriptor, &sha)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(
        &socket.with_extension("ready"),
        "publication pause before COMMIT",
    );
    // The transaction is open and has written every row, but readers still see the old pin.
    assert_eq!(world.head(), before);
    assert_eq!(world.store.semantic_at(before).unwrap(), prior);
    assert!(
        world
            .store
            .selected_raw_inputs_at(before)
            .unwrap()
            .is_empty()
    );
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 0);
    drop(UnixStream::connect(&socket).unwrap());
    let imported = succeeded(&child.wait_with_output().unwrap());
    let published: IndexPin =
        serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
    assert_eq!(published.index_revision, before.index_revision + 1);
    assert_eq!(world.head(), published);
    let snapshot = world.store.semantic_at(published).unwrap();
    assert_eq!(snapshot.coverage.len(), 8);
    assert!(snapshot.coverage.iter().all(
        |row| row.selected && row.state == trellis::semantic_evidence::CoverageState::Complete
    ));
    assert_eq!(
        world.store.selected_raw_inputs_at(published).unwrap()[0]
            .captures
            .len(),
        4
    );
    assert_eq!(
        world.store.semantic_at(before).unwrap(),
        prior,
        "the old pin stays readable and unchanged"
    );
}

#[test]
fn precommit_sql_fault_leaves_prior_head_and_retry_publishes_once() {
    let home_marker = Builder::new()
        .prefix("semantic-fault-")
        .tempdir_in("/tmp")
        .unwrap();
    let marker = home_marker.path().join("sql-fault-once.marker");
    let world = World::new(
        four_language_workspace,
        &[("TRELLIS_TEST_SEMANTIC_SQL_FAULT", &marker)],
    );
    let (descriptor, sha) = four_language_batch(&world);
    let before = world.head();
    let failed = world.import(&descriptor, &sha);
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("index_failed"),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(marker.exists());
    assert_eq!(
        world.head(),
        before,
        "the failed publication left the prior head"
    );
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 0);
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_captures"), 0);
    assert!(
        world
            .store
            .selected_raw_inputs_at(before)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        world.queue().last().unwrap().2.as_deref(),
        Some("index_failed")
    );
    let retried = succeeded(&world.import(&descriptor, &sha));
    let published: IndexPin = serde_json::from_value(retried["publishedRevision"].clone()).unwrap();
    assert_eq!(published.index_revision, before.index_revision + 1);
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 1);
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_captures"), 4);
}

#[test]
fn ambiguous_batch_commit_is_resolved_by_durable_reread() {
    let home_marker = Builder::new()
        .prefix("semantic-ambiguous-")
        .tempdir_in("/tmp")
        .unwrap();
    let marker = home_marker.path().join("commit.marker");
    let world = World::new(
        four_language_workspace,
        &[("TRELLIS_TEST_SEMANTIC_AMBIGUOUS_COMMIT", &marker)],
    );
    let (descriptor, sha) = four_language_batch(&world);
    let before = world.head();
    let imported = succeeded(&world.import(&descriptor, &sha));
    assert!(marker.exists(), "the COMMIT result was reported lost");
    let published: IndexPin =
        serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
    assert_eq!(
        published.index_revision,
        before.index_revision + 1,
        "the durable re-read found the commit; nothing was published twice"
    );
    assert_eq!(world.head(), published);
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 1);
    let row = world.queue().pop().unwrap();
    assert_eq!((row.1.as_str(), row.2), ("done", None));
}

#[test]
fn busy_publication_requeues_without_terminal_regression() {
    let home_marker = Builder::new()
        .prefix("semantic-busy-")
        .tempdir_in("/tmp")
        .unwrap();
    let marker = home_marker.path().join("busy-once.marker");
    let world = World::new(
        four_language_workspace,
        &[("TRELLIS_TEST_SEMANTIC_BUSY_ONCE", &marker)],
    );
    let (descriptor, sha) = four_language_batch(&world);
    let before = world.head();
    let imported = succeeded(&world.import(&descriptor, &sha));
    assert!(marker.exists(), "the first publication attempt was busy");
    let queue = world.queue();
    let row = queue.last().unwrap();
    assert_eq!((row.1.as_str(), row.2.as_ref()), ("done", None));
    assert!(
        queue.iter().all(|row| row.1 == "done"),
        "contention never failed a request: {queue:?}"
    );
    let published: IndexPin =
        serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
    assert_eq!(published.index_revision, before.index_revision + 1);
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 1);
}

#[test]
fn commit_contention_requeues_the_request_instead_of_rejoining_or_failing() {
    let home_marker = Builder::new()
        .prefix("semantic-commit-busy-")
        .tempdir_in("/tmp")
        .unwrap();
    let marker = home_marker.path().join("commit-busy.marker");
    // The first COMMIT fails from contention and rolls back; the request is requeued typed
    // `storage_busy` instead of being re-joined (which would charge the lookup budget again).
    // This fixture costs 432 lookup steps per join. A limit of 631 admits one join but
    // not two, so an in-request re-join after the failed COMMIT hits `limit_exceeded`.
    let world = World::new(
        four_language_workspace,
        &[
            ("TRELLIS_TEST_SEMANTIC_COMMIT_BUSY", marker.as_path()),
            (
                "TRELLIS_TEST_SEMANTIC_LIMITS",
                Path::new(r#"{"lookup_steps":631}"#),
            ),
        ],
    );
    let (descriptor, sha) = four_language_batch(&world);
    let before = world.head();
    let imported = succeeded(&world.import(&descriptor, &sha));
    assert!(marker.exists(), "the COMMIT failed from contention once");
    let published: IndexPin =
        serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
    assert_eq!(published.index_revision, before.index_revision + 1);
    let queue = world.queue();
    assert!(
        queue.iter().all(|row| row.1 == "done" && row.2.is_none()),
        "contention is never a failed request: {queue:?}"
    );
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 1);
}

/// Waits until no process holds the daemon's election lock. A process that no longer looks
/// running can still hold the lock for a moment under load, and a successor started in that
/// window loses the election and exits, leaving the CLI talking to the dying daemon (which
/// ends in `outcome_unknown`). The lock itself is the condition a successor needs.
fn wait_for_daemon_election_free(home: &Path) {
    let lock = trellis::daemon::SocketPaths::new(&data_dir(home)).lock;
    wait_until_lock_free(&lock, || {});
}

/// `on_poll` runs once per attempt that finds the lock held.
fn wait_until_lock_free(lock: &Path, mut on_poll: impl FnMut()) {
    use std::os::fd::AsRawFd;
    trellis::testing::wait_until("daemon election lock released", || {
        let Ok(file) = fs::File::open(lock) else {
            return true;
        };
        // The flock lives on this open file description and ends when `file` drops.
        let free = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if !free {
            on_poll();
        }
        free
    });
}

#[test]
fn election_wait_returns_only_after_the_holder_releases_the_lock() {
    use std::os::fd::AsRawFd;
    let dir = Builder::new()
        .prefix("election-")
        .tempdir_in("/tmp")
        .unwrap();
    let lock = dir.path().join("daemon.lock");
    let held = fs::File::create(&lock).unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let mut held = Some(held);
    let mut polls = 0;
    wait_until_lock_free(&lock, || {
        polls += 1;
        if polls == 3 {
            drop(held.take());
        }
    });
    assert_eq!(polls, 3, "the wait outlasted the holder, then returned");
    assert!(held.is_none());
}

/// A daemon started on its own so its diagnostics reach a file.
struct LoggedDaemon {
    child: std::process::Child,
}

impl LoggedDaemon {
    fn start(home: &Path, log: &Path, env: &[(&str, &str)]) -> Self {
        wait_for_daemon_election_free(home);
        let mut command = cli(home);
        command
            .arg("daemon")
            .env("TRELLIS_INDEX_DIAGNOSTICS", "1")
            .stdout(Stdio::null())
            .stderr(fs::File::create(log).unwrap());
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let socket = trellis::daemon::SocketPaths::new(&data_dir(home)).socket;
        // Only the elected daemon records its PID. Wait for this process to be the elected one
        // before connecting, so the socket is its own and not a departing daemon's.
        let pid_file = home.join("daemon.pid");
        trellis::testing::wait_until("logged daemon elected", || {
            assert!(
                child.try_wait().unwrap().is_none(),
                "the logged daemon exited without being elected"
            );
            fs::read_to_string(&pid_file).is_ok_and(|pid| pid == child.id().to_string())
        });
        trellis::testing::wait_until("logged daemon listening", || {
            assert!(child.try_wait().unwrap().is_none());
            std::os::unix::net::UnixStream::connect(&socket).is_ok()
        });
        Self { child }
    }
}

impl Drop for LoggedDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn claim_time_causes_map_to_stable_terminal_codes() {
    // (cause logged by the diagnostics, daemon env, how the batch is made bad)
    type Spoil = fn(&World, &mut BatchFixture) -> Option<(PathBuf, String)>;
    type Case = (&'static str, &'static [(&'static str, &'static str)], Spoil);
    let cases: [Case; 4] = [
        (
            "invalid_request: closed descriptor",
            &[],
            |world, fixture| {
                let (descriptor, _) = world.write_unwatched(fixture);
                Some(edit_descriptor(&descriptor, |value| {
                    value["unexpected"] = json!(true);
                }))
            },
        ),
        ("invalid_artifact:", &[], |_, fixture| {
            fixture.override_artifact_raw(0, vec![0xff; 24]);
            None
        }),
        (
            "limit_exceeded: descriptor bytes",
            &[("TRELLIS_TEST_SEMANTIC_LIMITS", r#"{"descriptor_bytes":64}"#)],
            |_, _| None,
        ),
        ("capture_unavailable:", &[], |world, fixture| {
            let (descriptor, _) = world.write_unwatched(fixture);
            Some(edit_descriptor(&descriptor, |value| {
                value["sourceRevision"] = json!("revision:v2:never-captured");
            }))
        }),
    ];
    for (cause, env, spoil) in cases {
        let world = World::new(four_language_workspace, &[]);
        world.stop_daemon();
        let log = world._temp.path().join("daemon.log");
        let mut fixture = world.fixture().synthetic();
        for language in LANGUAGES {
            let docs = fixture.paths(language);
            fixture.artifact(language, docs);
        }
        let (descriptor, sha) = match spoil(&world, &mut fixture) {
            Some(edited) => edited,
            None => world.write_unwatched(&fixture),
        };
        let _daemon = LoggedDaemon::start(&world.home, &log, env);
        let output = world.import(&descriptor, &sha);
        assert!(!output.status.success(), "{cause}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("index_failed"),
            "{cause}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let row = world.queue().pop().unwrap();
        assert_eq!(
            (row.1.as_str(), row.2.as_deref()),
            ("failed", Some("index_failed")),
            "{cause}"
        );
        assert_eq!(
            world.count("SELECT count(*) FROM semantic_raw_batches"),
            0,
            "{cause}: nothing was published"
        );
        trellis::testing::wait_until("diagnostic cause", || {
            fs::read_to_string(&log)
                .is_ok_and(|text| text.contains(&format!("request_failure={cause}")))
        });
    }
}

#[test]
fn queued_version_one_row_at_startup_settles_index_failed_and_later_request_completes() {
    let world = World::new(four_language_workspace, &[]);
    world.stop_daemon();
    let log = world._temp.path().join("daemon.log");
    // A durable version-one row, as an older build would have queued it.
    let v1 = world
        .store
        .enqueue_semantic_options_json_for_tests(
            &world.root,
            &json!({"version":1,"scip":world.root.join("artifact.scip"),
                "manifest":world.root.join("manifest.json"),"profileId":"mac-mini-m5-pro-v1",
                "producerName":"scip-typescript","manifestSha256":"a".repeat(64)}),
        )
        .unwrap();
    assert_eq!(v1.state, "queued");
    let mut fixture = world.fixture().synthetic();
    for language in LANGUAGES {
        let docs = fixture.paths(language);
        fixture.artifact(language, docs);
    }
    let (descriptor, sha) = world.write_unwatched(&fixture);
    let _daemon = LoggedDaemon::start(&world.home, &log, &[]);
    // The new daemon reads the FIFO head at startup without decoding it, claims it, fails it
    // typed, and only then reaches the later request.
    let imported = succeeded(&world.import(&descriptor, &sha));
    let published: IndexPin =
        serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
    assert_eq!(world.head(), published);
    let queue = world.queue();
    let old = queue.iter().find(|row| row.0 == v1.id).unwrap();
    assert_eq!(
        (old.1.as_str(), old.2.as_deref()),
        ("failed", Some("index_failed"))
    );
    trellis::testing::wait_until("v1 diagnostic cause", || {
        fs::read_to_string(&log).is_ok_and(|text| {
            text.contains("request_failure=invalid_request: semantic import options")
        })
    });
    let last = queue.last().unwrap();
    assert_ne!(last.0, v1.id);
    assert_eq!((last.1.as_str(), last.2.as_deref()), ("done", None));
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 1);
}

/// Entries of the index directory that belong to a batch stage.
fn stage_entries(world: &World) -> Vec<String> {
    fs::read_dir(world.index_db.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("semantic-stage-"))
        .collect()
}

fn four_language_fixture(world: &World) -> BatchFixture {
    let mut fixture = world.fixture().synthetic();
    for language in LANGUAGES {
        let docs = fixture.paths(language);
        fixture.artifact(language, docs);
    }
    fixture
}

/// Imports the batch on a fresh daemon with `env` and asserts the claim fails typed: terminal
/// `index_failed`, the diagnostic `cause`, nothing published and no stage left.
fn assert_claim_fails(
    world: &World,
    log: &Path,
    batch: &(PathBuf, String),
    env: &[(&str, &str)],
    cause: &str,
) {
    let daemon = LoggedDaemon::start(&world.home, log, env);
    let output = world.import(&batch.0, &batch.1);
    assert!(!output.status.success(), "{cause}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("index_failed"),
        "{cause}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let row = world.queue().pop().unwrap();
    assert_eq!(
        (row.1.as_str(), row.2.as_deref()),
        ("failed", Some("index_failed")),
        "{cause}"
    );
    assert_eq!(
        world.count("SELECT count(*) FROM semantic_raw_batches"),
        0,
        "{cause}: nothing was published"
    );
    assert!(
        world
            .store
            .selected_raw_inputs_at(world.head())
            .unwrap()
            .is_empty(),
        "{cause}: the head selects no batch"
    );
    assert_eq!(stage_entries(world), Vec::<String>::new(), "{cause}");
    trellis::testing::wait_until("diagnostic cause", || {
        fs::read_to_string(log).is_ok_and(|text| text.contains("request_failure="))
    });
    let text = fs::read_to_string(log).unwrap();
    assert!(
        text.contains(&format!("request_failure={cause}")),
        "{cause}: {text}"
    );
    drop(daemon);
}

type Fault = fn(&World) -> (PathBuf, String);

/// Runs each `(cause, make)` case on its own daemon against one checkout.
fn run_fault_cases(cases: &[(&str, Fault)]) {
    let world = World::new(four_language_workspace, &[]);
    world.stop_daemon();
    for (number, (cause, make)) in cases.iter().enumerate() {
        let batch = make(&world);
        let log = world._temp.path().join(format!("daemon-{number}.log"));
        assert_claim_fails(&world, &log, &batch, &[], cause);
    }
}

fn paged_fixture(world: &World) -> BatchFixture {
    four_language_fixture(world).rows_per_page(3)
}

/// Page indexes of `paged_fixture`: source pages 0..=2, then coverage pages 3..=5.
fn paged_batch(
    world: &World,
    edit: impl FnOnce(&Path, &Path) -> (PathBuf, String),
) -> (PathBuf, String) {
    let (descriptor, _) = world.write_unwatched(&paged_fixture(world));
    edit(&descriptor, &world.root)
}

#[test]
fn page_faults_fail_typed_through_production_admission() {
    run_fault_cases(&[
        ("invalid_request: page ordinals", |world| {
            paged_batch(world, |descriptor, _| {
                edit_descriptor(descriptor, |value| {
                    value["pages"].as_array_mut().unwrap().remove(1);
                })
            })
        }),
        ("invalid_request: page ordinals", |world| {
            paged_batch(world, |descriptor, _| {
                edit_descriptor(descriptor, |value| {
                    value["pages"].as_array_mut().unwrap().swap(0, 1);
                })
            })
        }),
        ("invalid_request: page order", |world| {
            paged_batch(world, |descriptor, _| {
                edit_descriptor(descriptor, |value| {
                    value["pages"].as_array_mut().unwrap().swap(2, 3);
                })
            })
        }),
        ("invalid_request: reordered page", |world| {
            paged_batch(world, |descriptor, root| {
                semantic_batch_support::edit_page(descriptor, 1, root, |page| {
                    page["ordinal"] = json!(0);
                })
            })
        }),
        ("invalid_request: page rows", |world| {
            paged_batch(world, |descriptor, root| {
                semantic_batch_support::edit_page(descriptor, 0, root, |page| {
                    page["rows"].as_array_mut().unwrap().pop();
                })
            })
        }),
        ("invalid_request: page ordinals", |world| {
            paged_batch(world, |descriptor, _| {
                edit_descriptor(descriptor, |value| {
                    let pages = value["pages"].as_array_mut().unwrap();
                    let repeated = pages[0].clone();
                    pages.insert(1, repeated);
                })
            })
        }),
        ("invalid_request: page totals", |world| {
            paged_batch(world, |descriptor, _| {
                edit_descriptor(descriptor, |value| {
                    value["totals"]["coveredDocuments"] = json!(7);
                })
            })
        }),
        ("invalid_request: duplicate source path", |world| {
            paged_batch(world, |descriptor, root| {
                semantic_batch_support::edit_page(descriptor, 0, root, |page| {
                    page["rows"][1] = page["rows"][0].clone();
                })
            })
        }),
        ("invalid_request: duplicate coverage row", |world| {
            paged_batch(world, |descriptor, root| {
                semantic_batch_support::edit_page(descriptor, 3, root, |page| {
                    page["rows"][1] = page["rows"][0].clone();
                })
            })
        }),
        ("invalid_request: unbound coverage path", |world| {
            paged_batch(world, |descriptor, root| {
                semantic_batch_support::edit_page(descriptor, 3, root, |page| {
                    let path = page["rows"][2]["relativePath"].as_str().unwrap().to_owned();
                    page["rows"][2]["relativePath"] = json!(format!("{path}z"));
                })
            })
        }),
        ("invalid_artifact: unlisted document", |world| {
            let mut fixture = four_language_fixture(world);
            let docs = fixture.paths("java");
            fixture.artifact("java", docs[..1].to_vec());
            world.write_unwatched(&fixture)
        }),
    ]);
}

#[test]
fn hash_faults_fail_typed_through_production_admission() {
    run_fault_cases(&[
        ("invalid_request: page digest", |world| {
            paged_batch(world, |descriptor, _| {
                edit_descriptor(descriptor, |value| {
                    value["pages"][0]["sha256"] = json!("0".repeat(64));
                })
            })
        }),
        ("invalid_request: page length", |world| {
            paged_batch(world, |descriptor, _| {
                edit_descriptor(descriptor, |value| {
                    let length = value["pages"][0]["byteLength"].as_u64().unwrap();
                    let total = value["totals"]["pageBytes"].as_u64().unwrap();
                    value["pages"][0]["byteLength"] = json!(length + 1);
                    value["totals"]["pageBytes"] = json!(total + 1);
                })
            })
        }),
        ("invalid_request: artifact digest", |world| {
            let (descriptor, _) = world.write_unwatched(&four_language_fixture(world));
            edit_descriptor(&descriptor, |value| {
                value["artifacts"][0]["sha256"] = json!("0".repeat(64));
            })
        }),
        ("invalid_request: artifact length", |world| {
            let (descriptor, _) = world.write_unwatched(&four_language_fixture(world));
            edit_descriptor(&descriptor, |value| {
                let length = value["artifacts"][0]["byteLength"].as_u64().unwrap();
                let total = value["totals"]["artifactBytes"].as_u64().unwrap();
                value["artifacts"][0]["byteLength"] = json!(length + 1);
                value["totals"]["artifactBytes"] = json!(total + 1);
            })
        }),
    ]);
}

#[test]
fn malformed_final_artifact_after_valid_artifacts_publishes_nothing() {
    run_fault_cases(&[("invalid_artifact:", |world| {
        let mut fixture = four_language_fixture(world);
        assert_eq!(fixture.artifact("java", vec![]), 4, "a fifth artifact");
        fixture.override_artifact_raw(4, vec![0xff; 24]);
        world.write_unwatched(&fixture)
    })]);
}

/// A batch whose artifact for `language` is edited after generation, next to ordinary ones.
fn batch_with_artifact_edit(
    world: &World,
    language: &str,
    edit: impl FnOnce(&mut scip::types::Index),
) -> (PathBuf, String) {
    let mut edit = Some(edit);
    let mut fixture = world.fixture().synthetic();
    for other in LANGUAGES {
        let docs = fixture.paths(other);
        if other == language {
            let bytes = edit_scip(&fixture.scip_bytes(other, &docs), edit.take().unwrap());
            let ordinal = fixture.artifact(other, docs);
            fixture.override_artifact_raw(ordinal, bytes);
        } else {
            fixture.artifact(other, docs);
        }
    }
    world.write_unwatched(&fixture)
}

#[test]
fn each_batch_limit_fails_limit_exceeded_before_expansion() {
    // (limits override, cause, how the batch is made)
    type Case = (&'static str, &'static str, Fault);
    let ordinary: Fault = |world| world.write_unwatched(&paged_fixture(world));
    let cases: [Case; 23] = [
        (r#"{"source_documents":4}"#, "source documents", ordinary),
        (r#"{"descriptor_bytes":64}"#, "descriptor bytes", ordinary),
        (r#"{"page_bytes":50}"#, "page bytes", ordinary),
        (r#"{"page_rows":2}"#, "page rows", ordinary),
        (r#"{"pages":2}"#, "pages", ordinary),
        (r#"{"total_page_bytes":100}"#, "total page bytes", ordinary),
        (r#"{"artifacts":2}"#, "artifacts", ordinary),
        (r#"{"artifact_bytes":10}"#, "artifact bytes", ordinary),
        (
            r#"{"total_artifact_bytes":10}"#,
            "total artifact bytes",
            ordinary,
        ),
        (r#"{"candidates":1}"#, "candidate facts", ordinary),
        (r#"{"wire_fields":3}"#, "wire fields", ordinary),
        (r#"{"unit_expansion_bytes":8}"#, "unit expansion", ordinary),
        (
            r#"{"document_occurrences":0}"#,
            "document entries",
            ordinary,
        ),
        (r#"{"document_symbols":0}"#, "document entries", ordinary),
        (r#"{"message_depth":0}"#, "message depth", ordinary),
        (r#"{"staged_rows":1}"#, "staged rows", ordinary),
        (r#"{"output_bytes":0}"#, "output bytes", ordinary),
        (r#"{"lookup_steps":1}"#, "lookup steps", ordinary),
        (r#"{"staging_bytes":1}"#, "staging bytes", ordinary),
        (
            r#"{"disk_growth_bytes":0}"#,
            "operation disk growth",
            ordinary,
        ),
        (r#"{"external_symbols":0}"#, "index entries", |world| {
            batch_with_artifact_edit(world, "java", |index| {
                index
                    .external_symbols
                    .push(scip::types::SymbolInformation::new());
            })
        }),
        (
            r#"{"signature_occurrences":0}"#,
            "signature occurrences",
            |world| {
                batch_with_artifact_edit(world, "java", |index| {
                    let signature = &mut index.documents[0].symbols[0].signature_documentation;
                    let mut document = scip::types::Signature::new();
                    document.occurrences.push(scip::types::Occurrence::new());
                    *signature = Some(document).into();
                })
            },
        ),
        (
            r#"{"relationships_per_artifact":0}"#,
            "relationships",
            |world| {
                let mut fixture = world.fixture().synthetic();
                for language in LANGUAGES {
                    let mut docs = fixture.paths(language);
                    if language == "rust" {
                        docs[0] = DocSpec::new(&docs[0].path).symbol("pkg a.", 26, &["pkg b."]);
                    }
                    fixture.artifact(language, docs);
                }
                world.write_unwatched(&fixture)
            },
        ),
    ];
    let world = World::new(four_language_workspace, &[]);
    world.stop_daemon();
    for (number, (limits, name, make)) in cases.iter().enumerate() {
        let batch = make(&world);
        let log = world._temp.path().join(format!("daemon-{number}.log"));
        let cause = format!("limit_exceeded: {name}");
        assert_claim_fails(
            &world,
            &log,
            &batch,
            &[("TRELLIS_TEST_SEMANTIC_LIMITS", limits)],
            &cause,
        );
    }
}

#[test]
fn full_disk_during_staging_fails_index_failed_with_prior_head() {
    let world = World::new(four_language_workspace, &[]);
    let batch = four_language_batch(&world);
    let prior = world.head();
    world.stop_daemon();
    let log = world._temp.path().join("daemon.log");
    let daemon = LoggedDaemon::start(
        &world.home,
        &log,
        &[("TRELLIS_TEST_SEMANTIC_STAGE_ENOSPC_AFTER_BYTES", "1")],
    );
    let output = world.import(&batch.0, &batch.1);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("index_failed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let row = world.queue().pop().unwrap();
    assert_eq!(
        (row.1.as_str(), row.2.as_deref()),
        ("failed", Some("index_failed"))
    );
    assert_eq!(world.head(), prior, "the prior head stays");
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 0);
    assert_eq!(stage_entries(&world), Vec::<String>::new());
    trellis::testing::wait_until("storage cause", || {
        fs::read_to_string(&log).is_ok_and(|text| {
            text.contains("request_failure=") && text.contains("No space left on device")
        })
    });
    drop(daemon);
}

#[test]
fn full_disk_at_commit_fails_index_failed_with_prior_head() {
    let marker = Builder::new()
        .prefix("semantic-commit-full-")
        .tempdir_in("/tmp")
        .unwrap();
    let world = World::new(four_language_workspace, &[]);
    let batch = four_language_batch(&world);
    let prior = world.head();
    world.stop_daemon();
    let log = world._temp.path().join("daemon.log");
    let disk_full = marker.path().join("commit-disk-full.marker");
    let daemon = LoggedDaemon::start(
        &world.home,
        &log,
        &[(
            "TRELLIS_TEST_SEMANTIC_COMMIT_DISK_FULL",
            disk_full.to_str().unwrap(),
        )],
    );
    // The first COMMIT hits a full disk and rolls back. Re-joining cannot help, so the request
    // fails with the real cause instead of being retried into a second attempt.
    let output = world.import(&batch.0, &batch.1);
    assert!(!output.status.success());
    assert!(disk_full.exists(), "the COMMIT failed once");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("index_failed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let row = world.queue().pop().unwrap();
    assert_eq!(
        (row.1.as_str(), row.2.as_deref()),
        ("failed", Some("index_failed"))
    );
    assert_eq!(world.head(), prior, "the prior head stays");
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 0);
    assert_eq!(stage_entries(&world), Vec::<String>::new());
    trellis::testing::wait_until("storage cause", || {
        fs::read_to_string(&log).is_ok_and(|text| text.contains("database or disk is full"))
    });
    drop(daemon);
}

/// Waits for the staging hook, which fires once artifact 0 is staged.
fn staged_first_artifact(world: &World, socket: &Path) {
    wait_for(&socket.with_extension("ready"), "artifact 0 staged");
    assert_eq!(world.queue().last().unwrap().1, "running");
    assert_eq!(stage_entries(world).len(), 1, "the claim owns one stage");
}

#[test]
fn cancellation_during_staging_publishes_nothing_and_request_completes_after_restart() {
    let marker = Builder::new()
        .prefix("semantic-staging-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = marker.path().join("staging.sock");
    let world = World::new(
        four_language_workspace,
        &[("TRELLIS_TEST_SEMANTIC_STAGING_SOCKET", &socket)],
    );
    let (descriptor, sha) = four_language_batch(&world);
    let prior = world.head();
    let mut child = world
        .import_command(&descriptor, &sha)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    staged_first_artifact(&world, &socket);
    let pid: i32 = fs::read_to_string(world.home.join("daemon.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    trellis::testing::wait_until("daemon exit", || !trellis::testing::process_running(pid));
    let _ = child.wait();
    assert_eq!(world.head(), prior);
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 0);
    let pending = world.queue().pop().unwrap();
    assert!(
        matches!(pending.1.as_str(), "queued" | "running"),
        "the accepted request is not terminal: {pending:?}"
    );
    // The next daemon reclaims the accepted request; the hook stays out of its way.
    fs::write(socket.with_extension("done"), []).unwrap();
    let restarted = succeeded(
        &cli(&world.home)
            .env("TRELLIS_TEST_SEMANTIC_STAGING_SOCKET", &socket)
            .args(["index", "--workspace", world.root.to_str().unwrap()])
            .output()
            .unwrap(),
    );
    assert!(restarted["publishedRevision"].is_object());
    let accepted = world
        .queue()
        .into_iter()
        .find(|row| row.0 == pending.0)
        .unwrap();
    assert_eq!((accepted.1.as_str(), accepted.2), ("done", None));
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 1);
    assert_eq!(stage_entries(&world), Vec::<String>::new());
}

/// Kills the daemon the CLI started, as a crash or power loss would.
fn kill_daemon(world: &World) {
    let pid: i32 = fs::read_to_string(world.home.join("daemon.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    trellis::testing::wait_until("daemon death", || !trellis::testing::process_running(pid));
}

/// A new daemon starts under `env`, and the CLI waits for its queue to drain through an
/// explicit native index.
fn restart_and_index(world: &World, env: &[(&str, &Path)]) -> Value {
    let mut command = cli(&world.home);
    for (key, value) in env {
        command.env(key, value);
    }
    succeeded(
        &command
            .args(["index", "--workspace", world.root.to_str().unwrap()])
            .output()
            .unwrap(),
    )
}

#[test]
fn crash_during_staging_discards_stage_and_reruns_request() {
    let marker = Builder::new()
        .prefix("semantic-crash-staging-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = marker.path().join("staging.sock");
    let world = World::new(
        four_language_workspace,
        &[("TRELLIS_TEST_SEMANTIC_STAGING_SOCKET", &socket)],
    );
    let (descriptor, sha) = four_language_batch(&world);
    let prior = world.head();
    let mut child = world
        .import_command(&descriptor, &sha)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    staged_first_artifact(&world, &socket);
    let abandoned = world
        .index_db
        .parent()
        .unwrap()
        .join(&stage_entries(&world)[0]);
    assert!(abandoned.join("artifact-0.scip").exists());
    kill_daemon(&world);
    let _ = child.wait();
    assert!(abandoned.exists(), "a crash leaves the stage behind");
    assert_eq!(world.head(), prior);
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 0);
    let claimed = world.queue().pop().unwrap();
    assert_eq!(claimed.1, "running");
    // The next daemon sweeps the stage before its first claim, then re-runs the request from
    // the caller's files under a new incarnation.
    fs::write(socket.with_extension("done"), []).unwrap();
    let restarted = restart_and_index(&world, &[("TRELLIS_TEST_SEMANTIC_STAGING_SOCKET", &socket)]);
    assert!(restarted["publishedRevision"].is_object());
    assert!(!abandoned.exists(), "the startup sweep removed the stage");
    let rerun = world
        .queue()
        .into_iter()
        .find(|row| row.0 == claimed.0)
        .unwrap();
    assert_eq!((rerun.1.as_str(), rerun.2), ("done", None));
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 1);
    assert_eq!(stage_entries(&world), Vec::<String>::new());
}

#[test]
fn crash_after_publish_before_completion_completes_without_terminal_regression() {
    let marker = Builder::new()
        .prefix("semantic-crash-published-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = marker.path().join("published.sock");
    let world = World::new(
        four_language_workspace,
        &[("TRELLIS_TEST_SEMANTIC_CRASH_AFTER_PUBLISH_SOCKET", &socket)],
    );
    let (descriptor, sha) = four_language_batch(&world);
    let before = world.head();
    let mut child = world
        .import_command(&descriptor, &sha)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&socket, "publication barrier");
    // The batch is durable, but the request has not been completed.
    let published = world.head();
    assert_eq!(published.index_revision, before.index_revision + 1);
    assert!(
        world
            .store
            .semantic_batch_report_at(published, &sha)
            .unwrap()
            .is_some()
    );
    let accepted = world.queue().pop().unwrap();
    assert_eq!(accepted.1, "running");
    kill_daemon(&world);
    let _ = child.wait();
    fs::write(socket.with_extension("done"), []).unwrap();
    fs::remove_file(&socket).unwrap();
    let restarted = restart_and_index(
        &world,
        &[("TRELLIS_TEST_SEMANTIC_CRASH_AFTER_PUBLISH_SOCKET", &socket)],
    );
    assert!(restarted["publishedRevision"].is_object());
    let completed = world
        .queue()
        .into_iter()
        .find(|row| row.0 == accepted.0)
        .unwrap();
    assert_eq!((completed.1.as_str(), completed.2), ("done", None));
    let row = world.store.request_by_id(&accepted.0).unwrap().unwrap();
    let revision = row.revision.unwrap();
    assert!(revision.index_revision >= published.index_revision);
    assert!(
        world
            .store
            .semantic_batch_report_at(revision, &sha)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        world.count("SELECT count(*) FROM semantic_raw_batches"),
        1,
        "the replay reused the published batch"
    );
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_captures"), 4);
}

#[test]
fn rebuild_preserves_queued_batch_request() {
    use trellis::store::requests::SemanticImportOptions;
    let world = World::new(four_language_workspace, &[]);
    let (descriptor, sha) = four_language_batch(&world);
    let generation = world.head().index_generation;
    world.stop_daemon();
    let queued = world
        .store
        .enqueue_semantic_import(
            &world.root,
            &SemanticImportOptions {
                version: 2,
                descriptor: descriptor.display().to_string(),
                descriptor_sha256: sha.clone(),
            },
        )
        .unwrap();
    assert_eq!(queued.state, "queued");
    // Corrupt the index so the next daemon rebuilds it from the checkout.
    let db = rusqlite::Connection::open(&world.index_db).unwrap();
    db.execute(
        "UPDATE index_metadata SET reconcile_options='not-json' WHERE singleton=1",
        [],
    )
    .unwrap();
    drop(db);
    let log = world._temp.path().join("daemon.log");
    let _daemon = LoggedDaemon::start(&world.home, &log, &[]);
    let indexed = succeeded(
        &cli(&world.home)
            .args(["index", "--workspace", world.root.to_str().unwrap()])
            .output()
            .unwrap(),
    );
    let rebuilt: IndexPin = serde_json::from_value(indexed["publishedRevision"].clone()).unwrap();
    assert_ne!(
        rebuilt.index_generation, generation,
        "the index was rebuilt"
    );
    let row = world
        .queue()
        .into_iter()
        .find(|row| row.0 == queued.id)
        .expect("the queued request survived the rebuild");
    assert_eq!((row.1.as_str(), row.2), ("done", None));
    let revision = world
        .store
        .request_by_id(&queued.id)
        .unwrap()
        .unwrap()
        .revision
        .unwrap();
    assert_eq!(revision.index_generation, rebuilt.index_generation);
    assert!(
        world
            .store
            .semantic_batch_report_at(revision, &sha)
            .unwrap()
            .is_some()
    );
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 1);
}

#[test]
fn replaced_root_settles_batch_request_root_changed() {
    use trellis::store::requests::SemanticImportOptions;
    let world = World::new(four_language_workspace, &[]);
    let (descriptor, sha) = four_language_batch(&world);
    world.stop_daemon();
    let options = SemanticImportOptions {
        version: 2,
        descriptor: descriptor.display().to_string(),
        descriptor_sha256: sha,
    };
    let accepted = world
        .store
        .enqueue_semantic_import(&world.root, &options)
        .unwrap();
    assert_eq!(accepted.state, "queued");
    // A different directory now stands at the checkout path.
    let moved = world._temp.path().join("old-checkout");
    fs::rename(&world.root, &moved).unwrap();
    fs::create_dir(&world.root).unwrap();
    fs::write(world.root.join("A.js"), "function g(){}\n").unwrap();
    fs::create_dir_all(descriptor.parent().unwrap()).unwrap();
    fs::write(&descriptor, b"new root descriptor").unwrap();
    assert_eq!(world.store.fail_changed_root_requests().unwrap(), 1);
    // The terminal row stays put when the same options are queued again for the new root.
    let roots = TopologyRoots::isolated_for_tests(cache_dir(&world.home), data_dir(&world.home));
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&world.root), &world.root)
            .unwrap();
    let fresh_store = Store::open(roots, identity).unwrap();
    let settled = fresh_store
        .request_any_root_for_tests(&accepted.id)
        .unwrap()
        .unwrap();
    assert_eq!(settled.state, "failed");
    assert_eq!(settled.error_code.as_deref(), Some("root_changed"));
    assert!(settled.revision.is_none());
    let fresh = fresh_store
        .enqueue_semantic_import(&world.root, &options)
        .unwrap();
    assert_eq!(fresh.state, "queued");
    assert_ne!(fresh.id, accepted.id);
    assert_eq!(
        fresh_store
            .request_any_root_for_tests(&accepted.id)
            .unwrap()
            .unwrap()
            .error_code
            .as_deref(),
        Some("root_changed")
    );
}

/// A batch captured at the first native revision, a source edit that moves the head, and an
/// import whose destination is therefore newer than its captured revision. The daemon (started
/// under `env`) pauses at the watcher cutoff; `edit` runs while it is paused. Returns the
/// import's output and the head the import joined against first.
fn import_with_cutoff_edit(
    env: &[(&str, &Path)],
    socket: &Path,
    edit: impl FnOnce(&World),
) -> (World, Output, IndexPin, PathBuf, String) {
    use std::os::unix::net::UnixStream;
    let world = World::new(four_language_workspace, env);
    let (descriptor, sha) = four_language_batch(&world);
    world.stop_daemon();
    fs::write(
        world.root.join("java/u00000.java"),
        "class U00000 { void f() { h(); } }\n",
    )
    .unwrap();
    let advanced: IndexPin =
        serde_json::from_value(restart_and_index(&world, env)["publishedRevision"].clone())
            .unwrap();
    let child = world
        .import_command(&descriptor, &sha)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&socket.with_extension("ready"), "watcher cutoff");
    assert_eq!(world.head(), advanced);
    edit(&world);
    drop(UnixStream::connect(socket).unwrap());
    let output = child.wait_with_output().unwrap();
    (world, output, advanced, descriptor, sha)
}

#[test]
fn watcher_cutoff_edit_after_staging_rejoins_against_new_destination() {
    let marker = Builder::new()
        .prefix("semantic-cutoff-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = marker.path().join("cutoff.sock");
    let (world, output, advanced, _, sha) = import_with_cutoff_edit(
        &[("TRELLIS_TEST_SEMANTIC_CUTOFF_SOCKET", &socket)],
        &socket,
        |world| {
            fs::write(
                world.root.join("rust/u00000.rs"),
                "fn u00000() { late(); }\n",
            )
            .unwrap();
        },
    );
    let imported = succeeded(&output);
    let published: IndexPin =
        serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
    assert!(
        published.index_revision >= advanced.index_revision + 2,
        "the late edit was reconciled natively before the batch published"
    );
    assert_eq!(world.head(), published);
    assert_eq!(imported["report"]["descriptorSha256"], sha);
    let batches = world.store.selected_raw_inputs_at(published).unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].captures.len(), 4);
    assert_ne!(
        batches[0].captured_pin, published,
        "the batch stays bound to its older captured revision"
    );
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 1);
    let queue = world.queue();
    assert!(queue.iter().all(|row| row.1 == "done" && row.2.is_none()));
}

#[test]
fn lookup_budget_spans_the_rejoin_after_a_cutoff_edit() {
    let marker = Builder::new()
        .prefix("semantic-cutoff-budget-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = marker.path().join("cutoff.sock");
    // This fixture costs 419 lookup steps before the late edit and 398 afterwards. The
    // limit admits either join alone, but not their cumulative in-request work (817).
    let (world, output, advanced, _, _) = import_with_cutoff_edit(
        &[
            ("TRELLIS_TEST_SEMANTIC_CUTOFF_SOCKET", &socket),
            (
                "TRELLIS_TEST_SEMANTIC_LIMITS",
                Path::new(r#"{"lookup_steps":631}"#),
            ),
        ],
        &socket,
        |world| {
            fs::write(
                world.root.join("rust/u00000.rs"),
                "fn u00000() { late(); }\n",
            )
            .unwrap();
        },
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("index_failed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let row = world.queue().pop().unwrap();
    assert_eq!(
        (row.1.as_str(), row.2.as_deref()),
        ("failed", Some("index_failed"))
    );
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 0);
    // The request can fail on a watcher generation change before the watcher has published the
    // late edit; the watcher publishes it afterwards, and the durable head shows when it has.
    trellis::testing::wait_until("late edit published by the watcher", || {
        world.head().index_revision > advanced.index_revision
    });
}

#[test]
fn commit_contention_after_in_loop_watcher_publication_requeues_and_completes() {
    use std::os::unix::net::UnixStream;
    let marker = Builder::new()
        .prefix("semantic-cutoff-busy-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = marker.path().join("cutoff.sock");
    let busy = marker.path().join("commit-busy.marker");
    let world = World::new(four_language_workspace, &[]);
    let (descriptor, sha) = four_language_batch(&world);
    world.stop_daemon();
    fs::write(
        world.root.join("java/u00000.java"),
        "class U00000 { void f() { h(); } }\n",
    )
    .unwrap();
    let log = world._temp.path().join("daemon.log");
    let _daemon = LoggedDaemon::start(
        &world.home,
        &log,
        &[
            (
                "TRELLIS_TEST_SEMANTIC_CUTOFF_SOCKET",
                socket.to_str().unwrap(),
            ),
            ("TRELLIS_TEST_SEMANTIC_COMMIT_BUSY", busy.to_str().unwrap()),
        ],
    );
    let advanced: IndexPin =
        serde_json::from_value(restart_and_index(&world, &[])["publishedRevision"].clone())
            .unwrap();
    let child = world
        .import_command(&descriptor, &sha)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&socket.with_extension("ready"), "watcher cutoff");
    assert_eq!(world.head(), advanced);
    // The watcher publishes this edit inside the import's retry loop, moving the head past the
    // pin the request started from. The COMMIT of the final attempt is then busy.
    fs::write(
        world.root.join("rust/u00000.rs"),
        "fn u00000() { late(); }\n",
    )
    .unwrap();
    drop(UnixStream::connect(&socket).unwrap());
    let output = child.wait_with_output().unwrap();
    let imported = succeeded(&output);
    assert!(busy.exists(), "the COMMIT failed from contention once");
    let published: IndexPin =
        serde_json::from_value(imported["publishedRevision"].clone()).unwrap();
    assert!(
        published.index_revision >= advanced.index_revision + 2,
        "the watcher published inside the loop before the batch"
    );
    assert_eq!(imported["report"]["descriptorSha256"], sha);
    assert_eq!(world.head(), published);
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 1);
    let queue = world.queue();
    assert!(
        queue.iter().all(|row| row.1 == "done" && row.2.is_none()),
        "contention left no request running or failed: {queue:?}"
    );
    let log = fs::read_to_string(&log).unwrap();
    assert!(
        !log.contains("leave running for verified recovery"),
        "the busy COMMIT was requeued by the running daemon, not left to a new incarnation: {log}"
    );
}
