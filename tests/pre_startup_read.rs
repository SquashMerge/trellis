//! Daemon-local selected reads use a validated committed head during mandatory startup reconciliation.
//! The retired cross-process predecessor permit and leader lock are not read gates.
mod common;
use std::sync::{Arc, atomic::AtomicBool};
use trellis::{
    index_coordinator::{self, IndexJobCoordinator},
    indexer::IndexOptions,
    model::CancelFlag,
    store::Store,
};

fn fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Store,
    IndexOptions,
    CancelFlag,
) {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("a.js"), "function oldHead() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    (state, work, store, options, cancel)
}

fn published() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Store,
    IndexOptions,
    CancelFlag,
    trellis::model::IndexPin,
) {
    let (state, work, store, options, cancel) = fixture();
    let (pin, _writer) =
        index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    (state, work, store, options, cancel, pin)
}

fn index_db(state: &std::path::Path) -> std::path::PathBuf {
    std::fs::read_dir(state.join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db")
}

#[test]
fn cold_head_has_no_valid_selected_revision() {
    let (_state, _work, store, _options, _cancel) = fixture();
    assert!(store.evidence_response_daemon_local().is_err());
}

#[test]
fn local_startup_keeps_validated_prior_head_readable_until_commit_then_selects_new_head() {
    let (_state, work, store, options, cancel, first) = published();
    std::fs::write(work.path().join("a.js"), "function newHead() {}\n").unwrap();
    let coordinator = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = coordinator.session();
    let prior = store.evidence_response_daemon_local().unwrap();
    assert_eq!(prior.status().unwrap().revision, first);
    prior.validate_pin(first).unwrap();
    let (pin, source) = prior.source_at("a.js", None).unwrap().unwrap();
    assert_eq!(pin, first);
    assert_eq!(source.text, "function oldHead() {}\n");
    prior.finish(()).unwrap();
    drop(prior); // A DELETE-journal writer must not wait behind this read snapshot.
    let second = coordinator.run(&options, &cancel, |_| {}).unwrap();
    assert!(second.index_revision > first.index_revision);
    session.verify().unwrap();
    let current = store.evidence_response_daemon_local().unwrap();
    assert_eq!(current.status().unwrap().revision, second);
    assert_eq!(
        current.source_at("a.js", None).unwrap().unwrap().1.text,
        "function newHead() {}\n"
    );
    current.finish(()).unwrap();
}

#[test]
fn startup_reconciliation_blocks_fifo_claim_without_blocking_validated_prior_read() {
    let (_state, _work, store, options, cancel, first) = published();
    let accepted = store.enqueue_request(&options, None).unwrap();
    let coordinator = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = coordinator.session();
    let refused = store.claim_request(&session).unwrap_err();
    assert!(
        format!("{refused:#}").contains("index_not_ready"),
        "{refused:#}"
    );
    assert_eq!(
        store.request_by_id(&accepted.id).unwrap().unwrap().state,
        "queued"
    );
    let prior = store.evidence_response_daemon_local().unwrap();
    assert_eq!(prior.status().unwrap().revision, first);
    prior.finish(()).unwrap();
    drop(prior);
    let h = coordinator.run(&options, &cancel, |_| {}).unwrap();
    assert!(h.index_revision > first.index_revision);
    let claimed = store.claim_request(&session).unwrap().unwrap();
    assert_eq!(claimed.id, accepted.id);
    assert_eq!(claimed.state, "running");
}

#[test]
fn selected_admission_rejects_corrupt_schema_and_foreign_revision() {
    let (state, _work, store, _options, _cancel, _) = published();
    let db = rusqlite::Connection::open(index_db(state.path())).unwrap();
    db.pragma_update(None, "user_version", 7).unwrap();
    drop(db);
    assert!(store.evidence_response_daemon_local().is_err());

    let (state, _work, store, _options, _cancel, _) = published();
    let db = rusqlite::Connection::open(index_db(state.path())).unwrap();
    db.execute(
        "UPDATE index_metadata SET index_revision=index_revision+1 WHERE singleton=1",
        [],
    )
    .unwrap();
    drop(db);
    assert!(store.evidence_response_daemon_local().is_err());
}

#[test]
fn selected_result_refuses_lost_root_even_after_materialization() {
    let (_state, work, store, _options, _cancel, first) = published();
    let selected = store.evidence_response_daemon_local().unwrap();
    assert_eq!(selected.status().unwrap().revision, first);
    let moved = work.path().with_extension("moved");
    std::fs::rename(work.path(), &moved).unwrap();
    std::fs::create_dir(work.path()).unwrap();
    let failure = selected.finish(()).unwrap_err();
    assert!(
        format!("{failure:#}").contains("root_changed"),
        "{failure:#}"
    );
    assert!(store.evidence_response_daemon_local().is_err());
    std::fs::remove_dir(work.path()).unwrap();
    std::fs::rename(moved, work.path()).unwrap();
}
