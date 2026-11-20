//! #1245: a live publication raises a write intent that the Architecture read gives way to,
//! and root-device adoption takes the write lock only when the device actually changed.
use super::*;
use crate::indexer::{IndexOptions, index_workspace_bundle};
use std::sync::atomic::AtomicBool;

fn publish(store: &Store, root: &Path) -> Result<IndexPin> {
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.into()),
        store.root_id(),
        &cancel,
        |_| {},
    )?;
    store.publish_native(
        &graph,
        &capture,
        &native,
        &store.writer()?,
        store.index_baseline()?,
        &cancel,
    )
}

/// The intent is raised inside the transaction, up to its COMMIT, and dropped once the
/// transaction ends: after a COMMIT, and after a publication that fails before it.
#[test]
fn a_live_publication_raises_its_write_intent_until_its_transaction_ends() {
    let (_state, root, store) = selected_read_once_tests::published_store(1);
    assert!(!store.publication_writing());
    let seen = Arc::new(AtomicBool::new(false));
    let (observer, observed) = (store.clone(), seen.clone());
    store.set_publication_before_commit_hook_for_tests(move || {
        observed.store(observer.publication_writing(), Ordering::Release);
    });
    std::fs::write(root.path().join("g.js"), "function g() {}\n").unwrap();
    let published = publish(&store, root.path()).unwrap();
    assert!(seen.load(Ordering::Acquire), "raised before the COMMIT");
    assert!(!store.publication_writing(), "dropped after the COMMIT");
    assert!(store.await_publications_written(Duration::ZERO));

    std::fs::write(root.path().join("h.js"), "function h() {}\n").unwrap();
    store.fail_next_live_publish_commit_busy();
    let failed = publish(&store, root.path()).unwrap_err();
    assert!(failed.to_string().starts_with("storage_busy"), "{failed:#}");
    assert!(!store.publication_writing(), "dropped after the rollback");
    assert_eq!(store.index_baseline().unwrap(), published);
}

/// A waiter on the gate is woken by the last intent to drop, and only by that one.
#[test]
fn the_last_intent_to_drop_ends_the_wait() {
    let (_state, _root, store) = selected_read_once_tests::published_store(1);
    let first = store.write_intent();
    let second = store.write_intent();
    assert!(store.publication_writing());
    assert!(!store.await_publications_written(Duration::ZERO));
    drop(first);
    assert!(store.publication_writing());
    let waiter = {
        let store = store.clone();
        std::thread::spawn(move || store.await_publications_written(crate::testing::HANG_BACKSTOP))
    };
    drop(second);
    assert!(waiter.join().unwrap());
    assert!(!store.publication_writing());
}

/// With the stored device unchanged, adoption writes nothing: it goes ahead while another
/// connection holds the write lock, which even a no-op UPDATE would wait for. A device that
/// did change is still adopted (#813).
#[test]
fn root_device_adoption_takes_the_write_lock_only_when_the_device_changed() {
    let (_state, _root, store) = selected_read_once_tests::published_store(1);
    let writer = store.writer().unwrap();
    let holder = store.cache_write().unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    store.adopt_root_device(&writer).unwrap();
    holder.execute_batch("ROLLBACK").unwrap();
    assert!(!store.publication_writing());

    let other = (store.identity.device ^ 1).to_string();
    holder
        .execute(
            "UPDATE index_metadata SET root_device=?1 WHERE singleton=1",
            [&other],
        )
        .unwrap();
    store.adopt_root_device(&writer).unwrap();
    let stored: String = holder
        .query_row(
            "SELECT root_device FROM index_metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, store.identity.device.to_string());
    assert!(!store.publication_writing());
}
