use super::*;
use crate::{index_coordinator::IndexJobCoordinator, indexer::IndexOptions};
use std::{fs, sync::atomic::AtomicBool};

const FILES: usize = 120;

fn write_tree(root: &Path, marker: usize) {
    for file in 0..FILES {
        let body: String = (0..40)
            .map(|method| format!("  int m{method}() {{ return {marker} + {file}; }}\n"))
            .collect();
        fs::write(
            root.join(format!("C{file}.java")),
            format!("class C{file} {{\n{body}}}\n"),
        )
        .unwrap();
    }
}

/// #210: once a large publication spills to `index.db`, it holds every reader
/// out until COMMIT. A read then fails at once with `rebuild_in_progress`
/// instead of waiting out its busy budget, and reads recover when the
/// publication commits or rolls back.
#[test]
fn reads_fail_fast_while_a_spilled_publication_holds_the_index() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    write_tree(work.path(), 1);
    let first_job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = first_job.session();
    let first = first_job.run(&options, &cancel, |_| {}).unwrap();

    // A tiny writer cache makes this small corpus spill like a 10k rebuild.
    store.set_publication_cache_kib_for_tests(64);
    write_tree(work.path(), 2);
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    store.set_publication_before_commit_hook_for_tests(move || {
        arrived_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    let job =
        IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone()).unwrap();
    let publish_options = options.clone();
    let publish_cancel = cancel.clone();
    let publisher = std::thread::spawn(move || job.run(&publish_options, &publish_cancel, |_| {}));
    arrived_rx
        .recv_timeout(crate::testing::HANG_BACKSTOP)
        .expect("publication did not reach the commit barrier");
    let started = Instant::now();
    let during = store.status();
    let waited = started.elapsed();
    let during_pinned = store.source_at("C0.java", Some(first));
    release_tx.send(()).unwrap();
    let next = publisher.join().unwrap().unwrap();

    let error = during.expect_err("a spilled publication must lock readers out");
    let rebuild = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RebuildInProgress>())
        .unwrap_or_else(|| panic!("expected rebuild_in_progress, got {error:#}"));
    assert_eq!(rebuild.documents, FILES);
    assert!(
        error.to_string().starts_with("rebuild_in_progress:"),
        "{error:#}"
    );
    // The ordinary busy wait is 5 s; failing fast means not waiting it out.
    assert!(waited < Duration::from_secs(4), "read waited {waited:?}");
    let pinned_error = during_pinned.expect_err("pinned reads are locked out too");
    assert!(
        pinned_error
            .chain()
            .any(|cause| cause.downcast_ref::<RebuildInProgress>().is_some()),
        "{pinned_error:#}"
    );
    assert!(!cli_read_retryable_contention(&error));
    assert!(transient_storage_contention(&error));

    // COMMIT clears the mark: reads see the new head.
    assert_eq!(next.index_revision, first.index_revision + 1);
    assert_eq!(store.status().unwrap().revision, next);

    // A spilled publication that rolls back clears it too.
    write_tree(work.path(), 3);
    let abort = cancel.clone();
    store.set_publication_before_commit_hook_for_tests(move || {
        abort.store(true, Ordering::Release);
    });
    let failed = IndexJobCoordinator::prepare_with_session(&store, Some(next), session)
        .unwrap()
        .run(&options, &cancel, |_| {});
    assert!(failed.is_err());
    assert!(
        store
            .publication_gate
            .readers_blocked
            .lock()
            .unwrap()
            .is_none()
    );
    assert_eq!(store.status().unwrap().revision, next);
}

/// #210 review: a spilled publication that fails still holds EXCLUSIVE until
/// its rollback finishes, so the mark may clear only after that. At the moment
/// it clears, the prior head must already be readable; clearing it earlier
/// would leave readers waiting on the rollback and getting `storage_busy`.
#[test]
fn the_mark_clears_only_after_a_failed_spilled_publication_rolls_back() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    write_tree(work.path(), 1);
    let first_job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = first_job.session();
    let first = first_job.run(&options, &cancel, |_| {}).unwrap();

    store.set_publication_cache_kib_for_tests(64);
    write_tree(work.path(), 2);
    // Cancel at the commit barrier; pause where the mark clears.
    let abort = cancel.clone();
    store.set_publication_before_commit_hook_for_tests(move || {
        abort.store(true, Ordering::Release);
    });
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    store.set_after_readers_unblocked_hook_for_tests(move || {
        arrived_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    let job = IndexJobCoordinator::prepare_with_session(&store, Some(first), session).unwrap();
    let publish_options = options.clone();
    let publish_cancel = cancel.clone();
    let publisher = std::thread::spawn(move || job.run(&publish_options, &publish_cancel, |_| {}));
    arrived_rx
        .recv_timeout(crate::testing::HANG_BACKSTOP)
        .expect("the failed spilled publication never cleared its mark");
    let at_clear = store.status();
    release_tx.send(()).unwrap();
    assert!(publisher.join().unwrap().is_err());

    assert_eq!(
        at_clear
            .unwrap_or_else(|error| panic!("the lock outlived the mark: {error:#}"))
            .revision,
        first
    );
    assert!(
        store
            .publication_gate
            .readers_blocked
            .lock()
            .unwrap()
            .is_none()
    );
}
