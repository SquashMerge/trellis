use trellis::search_vectors::{
    VectorCacheReader, VectorCacheReport, VectorCacheWriter, report_existing,
};

#[test]
fn read_only_inventory_does_not_create_or_touch_cache() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("absent").join("vectors.db");
    assert!(VectorCacheReader::open_existing(&path).unwrap().is_none());
    assert_eq!(
        report_existing(&path).unwrap(),
        VectorCacheReport::default()
    );
    assert!(!directory.path().join("absent").exists());

    let path = directory.path().join("vectors.db");
    let mut writer = VectorCacheWriter::open_with_budget(&path, 32 * 1024).unwrap();
    let model = "a".repeat(64);
    writer.put(&model, &"1".repeat(64), &[1]).unwrap();
    writer.put(&model, &"2".repeat(64), &[2]).unwrap();
    // Reporting must also work with the daemon writer still alive.
    assert_eq!(report_existing(&path).unwrap().entries, 2);
    drop(writer);
    // Never open/close a raw file descriptor while SQLite holds process locks.
    let before = std::fs::read(&path).unwrap();
    let report = report_existing(&path).unwrap();
    assert_eq!(report.entries, 2);
    assert_eq!(report.bytes, before.len() as u64);
    assert_eq!(before, std::fs::read(&path).unwrap());
}

#[test]
fn separate_checkout_consumers_reuse_machine_cache_and_model_namespaces() {
    let directory = tempfile::tempdir().unwrap();
    // Both checkout consumers receive the same machine-wide cache path. There
    // is deliberately no workspace identity in the cache key or API.
    let path = directory.path().join("vectors.db");
    let model_a = "a".repeat(64);
    let model_b = "b".repeat(64);
    let card = "c".repeat(64);
    {
        let mut daemon = VectorCacheWriter::open(&path).unwrap();
        daemon.put(&model_a, &card, &[-128, 0, 127]).unwrap();
        daemon.put(&model_b, &card, &[4, 5]).unwrap();
    }
    for _checkout in ["main", "feature"] {
        let mut reader = VectorCacheReader::open_existing(&path).unwrap().unwrap();
        let snapshot = reader.snapshot().unwrap();
        assert_eq!(snapshot.entry_count(), 2);
        assert_eq!(
            snapshot.get(&model_a, &card).unwrap(),
            Some(vec![-128, 0, 127])
        );
        assert_eq!(snapshot.get(&model_b, &card).unwrap(), Some(vec![4, 5]));
        assert_eq!(snapshot.get(&model_a, &"d".repeat(64)).unwrap(), None);
    }
    assert!(!directory.path().join("index.db").exists());
}

#[test]
fn batch_replaces_and_evicts_old_rows_in_input_lru_order() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("vectors.db");
    let budget = 32 * 1024;
    let model = "a".repeat(64);
    let oldest = "0".repeat(64);
    let replacement = "1".repeat(64);
    let newest = "2".repeat(64);
    let mut writer = VectorCacheWriter::open_with_budget(&path, budget).unwrap();
    writer.put(&model, &oldest, &[1; 1024]).unwrap();
    writer.put(&model, &replacement, &[2; 1024]).unwrap();
    writer
        .put_many(&[
            (&model, &replacement, &[-3; 1024]),
            (&model, &newest, &[4; 1024]),
        ])
        .unwrap();
    {
        let snapshot = writer.snapshot().unwrap();
        assert_eq!(snapshot.entry_count(), 3);
        assert_eq!(
            snapshot.get(&model, &replacement).unwrap(),
            Some(vec![-3; 1024])
        );
        assert_eq!(snapshot.get(&model, &newest).unwrap(), Some(vec![4; 1024]));
    }
    let mut all_evicted = false;
    // Add ordinary 1 KiB rows until the initial rows disappear. A physical
    // page can require more than one deletion, but eviction must remain LRU.
    for number in 3..40 {
        let card = format!("{number:064x}");
        writer.put_many(&[(&model, &card, &[5; 1024])]).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() <= budget);
        let snapshot = writer.snapshot().unwrap();
        let oldest_missing = snapshot.get(&model, &oldest).unwrap().is_none();
        let replacement_missing = snapshot.get(&model, &replacement).unwrap().is_none();
        let newest_missing = snapshot.get(&model, &newest).unwrap().is_none();
        if replacement_missing {
            assert!(oldest_missing);
        }
        if newest_missing {
            assert!(replacement_missing);
            all_evicted = true;
            break;
        }
    }
    assert!(all_evicted);
}
