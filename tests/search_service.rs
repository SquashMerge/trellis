use std::sync::atomic::AtomicBool;
use trellis::{
    daemon::search::SearchService,
    search_embed::{Backend, EmbedError},
    search_model::{LexicalState, SearchConfig, SearchIndexState},
    store::topology::TopologyRoots,
};

#[test]
fn daemon_search_is_machine_wide_and_never_opens_a_checkout_index() {
    let directory = tempfile::tempdir().unwrap();
    let roots = TopologyRoots::isolated_for_tests(
        directory.path().join("cache"),
        directory.path().join("data"),
    );
    let service = SearchService::new(
        roots.clone(),
        SearchConfig::from_values(None, Some("cpu")).unwrap(),
    )
    .unwrap();
    let status =
        service
            .status_handle()
            .snapshot("test-card.v1", 0, 0, LexicalState::Unavailable, false);
    assert_eq!(status.backend, Some(Backend::Cpu));
    assert_eq!(status.state, SearchIndexState::Unavailable);
    assert_eq!(service.gc_report().unwrap().vectors.entries, 0);
    assert!(!roots.cache.exists());
    let result: Result<(), EmbedError> =
        service.with_model_job(&AtomicBool::new(false), |_| unreachable!());
    assert!(matches!(result, Err(EmbedError::ModelUnavailable(_))));
    service
        .with_vector_writer(|writer| writer.put(&"a".repeat(64), &"b".repeat(64), &[1, -2]))
        .unwrap();
    assert_eq!(service.vector_report().unwrap().entries, 1);
    assert_eq!(service.gc_report().unwrap().vectors.entries, 1);
    assert!(roots.vectors_db().exists());
    assert!(!roots.cache.join("indexes").exists());
    assert!(!roots.data.exists());
}

#[test]
fn fill_misses_are_read_only_and_model_scoped() {
    let directory = tempfile::tempdir().unwrap();
    let roots = TopologyRoots::isolated_for_tests(
        directory.path().join("cache"),
        directory.path().join("data"),
    );
    let service = SearchService::new(
        roots.clone(),
        SearchConfig::from_values(None, Some("cpu")).unwrap(),
    )
    .unwrap();
    let model = "a".repeat(64);
    let card = "b".repeat(64);
    let missing = "c".repeat(64);
    assert_eq!(
        service.missing_vectors(&model, &[&card, &missing]).unwrap(),
        [true, true]
    );
    assert!(!roots.cache.exists());
    service
        .with_vector_writer(|writer| writer.put_many(&[(&model, &card, &[1; 1024])]))
        .unwrap();
    assert_eq!(
        service.missing_vectors(&model, &[&card, &missing]).unwrap(),
        [false, true]
    );
    assert_eq!(service.missing_vectors(&missing, &[&card]).unwrap(), [true]);
    assert_eq!(service.vector_report().unwrap().entries, 1);
}

#[test]
fn query_snapshot_absence_and_counts_share_one_read_only_transaction() {
    let directory = tempfile::tempdir().unwrap();
    let roots = TopologyRoots::isolated_for_tests(
        directory.path().join("cache"),
        directory.path().join("data"),
    );
    let service = SearchService::new(
        roots.clone(),
        SearchConfig::from_values(None, Some("cpu")).unwrap(),
    )
    .unwrap();
    service
        .with_vector_snapshot(|snapshot| {
            assert!(snapshot.is_none());
            Ok(())
        })
        .unwrap();
    assert!(!roots.cache.exists());
    let model = "a".repeat(64);
    let first = "b".repeat(64);
    let second = "c".repeat(64);
    service
        .with_vector_writer(|writer| {
            writer.put_many(&[(&model, &first, &[1, -2]), (&model, &second, &[2, 1])])
        })
        .unwrap();
    let vectors = service
        .with_vector_snapshot(|snapshot| {
            let snapshot = snapshot.unwrap();
            assert_eq!(snapshot.entry_count(), 2);
            [&first, &second]
                .into_iter()
                .map(|card| snapshot.get(&model, card))
                .collect::<anyhow::Result<Vec<_>>>()
        })
        .unwrap();
    assert_eq!(vectors, [Some(vec![1, -2]), Some(vec![2, 1])]);
    assert_eq!(service.vector_report().unwrap().entries, 2);
    assert!(!roots.cache.join("indexes").exists());
}
