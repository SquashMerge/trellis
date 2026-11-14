use super::*;
use crate::{
    index_coordinator::{IndexJobCoordinator, recreate_capture},
    indexer::IndexOptions,
};
use std::{cell::Cell, fs, os::unix::fs::MetadataExt, sync::atomic::AtomicBool};

/// Whether another descriptor can take the index use lock shared without waiting: true only
/// once the recovering leader no longer holds it exclusively.
fn use_lock_is_shared(store: &Store) -> bool {
    match store.roots.index_use_existing_readonly(&store.identity) {
        Ok(_) => true,
        Err(error) => {
            assert!(error.is::<topology::StorageBusy>(), "{error:#}");
            false
        }
    }
}

/// The caller's capture is the hook: it observes the lock and recovery state at the moment
/// recreation calls it, then runs the real capture. An in-place rebuild calls it after the
/// exclusive use lock is downgraded to shared, the disposition is `Rebuild` and the
/// publication baseline is read, and before anything is published.
#[test]
fn in_place_rebuild_captures_after_the_downgrade_and_before_publication() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("a.js"), "function a() { return 1; }\n").unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let first = IndexJobCoordinator::prepare(&store, None)
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    let index = store.roots.index_db(&store.identity);
    drop(store);
    let inode = fs::metadata(&index).unwrap().ino();
    // Classified for recreation, but the file is sound: the exclusive probe finds no
    // file-level damage and recreation rebuilds in place.
    let recovering = Store::open_for_tests(state.path(), work.path()).unwrap();
    recovering.mark_recovery(RecoveryDisposition::RecreatePending);
    let exclusive = recovering
        .roots
        .index_use_exclusive_existing(&recovering.identity)
        .unwrap();
    let mut leader = recovering
        .roots
        .writer_under_exclusive(recovering.identity.clone(), exclusive)
        .unwrap();
    assert!(!use_lock_is_shared(&recovering));
    let captures = Cell::new(0);
    let pin = recovering
        .recreate_index_exclusive(&options, &mut leader, &cancel, &mut || {
            captures.set(captures.get() + 1);
            assert!(
                use_lock_is_shared(&recovering),
                "the capture ran before the use lock was downgraded"
            );
            assert_eq!(recovering.disposition(), RecoveryDisposition::Rebuild);
            assert_eq!(
                recovering.publication_index_baseline()?.pin(),
                Some(first),
                "the capture ran after publication"
            );
            recreate_capture(&recovering, &options, &cancel)
        })
        .unwrap();
    assert_eq!(captures.get(), 1);
    assert_ne!(pin, first);
    assert_eq!(fs::metadata(&index).unwrap().ino(), inode);
    assert_eq!(recovering.status().unwrap().revision, pin);
}

/// A replacement calls the capture while use is still exclusive, after the private stage
/// exists and before the damaged index is touched.
#[test]
fn replacement_captures_under_exclusive_use_before_the_index_is_replaced() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("a.js"), "function a() { return 1; }\n").unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let index = store.roots.index_db(&store.identity);
    let dir = store.roots.index_dir(&store.identity);
    drop(store);
    fs::write(&index, b"short").unwrap();
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
    let captures = Cell::new(0);
    let pin = recovering
        .recreate_index_exclusive(&options, &mut leader, &cancel, &mut || {
            captures.set(captures.get() + 1);
            assert!(
                !use_lock_is_shared(&recovering),
                "the capture ran after the use lock was downgraded"
            );
            assert_eq!(
                recovering.disposition(),
                RecoveryDisposition::RecreatePending
            );
            let staged = fs::read_dir(&dir)?
                .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                .filter(|name| name.starts_with("index.db.tmp-"))
                .count();
            assert_eq!(
                staged, 1,
                "the capture ran before the private stage existed"
            );
            assert_eq!(fs::read(&index)?, b"short");
            recreate_capture(&recovering, &options, &cancel)
        })
        .unwrap();
    assert_eq!(captures.get(), 1);
    assert_eq!(pin.index_revision, 1);
    assert!(use_lock_is_shared(&recovering));
    assert_eq!(recovering.status().unwrap().revision, pin);
}
