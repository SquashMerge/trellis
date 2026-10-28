use super::*;
use crate::indexer::{IndexOptions, index_workspace_bundle};
use std::{
    fs,
    process::Command,
    sync::{atomic::AtomicBool, mpsc},
    thread,
};

const HOOK_TIMEOUT: Duration = Duration::from_secs(10);
const CORRUPT_INVENTORY: &str =
    "UPDATE revision_documents SET capture_stat='{}' WHERE path='f0.js'";

fn counts() -> (usize, u64) {
    (
        STATUS_VALIDATIONS.with(|count| count.get()),
        PAIRED_CHECK_COUNT.with(|count| count.get()),
    )
}

fn publish_changed(store: &Store, root: &Path, expected: IndexPin) -> IndexPin {
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.into()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &store.writer().unwrap(),
            expected,
            &cancel,
        )
        .unwrap()
}

fn assert_inventory_corruption(store: &Store) {
    let before = counts().0;
    let error = store.status().unwrap_err();
    assert!(
        format!("{error:#}").contains("incompatible_index"),
        "{error:#}"
    );
    assert_eq!(
        counts().0 - before,
        1,
        "changed disk state must be validated"
    );
    assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
}

#[test]
fn unchanged_head_is_admitted_once_across_responses_and_clones() {
    let (_state, _root, store) = selected_read_once_tests::published_store(2);
    store.published_admission.invalidate();
    let before = counts();
    let status = store.status().unwrap();
    let response = store.evidence_response().unwrap();
    let original = response.source_at("f0.js", None).unwrap().unwrap();
    assert_eq!(response.status().unwrap(), status);
    response.finish(()).unwrap();
    drop(response);
    let clone = store.clone();
    for _ in 0..2 {
        assert_eq!(clone.status().unwrap(), status);
        let response = clone.evidence_response().unwrap();
        assert_eq!(
            response.source_at("f0.js", Some(status.revision)).unwrap(),
            Some(original.clone())
        );
        response.finish(()).unwrap();
    }
    let after = counts();
    assert_eq!((after.0 - before.0, after.1 - before.1), (1, 1));
}

#[test]
fn same_pin_commit_child() {
    let Some(state) = std::env::var_os("TRELLIS_ADMISSION_TEST_STATE") else {
        return;
    };
    let root = std::env::var_os("TRELLIS_ADMISSION_TEST_ROOT").unwrap();
    let store = Store::open_for_tests(Path::new(&state), Path::new(&root)).unwrap();
    let db = store.cache_write().unwrap();
    assert_eq!(db.execute(CORRUPT_INVENTORY, []).unwrap(), 1);
}

#[test]
fn separate_process_same_pin_commit_invalidates_parent_admission() {
    let (state, root, store) = selected_read_once_tests::published_store(1);
    let pin = store.status().unwrap().revision;
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "store::published_admission_tests::same_pin_commit_child",
            "--nocapture",
        ])
        .env("TRELLIS_ADMISSION_TEST_STATE", state.path())
        .env("TRELLIS_ADMISSION_TEST_ROOT", root.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let (done_tx, done_rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let _ = done_tx.send(child.wait_with_output());
    });
    let output = match done_rx.recv_timeout(HOOK_TIMEOUT) {
        Ok(result) => result.unwrap(),
        Err(error) => {
            // Kill/reap the helper on a failed liveness backstop, never retry it.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            waiter.join().unwrap();
            panic!("same-pin writer did not finish: {error}");
        }
    };
    waiter.join().unwrap();
    assert!(output.status.success(), "child failed: {output:?}");
    assert_eq!(
        ReadRevision::current(&store.cache_write().unwrap())
            .unwrap()
            .pin,
        pin
    );
    assert_inventory_corruption(&store);
}

#[test]
fn recovery_latch_rejects_a_validator_paused_before_remember() {
    let (_state, _root, store) = selected_read_once_tests::published_store(1);
    store.published_admission.invalidate();
    let (reached_tx, reached_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    store.published_admission.before_remember.set(move || {
        reached_tx.send(()).unwrap();
        resume_rx.recv_timeout(HOOK_TIMEOUT).unwrap();
    });
    let reader = store.clone();
    let worker = thread::spawn(move || reader.status());
    reached_rx.recv_timeout(HOOK_TIMEOUT).unwrap();
    store.mark_recovery(RecoveryDisposition::Rebuild);
    resume_tx.send(()).unwrap();
    let error = worker.join().unwrap().unwrap_err();
    assert!(
        error.to_string().contains("reconciliation required"),
        "{error:#}"
    );
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("reconciliation required")
    );
}

#[test]
fn invalidation_epoch_prevents_late_remember_even_after_ready_returns() {
    let (_state, _root, store) = selected_read_once_tests::published_store(1);
    store.published_admission.invalidate();
    let (reached_tx, reached_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    store.published_admission.before_remember.set(move || {
        reached_tx.send(()).unwrap();
        resume_rx.recv_timeout(HOOK_TIMEOUT).unwrap();
    });
    let reader = store.clone();
    let worker = thread::spawn(move || {
        let response = reader.evidence_response()?;
        response.finish(())
    });
    reached_rx.recv_timeout(HOOK_TIMEOUT).unwrap();
    store.mark_recovery(RecoveryDisposition::Rebuild);
    // Model completion of recovery without changing disk/pin: only the epoch
    // distinguishes this from the validator's earlier Ready observation.
    store
        .recovery_disposition
        .store(RecoveryDisposition::Ready as u8, Ordering::Release);
    store.recovery_required.store(false, Ordering::Release);
    resume_tx.send(()).unwrap();
    worker.join().unwrap().unwrap();
    let before = counts();
    store.status().unwrap();
    let after = counts();
    assert_eq!((after.0 - before.0, after.1 - before.1), (1, 1));
}

#[test]
fn publication_waits_for_the_snapshot_and_the_next_read_gets_the_new_head() {
    let (_state, root, store) = selected_read_once_tests::published_store(1);
    let old = store.status().unwrap().revision;
    let (snapshot_tx, snapshot_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    store.published_admission.before_lookup.set(move || {
        snapshot_tx.send(()).unwrap();
        resume_rx.recv_timeout(HOOK_TIMEOUT).unwrap();
    });
    let reader = store.clone();
    let reading = thread::spawn(move || {
        let response = reader.evidence_response().unwrap();
        let status = response.status().unwrap();
        let source = response.source_at("f0.js", None).unwrap().unwrap();
        response.finish(()).unwrap();
        (status.revision, source)
    });
    snapshot_rx.recv_timeout(HOOK_TIMEOUT).unwrap();
    fs::write(root.path().join("f0.js"), "function NewHead() {}\n").unwrap();
    let (commit_tx, commit_rx) = mpsc::channel();
    let (allow_tx, allow_rx) = mpsc::channel();
    store.publication_before_commit_hook.set(move || {
        commit_tx.send(()).unwrap();
        allow_rx.recv_timeout(HOOK_TIMEOUT).unwrap();
    });
    let writer = store.clone();
    let path = root.path().to_owned();
    let writing = thread::spawn(move || publish_changed(&writer, &path, old));
    commit_rx.recv_timeout(HOOK_TIMEOUT).unwrap();
    resume_tx.send(()).unwrap();
    let (read_pin, (source_pin, source)) = reading.join().unwrap();
    assert_eq!((read_pin, source_pin), (old, old));
    assert!(source.text.contains("function f0"));
    // Release the read snapshot before waiting for DELETE-journal COMMIT.
    allow_tx.send(()).unwrap();
    let new_pin = writing.join().unwrap();
    assert_ne!(new_pin, old);
    let response = store.evidence_response().unwrap();
    assert_eq!(response.status().unwrap().revision, new_pin);
    assert_eq!(
        response.source_at("f0.js", None).unwrap().unwrap().1.text,
        "function NewHead() {}\n"
    );
    response.finish(()).unwrap();
}

#[test]
fn commit_after_open_before_snapshot_cannot_hit_old_admission() {
    let (_state, _root, store) = selected_read_once_tests::published_store(1);
    store.status().unwrap();
    let error = store
        .status_with_open_hook(|_| {
            let db = store.cache_write()?;
            db.execute(CORRUPT_INVENTORY, [])?;
            Ok(())
        })
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("incompatible_index"),
        "{error:#}"
    );
    assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
}

#[test]
fn warm_admission_does_not_bypass_the_root_finish_fence() {
    let (state, root, store) = selected_read_once_tests::published_store(1);
    store.status().unwrap();
    let before = counts();
    let response = store.evidence_response().unwrap();
    assert_eq!(counts(), before, "response uses the warm admission");
    fs::rename(root.path(), state.path().join("displaced-root")).unwrap();
    fs::create_dir(root.path()).unwrap();
    let error = response.finish(()).unwrap_err();
    assert!(error.to_string().contains("root_changed"), "{error:#}");
}

#[test]
fn writable_uncommitted_control_status_never_uses_or_populates_the_cache() {
    let (_state, _root, store) = selected_read_once_tests::published_store(1);
    let original = store.status().unwrap();
    let mut db = store.cache_write().unwrap();
    let tx = db.transaction().unwrap();
    tx.execute("UPDATE index_metadata SET indexed_at='uncommitted'", [])
        .unwrap();
    let before = counts().0;
    let uncommitted = store.read_status_for(&tx, true).unwrap();
    assert_eq!(uncommitted.indexed_at.as_deref(), Some("uncommitted"));
    assert_eq!(counts().0 - before, 1);
    tx.rollback().unwrap();
    drop(db);
    assert_eq!(store.status().unwrap(), original);
}
