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
    BatchFixture, edit_descriptor, long_path_workspace, uniform_workspace,
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
    let world = World::new(
        four_language_workspace,
        &[("TRELLIS_TEST_SEMANTIC_COMMIT_BUSY", &marker)],
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

/// A daemon started on its own so its diagnostics reach a file.
struct LoggedDaemon {
    child: std::process::Child,
}

impl LoggedDaemon {
    fn start(home: &Path, log: &Path, env: &[(&str, &str)]) -> Self {
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
    let last = queue.last().unwrap();
    assert_ne!(last.0, v1.id);
    assert_eq!((last.1.as_str(), last.2.as_deref()), ("done", None));
    assert_eq!(world.count("SELECT count(*) FROM semantic_raw_batches"), 1);
}
