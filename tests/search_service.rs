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
    assert_eq!(status.backend, Backend::Cpu);
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
