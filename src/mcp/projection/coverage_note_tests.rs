//! #1255: the coverage note each call tool returns names a language's semantic state.
//! A language whose evidence needs re-import or is catching up says so, worded as status and
//! describe word it; "does not cover this language" is only for a language that no published
//! producer covers.
use super::*;
use crate::store::{AutoRunLabels, Store};
use std::sync::{Arc, atomic::AtomicBool};

const REASON: &str = "retained run native revision no longer matches; import a current artifact";
const NEEDS_REIMPORT: &str = "semantic evidence needs re-import: retained run native revision no longer matches; import a current artifact";
const CATCHING_UP: &str = "semantic evidence catching up: producer re-run scheduled";
const FAILED: &str = "semantic evidence needs re-import: automatic re-run failed: producer_failed";

struct Checkout {
    _state: tempfile::TempDir,
    _root: tempfile::TempDir,
    store: Store,
    index_db: std::path::PathBuf,
    key: String,
}

/// A JavaScript checkout whose head publishes one producer, which covers only Rust.
fn checkout() -> Checkout {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("a.js"),
        "function a() { b(); }\nfunction b() {}\n",
    )
    .unwrap();
    let store =
        crate::testing::retry_admission(|| Store::open_for_tests(state.path(), root.path()));
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = crate::indexer::index_workspace_bundle(
        &crate::indexer::IndexOptions::new(root.path().into()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &store.writer().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let index_db = std::fs::read_dir(state.path().join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db");
    let key = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
    let checkout = Checkout {
        _state: state,
        _root: root,
        store,
        index_db,
        key,
    };
    checkout.sql(
        "INSERT INTO semantic_producers VALUES('rust-analyzer','1',?2,'semantic','utf8','executable');
         INSERT INTO semantic_producer_languages VALUES('rust-analyzer','1','rust',0);
         INSERT INTO semantic_revision_producers VALUES(?1,'rust-analyzer','1','fullCaptured',NULL,NULL);",
    );
    checkout
}

impl Checkout {
    /// Runs `statements` with `?1` as the head revision and `?2` as a 64-hex digest.
    fn sql(&self, statements: &str) {
        let db = rusqlite::Connection::open(&self.index_db).unwrap();
        for statement in statements.split(';').filter(|s| !s.trim().is_empty()) {
            let digest = "a".repeat(64);
            let mut statement = db.prepare(statement).unwrap();
            let count = statement.parameter_count();
            let params: Vec<&dyn rusqlite::ToSql> = [&self.key as &dyn rusqlite::ToSql, &digest]
                .into_iter()
                .take(count)
                .collect();
            statement.execute(params.as_slice()).unwrap();
        }
    }

    /// The retained-run state `trellis status` reported in the #1255 dogfood.
    fn reimport(&self) {
        rusqlite::Connection::open(&self.index_db)
            .unwrap()
            .execute(
                "INSERT INTO semantic_reimport VALUES(?1,'scip-typescript','javascript',?2)",
                rusqlite::params![self.key, REASON],
            )
            .unwrap();
    }

    /// The `syntaxOnly` message of each call tool and the note of the Files view's sequence
    /// and the CLI view, all read under `labels`.
    fn notes(&self, labels: &AutoRunLabels) -> Notes {
        let response = self
            .store
            .evidence_response()
            .unwrap()
            .with_auto_run_labels(labels.clone());
        let id = |name: &str| {
            let (_, symbols) = response.symbols_at(name, 10).unwrap();
            symbols.into_iter().find(|s| s.name == name).unwrap().id
        };
        let (a, b) = (id("a"), id("b"));
        let tiers = response.mcp_language_tiers().unwrap();
        let root = std::path::Path::new("/workspace");
        let syntax_only = |envelope: Value| -> Option<String> {
            envelope["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["code"] == "syntaxOnly")
                .map(|w| w["message"].as_str().unwrap().to_owned())
        };

        let mut answer = response
            .graph_query(
                &crate::graph_query::GraphQuery {
                    root_syntax_id: a.clone(),
                    semantic_producer_id: None,
                    depth: 2,
                    max_nodes: 50,
                    max_calls: 50,
                    include_external: true,
                    include_tests: true,
                    app_paths: None,
                },
                None,
                &mut || Ok(()),
            )
            .unwrap();
        let graph = call_graph(
            &json!(1),
            &mut answer,
            None,
            &Default::default(),
            &tiers,
            false,
            root,
            false,
            true,
        )
        .unwrap()
        .unwrap();

        let deadline = std::time::Instant::now() + crate::mcp::CALL_DEADLINE;
        let found = response
            .mcp_outgoing_with_lines_excluding(
                &a,
                20,
                None,
                None,
                &std::time::Instant::now,
                deadline,
            )
            .unwrap()
            .unwrap();
        let semantic = response
            .mcp_call_semantics(
                None,
                &found.owner.selected.declaration.document,
                found.calls.iter().map(|call| &call.selected.call),
            )
            .unwrap();
        let outgoing = outgoing(
            &json!(1),
            found,
            &semantic,
            &tiers,
            false,
            root,
            false,
            true,
        )
        .unwrap()
        .unwrap();

        let found = response
            .mcp_incoming_excluding_bound_elsewhere(&b, 20, None, None, &mut || Ok(()))
            .unwrap()
            .unwrap();
        assert_eq!(found.calls.len(), 1, "a calls b");
        let semantic = response
            .mcp_call_semantics(
                None,
                &found.selected.selected.declaration.document,
                found.calls.iter().map(|call| &call.call),
            )
            .unwrap();
        let incoming = incoming(
            &json!(1),
            found,
            &semantic,
            &tiers,
            false,
            root,
            false,
            true,
        )
        .unwrap()
        .unwrap();

        let pin = response.status().unwrap().revision;
        let sequence = response.sequence_at(&a, pin, false).unwrap().unwrap();
        let view = response
            .query_view(&crate::model::ViewQuery {
                seed: a,
                depth: 1,
                max_nodes: 40,
                max_calls: 200,
                include_callbacks: false,
                exclude_paths: vec![],
                semantic_producer_id: None,
            })
            .unwrap()
            .unwrap();
        Notes {
            call_graph: syntax_only(graph),
            outgoing: syntax_only(outgoing),
            incoming: syntax_only(incoming),
            sequence: sequence.semantic.reason,
            view: view.semantic.note,
            describe: tiers[3].note.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Notes {
    call_graph: Option<String>,
    outgoing: Option<String>,
    incoming: Option<String>,
    /// The Files view's sequence.
    sequence: Option<String>,
    /// The CLI view.
    view: Option<String>,
    /// describe's JavaScript row.
    describe: Option<String>,
}

impl Notes {
    fn all(note: &str) -> Self {
        let note = Some(note.to_owned());
        Self {
            call_graph: note.clone(),
            outgoing: note.clone(),
            incoming: note.clone(),
            sequence: note.clone(),
            view: note.clone(),
            describe: note,
        }
    }
}

#[test]
fn a_language_no_producer_covers_keeps_the_coverage_note() {
    let checkout = checkout();
    assert_eq!(
        checkout.notes(&AutoRunLabels::default()),
        Notes::all(crate::graph_query::LANGUAGE_NOTE)
    );
}

#[test]
fn a_language_that_needs_reimport_names_the_state_and_the_action() {
    let checkout = checkout();
    checkout.reimport();
    assert_eq!(
        checkout.notes(&AutoRunLabels::default()),
        Notes::all(NEEDS_REIMPORT)
    );
    // The wording is status's.
    let status = checkout
        .store
        .evidence_response()
        .unwrap()
        .status()
        .unwrap();
    let row = status
        .semantic_languages
        .iter()
        .find(|row| row.language == "javascript")
        .unwrap();
    assert_eq!(row.semantic_note.as_deref(), Some(NEEDS_REIMPORT));
    // A failed automatic re-run carries its failure as the reason, as describe does.
    let mut failed = AutoRunLabels::default();
    failed.failed.insert(
        "javascript".into(),
        "automatic re-run failed: producer_failed".into(),
    );
    assert_eq!(checkout.notes(&failed), Notes::all(FAILED));
}

#[test]
fn a_language_catching_up_names_the_state() {
    let checkout = checkout();
    let mut labels = AutoRunLabels::default();
    labels.catching_up.insert("javascript".into());
    assert_eq!(checkout.notes(&labels), Notes::all(CATCHING_UP));
    // A scheduled re-run of evidence that needs re-import is catching up (#660).
    checkout.reimport();
    assert_eq!(checkout.notes(&labels), Notes::all(CATCHING_UP));
    // A label on another language changes nothing here.
    let mut other = AutoRunLabels::default();
    other.catching_up.insert("python".into());
    assert_eq!(
        checkout.notes(&other),
        Notes::all(NEEDS_REIMPORT),
        "javascript keeps its own state"
    );
}
