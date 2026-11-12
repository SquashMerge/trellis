//! The automatic re-run due query (#660 SD11): which producers a head leaves behind.
#![cfg(feature = "test-support")]
use crate::semantic_batch_support::BatchFixture;
use rusqlite::Connection;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::{Builder, TempDir};
use trellis::{
    model::IndexPin,
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

const PRODUCER: &str = "trellis-synthetic-scip";
const JAVASCRIPT: &[&str] = &["javascript"];

struct World {
    _temp: TempDir,
    home: PathBuf,
    root: PathBuf,
    store: Store,
    index_db: PathBuf,
}

fn cli(home: &Path) -> Command {
    let mut cmd = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
    cmd.env("HOME", home)
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("daemon.pid"))
        .env(crate::child::DAEMON_STDERR, home.join("daemon.stderr"))
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null());
    cmd
}

fn run(home: &Path, args: &[&str]) -> Value {
    let output = cli(home).args(args).output().unwrap();
    crate::child::cli_json_with(
        &output,
        format_args!("trellis {}", args.first().copied().unwrap_or_default()),
        Some(&home.join("daemon.stderr")),
    )
}

impl World {
    /// A checkout of `files`, indexed natively; no semantic batch is imported yet.
    fn new(files: &[(&str, &str)]) -> Self {
        let temp = Builder::new()
            .prefix("auto-run-due-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join(".gitignore"), ".trellis-semantic-batch*/\n").unwrap();
        for (path, text) in files {
            fs::write(root.join(path), text).unwrap();
        }
        let root = fs::canonicalize(root).unwrap();
        let indexed = run(&home, &["index", "--workspace", root.to_str().unwrap()]);
        let roots = TopologyRoots::isolated_for_tests(
            if cfg!(target_os = "macos") {
                home.join("Library/Caches/dev.squashmerge.trellis")
            } else {
                home.join(".cache/trellis")
            },
            if cfg!(target_os = "macos") {
                home.join("Library/Application Support/dev.squashmerge.trellis")
            } else {
                home.join(".local/share/trellis")
            },
        );
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let index_db = roots.index_db(&identity);
        let store = trellis::testing::retry_admission(|| {
            Store::open(
                roots.clone(),
                WorkspaceIdentity::discover(Some(&root), &root)?,
            )
        });
        let world = Self {
            _temp: temp,
            home,
            root,
            store,
            index_db,
        };
        world.caught_up(&indexed);
        world
    }

    fn root_string(&self) -> &str {
        self.root.to_str().unwrap()
    }

    /// Waits for the daemon's semantic catch-up after the reply's native publication.
    fn caught_up(&self, reply: &Value) -> IndexPin {
        let native: IndexPin = serde_json::from_value(reply["publishedRevision"].clone()).unwrap();
        trellis::testing::wait_until("semantic catch-up", || {
            !self.store.semantic_catch_up_pending_for_tests().unwrap()
        });
        let head = self.store.index_baseline().unwrap();
        assert_eq!(head.index_generation, native.index_generation);
        assert!(head.index_revision >= native.index_revision);
        head
    }

    fn reindex(&self) -> IndexPin {
        self.caught_up(&run(
            &self.home,
            &["index", "--workspace", self.root_string()],
        ))
    }

    /// Imports one synthetic batch covering every JavaScript document at the head.
    fn import_javascript(&self) {
        let mut fixture = BatchFixture::from_published(&self.index_db, &self.root).synthetic();
        let docs = fixture.paths("javascript");
        fixture.artifact("javascript", docs);
        let (descriptor, sha) = fixture.write(&self.root.join(".trellis-semantic-batch"));
        let reply = run(
            &self.home,
            &[
                "semantic",
                "import",
                "--workspace",
                self.root_string(),
                "--batch",
                descriptor.to_str().unwrap(),
                "--batch-sha256",
                &sha,
            ],
        );
        self.caught_up(&reply);
    }

    fn write(&self, path: &str, text: &str) {
        fs::write(self.root.join(path), text).unwrap();
    }

    fn due(&self, languages: &[&str]) -> bool {
        let head = self.store.auto_run_due(&[(PRODUCER, languages)]).unwrap();
        assert_eq!(head.pin, self.store.index_baseline().unwrap());
        head.due[PRODUCER]
    }
}

const FILES: &[(&str, &str)] = &[
    ("src/A.js", "function run(){ target(); }\n"),
    ("src/B.js", "function target(){ return 1; }\n"),
    ("src/C.py", "def helper(): return 1\n"),
];

#[test]
fn a_changed_document_is_due() {
    let world = World::new(FILES);
    world.import_javascript();
    assert!(!world.due(JAVASCRIPT));
    world.write("src/B.js", "function target(){ return 2; }\n");
    world.reindex();
    let head = world.store.auto_run_due(&[(PRODUCER, JAVASCRIPT)]).unwrap();
    assert!(head.due[PRODUCER]);
    assert!(!head.native_revision_id.is_empty());
}

#[test]
fn an_added_document_is_due() {
    let world = World::new(FILES);
    world.import_javascript();
    world.write("src/D.js", "function added(){}\n");
    world.reindex();
    assert!(world.due(JAVASCRIPT));
}

#[test]
fn a_change_in_another_language_is_not_due() {
    let world = World::new(FILES);
    world.import_javascript();
    world.write("src/C.py", "def helper(): return 2\n");
    world.reindex();
    assert!(!world.due(JAVASCRIPT));
    assert!(world.due(&["python"]));
}

#[test]
fn a_deleted_document_is_due() {
    let world = World::new(FILES);
    world.import_javascript();
    fs::remove_file(world.root.join("src/B.js")).unwrap();
    world.reindex();
    assert!(world.due(JAVASCRIPT));
}

#[test]
fn an_unchanged_head_is_not_due() {
    let world = World::new(FILES);
    world.import_javascript();
    world.reindex();
    assert!(!world.due(JAVASCRIPT));
}

#[test]
fn a_reimport_row_is_due_for_its_language_only() {
    let world = World::new(FILES);
    world.import_javascript();
    let head = world.store.index_baseline().unwrap();
    let key = format!("pin:v1:{}:{}", head.index_generation, head.index_revision);
    Connection::open(&world.index_db)
        .unwrap()
        .execute(
            "INSERT INTO semantic_reimport VALUES(?1,?2,'javascript','import a current artifact')",
            rusqlite::params![key, PRODUCER],
        )
        .unwrap();
    assert!(world.due(JAVASCRIPT));
    assert!(!world.due(&["python"]));
}

#[test]
fn no_selection_and_no_row_is_not_due() {
    let world = World::new(FILES);
    world.write("src/B.js", "function target(){ return 2; }\n");
    world.reindex();
    assert!(!world.due(JAVASCRIPT));
}
