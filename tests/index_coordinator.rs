use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use trellis::{
    index_coordinator::{drain_requests_observed_with_native, reconcile_workspace},
    indexer::IndexOptions,
    store::Store,
};

#[test]
fn changed_input_requeues_claim_and_commits_edited_source() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = workspace.path().join("a.js");
    let manifest = workspace.path().join("package.json");
    fs::write(&source, "function before() {}\n").unwrap();
    fs::write(&manifest, "{\"name\":\"before\"}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let (prior, owner) = reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    let request = store.enqueue_request(&options, None).unwrap();
    let edited = AtomicBool::new(false);
    let count = drain_requests_observed_with_native(
        &store,
        &owner,
        |id, progress| {
            if id == request.id && progress.phase == "scan" && !edited.swap(true, Ordering::AcqRel)
            {
                fs::write(&manifest, "{\"name\":\"after\"}\n").unwrap();
                fs::write(&source, "function after() {}\n").unwrap();
            }
        },
        |_, _, _| {},
    )
    .unwrap();
    assert!(edited.load(Ordering::Acquire));
    assert_eq!(count, 0);
    assert_eq!(store.status().unwrap().revision, prior);
    let pending = store.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(pending.seq, request.seq);
    assert_eq!(pending.state, "queued");
    assert!(pending.claim_incarnation.is_none() && pending.error_code.is_none());
    assert_eq!(
        drain_requests_observed_with_native(&store, &owner, |_, _| {}, |_, _, _| {}).unwrap(),
        1
    );
    let done = store.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(done.seq, request.seq);
    assert_eq!(done.state, "done");
    let response = store.evidence_response().unwrap();
    assert_eq!(
        response
            .source_at("a.js", done.revision)
            .unwrap()
            .unwrap()
            .1
            .text,
        "function after() {}\n"
    );
    response.finish(()).unwrap();
}
