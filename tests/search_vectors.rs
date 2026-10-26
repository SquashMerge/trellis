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
