//! The automatic re-run read-path labels (#660 D10): only the label comes from runtime state.
//! Evidence, coverage and freshness stay the committed revision's.
#![cfg(feature = "test-support")]
use crate::semantic_batch_support::BatchFixture;
use rusqlite::Connection;
use serde_json::Value;
use std::sync::{Arc, atomic::AtomicBool};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::{Builder, TempDir};
use trellis::{
    graph_query::GraphQuery,
    indexer::IndexOptions,
    model::{DocumentSemanticStatus, IndexPin, IndexStatus, SemanticReimport, SemanticStatus},
    store::{
        AutoRunLabels, LanguageTier, Store, relabel_status,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

const PRODUCER: &str = "trellis-synthetic-scip";

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

    /// Edits `path` and publishes only the native revision, with no daemon to catch semantic
    /// evidence up, then drops the batch's retained raw input: the changed documents of the
    /// producer need re-import.
    fn edit_needing_reimport(&self, path: &str, text: &str) -> IndexPin {
        crate::support::stop_daemon(&self.home);
        self.write(path, text);
        let cancel = Arc::new(AtomicBool::new(false));
        let options = IndexOptions::new(self.root.clone());
        let capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let leader = self.store.publication_for_tests().unwrap();
        let head = self
            .store
            .publish_stream_native_only_for_tests(&options, &capture, &leader, &cancel)
            .unwrap();
        drop(leader);
        let key = format!("pin:v1:{}:{}", head.index_generation, head.index_revision);
        Connection::open(&self.index_db)
            .unwrap()
            .execute(
                "DELETE FROM semantic_revision_raw_selections WHERE revision_id=?1 AND producer_id=?2",
                rusqlite::params![key, PRODUCER],
            )
            .unwrap();
        head
    }

    fn tiers(&self, labels: &AutoRunLabels) -> [LanguageTier; 5] {
        self.store
            .evidence_response()
            .unwrap()
            .with_auto_run_labels(labels.clone())
            .mcp_language_tiers()
            .unwrap()
    }

    fn status(&self) -> IndexStatus {
        self.store.evidence_response().unwrap().status().unwrap()
    }
}

/// The language order of `mcp_language_tiers`.
const JAVASCRIPT_AT: usize = 3;
const PYTHON_AT: usize = 2;
const REASON: &str = "automatic re-run failed: producer_failed";

fn catching_up() -> AutoRunLabels {
    AutoRunLabels {
        catching_up: ["javascript".to_owned()].into(),
        ..AutoRunLabels::default()
    }
}

fn failed() -> AutoRunLabels {
    AutoRunLabels {
        failed: [("javascript".to_owned(), REASON.to_owned())].into(),
        ..AutoRunLabels::default()
    }
}

fn statuses(documents: &[DocumentSemanticStatus]) -> Vec<SemanticStatus> {
    documents
        .iter()
        .map(|document| document.projection.semantic_status)
        .collect()
}

const FILES: &[(&str, &str)] = &[
    ("src/A.js", "function run(){ target(); }\n"),
    ("src/B.js", "function target(){ return 1; }\n"),
    ("src/C.py", "def helper(): return 1\n"),
];

fn stale_world() -> (World, IndexPin) {
    let world = World::new(FILES);
    world.import_javascript();
    let head = world.edit_needing_reimport("src/B.js", "function target(){ return 2; }\n");
    (world, head)
}

/// Rule 1: a scheduled or running producer relabels its `NeedsReimport` documents `CatchingUp`
/// and gives the language `producer re-run scheduled`. Counts, freshness and producer are the
/// committed revision's.
#[test]
fn catching_up_relabels_needs_reimport_documents_only_as_a_label() {
    let (world, _) = stale_world();
    let committed = world.tiers(&AutoRunLabels::default());
    let before = &committed[JAVASCRIPT_AT];
    assert!(!before.documents.is_empty());
    assert!(
        statuses(&before.documents)
            .iter()
            .all(|status| *status == SemanticStatus::NeedsReimport)
    );
    assert_eq!(
        before.pending.as_ref().unwrap().reason,
        "re-import required"
    );

    let labelled = world.tiers(&catching_up());
    let after = &labelled[JAVASCRIPT_AT];
    assert_eq!(
        statuses(&after.documents),
        vec![SemanticStatus::CatchingUp; before.documents.len()]
    );
    let pending = after.pending.as_ref().unwrap();
    assert_eq!(pending.status, SemanticStatus::CatchingUp);
    assert_eq!(pending.reason, "producer re-run scheduled");
    assert_eq!(after.counts, before.counts);
    assert_eq!(after.freshness, before.freshness);
    assert_eq!(after.producer_id, before.producer_id);
    assert_eq!(after.reimport, before.reimport);
    assert_eq!(after.partial_reasons, before.partial_reasons);
    for (relabelled, committed) in after.documents.iter().zip(&before.documents) {
        assert_eq!(relabelled.path, committed.path);
        assert_eq!(
            relabelled.projection.basis_native_revision_id,
            committed.projection.basis_native_revision_id
        );
    }
    // Another language is untouched.
    assert_eq!(labelled[PYTHON_AT], committed[PYTHON_AT]);
}

/// Rule 2: a failed automatic re-run is the language's re-import reason; documents stay
/// `NeedsReimport` and the committed evidence is unchanged.
#[test]
fn failed_overrides_the_reason_and_keeps_documents() {
    let (world, _) = stale_world();
    let committed = world.tiers(&AutoRunLabels::default());
    let labelled = world.tiers(&failed());
    let (before, after) = (&committed[JAVASCRIPT_AT], &labelled[JAVASCRIPT_AT]);
    let pending = after.pending.as_ref().unwrap();
    assert_eq!(pending.status, SemanticStatus::NeedsReimport);
    assert_eq!(pending.reason, REASON);
    assert_eq!(
        after.reimport,
        Some(SemanticReimport::NeedsReimport {
            reason: REASON.to_owned()
        })
    );
    assert_eq!(after.documents, before.documents);
    assert_eq!(after.counts, before.counts);
    assert_eq!(after.freshness, before.freshness);
    assert_eq!(after.producer_id, before.producer_id);
    assert_eq!(labelled[PYTHON_AT], committed[PYTHON_AT]);

    // Catching up wins over a failure of another producer of the same language.
    let both = AutoRunLabels {
        catching_up: catching_up().catching_up,
        failed: failed().failed,
    };
    assert_eq!(
        world.tiers(&both)[JAVASCRIPT_AT]
            .pending
            .as_ref()
            .unwrap()
            .reason,
        "producer re-run scheduled"
    );
}

/// Empty labels are the committed projection.
#[test]
fn empty_labels_change_nothing() {
    let (world, _) = stale_world();
    let plain = world.store.evidence_response().unwrap();
    assert_eq!(
        plain.mcp_language_tiers().unwrap(),
        world.tiers(&AutoRunLabels::default())
    );
}

/// The overlay is never applied inside the status projection, and `relabel_status` applies
/// rules 1 and 2 to the cached status afterwards.
#[test]
fn relabel_status_applies_labels_after_the_committed_status() {
    let (world, _) = stale_world();
    let committed = world.status();
    let row = |status: &IndexStatus| {
        status
            .semantic_languages
            .iter()
            .find(|row| row.language == "javascript")
            .unwrap()
            .clone()
    };
    let before = row(&committed);
    assert!(!before.documents.is_empty());
    assert!(
        statuses(&before.documents)
            .iter()
            .all(|status| *status == SemanticStatus::NeedsReimport)
    );

    let mut relabelled = committed.clone();
    relabel_status(&mut relabelled, &AutoRunLabels::default());
    assert_eq!(relabelled, committed);

    relabel_status(&mut relabelled, &catching_up());
    let after = row(&relabelled);
    assert_eq!(
        statuses(&after.documents),
        vec![SemanticStatus::CatchingUp; before.documents.len()]
    );
    assert_eq!(after.reimport, before.reimport);
    assert_eq!(
        (
            after.calls,
            after.internal,
            after.external,
            after.unresolved
        ),
        (
            before.calls,
            before.internal,
            before.external,
            before.unresolved
        )
    );
    assert_eq!(after.coverage, before.coverage);
    assert_eq!(after.semantic_state, before.semantic_state);
    assert_eq!(relabelled.stats, committed.stats);
    // The python row is not the label's language.
    let python = |status: &IndexStatus| {
        status
            .semantic_languages
            .iter()
            .find(|row| row.language == "python")
            .cloned()
    };
    assert_eq!(python(&relabelled), python(&committed));

    let mut failed_status = committed.clone();
    relabel_status(&mut failed_status, &failed());
    let after = row(&failed_status);
    assert_eq!(after.documents, before.documents);
    assert_eq!(
        after.reimport,
        Some(SemanticReimport::NeedsReimport {
            reason: REASON.to_owned()
        })
    );
    assert_eq!(after.calls, before.calls);
    assert_eq!(after.coverage, before.coverage);

    // A language without a row gains none.
    let mut rows = committed.clone();
    relabel_status(
        &mut rows,
        &AutoRunLabels {
            catching_up: ["typescript".to_owned()].into(),
            failed: [("java".to_owned(), REASON.to_owned())].into(),
        },
    );
    assert_eq!(rows, committed);
}

/// The call-graph boundary is committed: the labels never reach it.
#[test]
fn graph_query_boundary_stays_needs_reimport_with_labels_set() {
    let (world, head) = stale_world();
    let (native, _) = world.store.retained_native_snapshot_at(head).unwrap();
    let root = native
        .declarations
        .iter()
        .find(|declaration| declaration.name.as_deref() == Some("run"))
        .unwrap()
        .syntax_id
        .clone();
    let query = GraphQuery {
        root_syntax_id: root,
        semantic_producer_id: Some(PRODUCER.into()),
        depth: 2,
        max_nodes: 20,
        max_calls: 20,
        include_external: true,
        include_tests: true,
        app_paths: None,
    };
    let ask = |labels: AutoRunLabels| {
        world
            .store
            .evidence_response()
            .unwrap()
            .with_auto_run_labels(labels)
            .graph_query(&query, Some(head), &mut || Ok(()))
            .unwrap()
            .outcome
            .unwrap()
    };
    let committed = ask(AutoRunLabels::default());
    assert!(
        committed
            .semantic_projections
            .iter()
            .any(|projection| projection.semantic_status == SemanticStatus::NeedsReimport),
        "{:?}",
        committed.semantic_projections
    );
    assert_eq!(ask(catching_up()), committed);
    assert_eq!(ask(failed()), committed);
}
