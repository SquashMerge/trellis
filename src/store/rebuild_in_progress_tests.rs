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

/// A second publication paused at its commit barrier with its transaction
/// spilled to `index.db`, so it holds every reader out (#210).
struct HeldSpill {
    store: Store,
    first: IndexPin,
    held: Held,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

/// The paused publisher of a `HeldSpill`.
struct Held {
    first: IndexPin,
    release: std::sync::mpsc::Sender<()>,
    publisher: std::thread::JoinHandle<Result<IndexPin>>,
}

impl Held {
    /// Let the held publication commit and return its pin.
    fn commit(self) -> IndexPin {
        self.release.send(()).unwrap();
        let next = self.publisher.join().unwrap().unwrap();
        assert_eq!(next.index_revision, self.first.index_revision + 1);
        next
    }
}

impl HeldSpill {
    fn start() -> Self {
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
        let (release, release_rx) = std::sync::mpsc::channel();
        store.set_publication_before_commit_hook_for_tests(move || {
            arrived_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        let job = IndexJobCoordinator::prepare_with_session(&store, Some(first), session).unwrap();
        let publisher = std::thread::spawn(move || job.run(&options, &cancel, |_| {}));
        arrived_rx
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .expect("publication did not reach the commit barrier");
        assert!(
            store
                .publication_gate
                .readers_blocked
                .lock()
                .unwrap()
                .is_some(),
            "the publication must have spilled"
        );
        Self {
            store,
            first,
            held: Held {
                first,
                release,
                publisher,
            },
            _dirs: (state, work),
        }
    }
}

enum ReaderEvent<T> {
    Waiting,
    Done(Result<T>),
}

/// Run `read` on another thread during the held spill. Once the reader is
/// waiting on the spilled publication (or has already returned), let the
/// publication commit; return the read's result and the new pin.
fn read_across_commit<T: Send + 'static>(
    spill: HeldSpill,
    read: impl FnOnce(&Store) -> Result<T> + Send,
) -> (Result<T>, IndexPin) {
    let HeldSpill {
        store, held, _dirs, ..
    } = spill;
    let (events, events_rx) = std::sync::mpsc::channel();
    let waiting = events.clone();
    store.publication_gate.test_before_spill_wait.set(move || {
        let _ = waiting.send(ReaderEvent::Waiting);
    });
    std::thread::scope(|scope| {
        let store = &store;
        scope.spawn(move || {
            let result = read(store);
            events.send(ReaderEvent::Done(result)).unwrap();
        });
        let first_event = events_rx
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .expect("the reader neither waited nor returned");
        let waited = matches!(first_event, ReaderEvent::Waiting);
        let next = held.commit();
        let result = match first_event {
            ReaderEvent::Done(result) => result,
            ReaderEvent::Waiting => match events_rx
                .recv_timeout(crate::testing::HANG_BACKSTOP)
                .expect("the waiting reader did not return after COMMIT")
            {
                ReaderEvent::Done(result) => result,
                ReaderEvent::Waiting => panic!("the reader waited twice"),
            },
        };
        let result = result.map_err(|error| {
            anyhow::anyhow!("read failed (waited on the spill: {waited}): {error:#}")
        });
        (result, next)
    })
}

/// #674: a read that finds a spilled publication holding the index waits on
/// the gate's own signal for its COMMIT, then reads the newly published head,
/// instead of failing `rebuild_in_progress` at once.
#[test]
fn a_read_during_a_spill_waits_for_commit_and_reads_the_new_head() {
    let (status, next) = read_across_commit(HeldSpill::start(), Store::status);
    let status = status.unwrap_or_else(|error| panic!("{error:#}"));
    assert_eq!(status.revision, next);
}

/// #674: a pinned read of the prior head waits out the spill the same way and
/// still serves its own pin, never the newly published one.
#[test]
fn a_pinned_read_during_a_spill_keeps_its_pin() {
    let spill = HeldSpill::start();
    let first = spill.first;
    let (source, next) =
        read_across_commit(spill, move |store| store.source_at("C0.java", Some(first)));
    let (served, file) = source
        .unwrap_or_else(|error| panic!("{error:#}"))
        .expect("C0.java at the prior pin");
    assert_eq!(served, first);
    assert_ne!(served, next);
    assert!(file.text.contains("return 1 + 0;"), "{}", file.text);
}

/// #674: with the spilled publication held past the read budget, the read
/// fails typed `rebuild_in_progress` after about that budget: not at once,
/// and not after its ordinary 5 s busy wait (#210).
#[test]
fn a_read_held_past_the_spill_budget_fails_rebuild_in_progress() {
    let spill = HeldSpill::start();
    let budget = Duration::from_millis(400);
    *spill
        .store
        .publication_gate
        .test_spill_read_wait
        .lock()
        .unwrap() = Some(budget);
    let started = Instant::now();
    let during = spill.store.status();
    let waited = started.elapsed();
    let pinned_started = Instant::now();
    let during_pinned = spill.store.source_at("C0.java", Some(spill.first));
    let pinned_waited = pinned_started.elapsed();
    let HeldSpill {
        store, held, _dirs, ..
    } = spill;
    let next = held.commit();

    let error = during.expect_err("a held spill must fail the read once its budget runs out");
    let rebuild = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RebuildInProgress>())
        .unwrap_or_else(|| panic!("expected rebuild_in_progress, got {error:#}"));
    assert_eq!(rebuild.documents, FILES);
    assert!(
        error.to_string().starts_with("rebuild_in_progress:"),
        "{error:#}"
    );
    assert!(waited >= budget, "read gave up after {waited:?}");
    // The ordinary busy wait is 5 s; the read stops at the spill budget.
    assert!(waited < Duration::from_secs(4), "read waited {waited:?}");
    let pinned_error = during_pinned.expect_err("pinned reads are held out too");
    assert!(
        pinned_error
            .chain()
            .any(|cause| cause.downcast_ref::<RebuildInProgress>().is_some()),
        "{pinned_error:#}"
    );
    assert!(
        pinned_waited >= budget,
        "pinned read gave up after {pinned_waited:?}"
    );
    assert!(!cli_read_retryable_contention(&error));
    assert!(transient_storage_contention(&error));

    // COMMIT clears the mark: reads see the new head.
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

/// #210: a spilled publication that rolls back clears the mark, and reads
/// see the prior head.
#[test]
fn a_spilled_publication_that_rolls_back_clears_the_mark() {
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
    write_tree(work.path(), 3);
    let abort = cancel.clone();
    store.set_publication_before_commit_hook_for_tests(move || {
        abort.store(true, Ordering::Release);
    });
    let failed = IndexJobCoordinator::prepare_with_session(&store, Some(first), session)
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
    assert_eq!(store.status().unwrap().revision, first);
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
