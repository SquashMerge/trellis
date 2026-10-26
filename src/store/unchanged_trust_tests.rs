use super::*;
use crate::{capture::Capture, indexer::IndexOptions};
use std::{fs, sync::atomic::AtomicBool};

fn fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Store,
    IndexOptions,
    IndexPin,
) {
    let (state, work, store) = super::selected_read_once_tests::published_store(1);
    let options = IndexOptions::new(work.path().to_owned());
    let pin = store.recovery_index_baseline().unwrap().pin().unwrap();
    (state, work, store, options, pin)
}

#[test]
fn unchanged_writers_trust_published_hashes_without_reading_stored_source() {
    for truncated in [false, true] {
        let (_state, _work, store, options, pin) = fixture();
        let cancel = Arc::new(AtomicBool::new(false));
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let baseline = store.recovery_index_baseline().unwrap();
        let leader = store.writer().unwrap();
        let db = store.cache_write().unwrap();
        // Include invalid UTF-8 and a length mismatch. Only stored metadata,
        // not the disposable BLOB, participates in UNCHANGED writer admission.
        db.pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        let bytes = vec![
            0xff;
            if truncated {
                1
            } else {
                capture.files[0].text.len()
            }
        ];
        db.execute(
            "UPDATE document_versions SET source_bytes=?1 WHERE path='f0.js'",
            [bytes],
        )
        .unwrap();
        drop(db);
        take_selected_read_counts();
        assert!(
            store
                .selected_capture_unchanged(&capture, &leader, &baseline, &cancel)
                .unwrap()
        );
        assert_eq!(
            store
                .reuse_unchanged_native_head(&capture, &leader, &baseline, &cancel)
                .unwrap(),
            Some(pin)
        );
        assert_eq!(take_selected_read_counts().source_reads, 0);
        drop(leader);

        // This ruling does not alter public source reads or their corruption
        // handling. They still reject the same stored bytes.
        let response = store.evidence_response().unwrap();
        assert!(response.source_at("f0.js", Some(pin)).is_err());
    }
}

#[test]
fn fresh_working_tree_hash_changes_cannot_complete_unchanged() {
    let (_state, work, store, options, _pin) = fixture();
    let baseline = store.recovery_index_baseline().unwrap();
    fs::write(work.path().join("f0.js"), "function changed(){return 2;}\n").unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let leader = store.writer().unwrap();
    assert!(
        !store
            .selected_capture_unchanged(&capture, &leader, &baseline, &cancel)
            .unwrap()
    );
    assert_eq!(
        store
            .reuse_unchanged_native_head(&capture, &leader, &baseline, &cancel)
            .unwrap(),
        None
    );
}

#[test]
fn unchanged_writer_metadata_and_projection_failures_remain_typed() {
    for sql in [
        "UPDATE document_versions SET byte_length=byte_length+1 WHERE path='f0.js'",
        "UPDATE document_versions SET extraction_context=printf('%064d',0) WHERE path='f0.js'",
        "UPDATE graph_projections SET state='unavailable'",
        "UPDATE class_projections SET state='unavailable'",
        "UPDATE revision_documents SET cid=NULL WHERE path='f0.js'",
        "UPDATE revision_documents SET vid=-1 WHERE path='f0.js'",
    ] {
        let (_state, _work, store, options, _pin) = fixture();
        let cancel = Arc::new(AtomicBool::new(false));
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let baseline = store.recovery_index_baseline().unwrap();
        let leader = store.writer().unwrap();
        let db = store.cache_write().unwrap();
        db.pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        db.pragma_update(None, "foreign_keys", false).unwrap();
        db.execute(sql, []).unwrap();
        drop(db);
        let final_inventory = store
            .selected_capture_unchanged(&capture, &leader, &baseline, &cancel)
            .unwrap_err();
        assert!(
            final_inventory.is::<SelectedIntegrity>(),
            "{sql}: {final_inventory:#}"
        );
        let unchanged = store
            .reuse_unchanged_native_head(&capture, &leader, &baseline, &cancel)
            .unwrap_err();
        assert!(unchanged.is::<SelectedIntegrity>(), "{sql}: {unchanged:#}");
    }
}

#[test]
fn unchanged_completion_keeps_pin_and_cancellation_fences() {
    let (_state, _work, store, options, pin) = fixture();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let mut baseline = store.recovery_index_baseline().unwrap();
    let leader = store.writer().unwrap();
    baseline.pin = Some(IndexPin {
        index_revision: pin.index_revision + 1,
        ..pin
    });
    assert!(
        store
            .reuse_unchanged_native_head(&capture, &leader, &baseline, &cancel)
            .is_err()
    );
    baseline.pin = Some(pin);
    cancel.store(true, Ordering::Release);
    assert!(
        store
            .reuse_unchanged_native_head(&capture, &leader, &baseline, &cancel)
            .is_err()
    );
}
