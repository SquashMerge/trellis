use super::*;
use crate::{index_coordinator::IndexJobCoordinator, indexer::IndexOptions};
use std::{fs, sync::atomic::AtomicBool};

/// The stream preflight hook is reached only when the stage publisher consumes
/// the full document spool. Counting coordinator full runs alone would miss a
/// recreate that falls back to the whole-corpus index_workspace_bundle path.
#[test]
fn corrupt_recreate_stages_from_spool_without_losing_prior_head_on_failure() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("a.js"), "function a() { return 1; }\n").unwrap();
    fs::write(work.path().join("b.py"), "def b():\n    return 2\n").unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let first = IndexJobCoordinator::prepare(&store, None)
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    assert_eq!(first.index_revision, 1);
    let index = store.roots.index_db(&store.identity);
    drop(store);
    fs::write(&index, b"short").unwrap();
    let prior_head = fs::read(&index).unwrap();
    let recovering = Store::open_for_tests(state.path(), work.path()).unwrap();
    assert_eq!(
        recovering.disposition(),
        RecoveryDisposition::RecreatePending
    );
    let exclusive = recovering
        .roots
        .index_use_exclusive_existing(&recovering.identity)
        .unwrap();
    let mut leader = recovering
        .roots
        .writer_under_exclusive(recovering.identity.clone(), exclusive)
        .unwrap();
    let seen = Arc::new(AtomicBool::new(false));
    let observed = seen.clone();
    *recovering.test_stream_before_preflight_hook.lock().unwrap() = Some(Box::new(move |spool| {
        assert_eq!(spool.len(), 2);
        observed.store(true, Ordering::Release);
    }));
    let error = recovering
        .recreate_index_exclusive_with_hook(&options, &mut leader, &cancel, |stage| {
            if stage == ActivationStage::BeforeRename {
                anyhow::bail!("stop before replacement");
            }
            Ok(())
        })
        .unwrap_err();
    assert!(
        error.to_string().contains("stop before replacement"),
        "{error:#}"
    );
    assert!(
        seen.load(Ordering::Acquire),
        "recreate bypassed spool publisher"
    );
    assert_eq!(fs::read(&index).unwrap(), prior_head);
    assert_eq!(
        recovering.disposition(),
        RecoveryDisposition::RecreatePending
    );
    let pin = recovering
        .recreate_index_exclusive(&options, &mut leader, &cancel)
        .unwrap();
    assert_eq!(pin.index_revision, 1);
    assert_eq!(recovering.status().unwrap().revision, pin);
}
