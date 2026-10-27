//! Exercise startup discovery through the real daemon-wide fill owner.
use super::*;
use crate::{
    daemon::{
        registry::{CheckoutOptions, CheckoutRegistry},
        search::SearchService,
        search_fill::SearchFill,
    },
    indexer::{IndexOptions, index_workspace_bundle},
    search_card::CardFormat,
    search_embed::{
        DIMENSIONS, EmbedError, EmbeddingControl, InputKind, QuantizedVector, SearchEmbedder,
        SearchEmbedderFactory, model_id_for_card_format,
    },
    store::topology::WorkspaceIdentity,
};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};

struct CountingFactory {
    loads: Arc<AtomicUsize>,
    documents: Arc<AtomicUsize>,
}

struct CountingEmbedder(Arc<AtomicUsize>);

impl SearchEmbedderFactory for CountingFactory {
    fn load(&self, control: &EmbeddingControl<'_>) -> Result<Box<dyn SearchEmbedder>, EmbedError> {
        control.check()?;
        self.loads.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(CountingEmbedder(self.documents.clone())))
    }
}

impl SearchEmbedder for CountingEmbedder {
    fn embed(
        &self,
        texts: &[String],
        kind: InputKind,
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<QuantizedVector>, EmbedError> {
        control.check()?;
        assert_eq!(kind, InputKind::Document, "startup must not infer a query");
        self.0.fetch_add(texts.len(), Ordering::Relaxed);
        Ok(texts
            .iter()
            .map(|_| QuantizedVector {
                values: vec![1; DIMENSIONS],
            })
            .collect())
    }
}

fn startup_checkout(state: &std::path::Path, opened_at: u64) -> (tempfile::TempDir, String) {
    let checkout = tempfile::tempdir().unwrap();
    git2::Repository::init(checkout.path()).unwrap();
    fs::write(checkout.path().join("orchard.rs"), "fn orchard() {}\n").unwrap();
    let store = Store::open_for_tests(state, checkout.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(checkout.path().to_owned()),
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
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let root_key = store.root_key().to_owned();
    let identity = WorkspaceIdentity::discover_unattached(Some(checkout.path()), checkout.path())
        .unwrap()
        .attach_existing_marker_readonly()
        .unwrap();
    let roots = TopologyRoots::isolated_for_tests(state.join("cache"), state.join("data"));
    let index_path = roots.index_db(&identity);
    // No live Store/SQLite connection may overlap this fixture-only metadata edit.
    drop(store);
    let db = rusqlite::Connection::open(index_path).unwrap();
    db.execute(
        "UPDATE index_metadata SET last_opened_at=?1 WHERE singleton=1",
        [i64::try_from(opened_at).unwrap()],
    )
    .unwrap();
    drop(db);
    (checkout, root_key)
}

#[test]
fn actual_startup_discovery_selects_four_most_recent_opened_within_seven_days() {
    let state = tempfile::tempdir().unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(state.path().join("cache"), state.path().join("data"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let day = 24 * 60 * 60;
    let mut fixtures = Vec::new();
    let mut expected = Vec::new();
    for age in [day, day, 7 * day - 3_600, 8 * day, 20 * day] {
        let opened_at = now - age;
        let (checkout, key) = startup_checkout(state.path(), opened_at);
        if age <= 7 * day {
            expected.push((opened_at, key));
        }
        fixtures.push(checkout);
    }
    expected.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let source_keys = || {
        let sources = discover_sources(roots.clone());
        assert!(sources.iter().all(|source| source.is_startup()));
        sources
            .iter()
            .map(|source| source.root_key().unwrap())
            .collect::<Vec<_>>()
    };
    // Fewer than four eligible checkouts proves age filtering independently of
    // the limit; equal open times also exercise the root-key tie-break.
    assert_eq!(
        source_keys(),
        expected
            .iter()
            .map(|(_, key)| key.clone())
            .collect::<Vec<_>>(),
    );
    for age_days in [2, 3, 4] {
        let opened_at = now - age_days * day;
        let (checkout, key) = startup_checkout(state.path(), opened_at);
        expected.push((opened_at, key));
        fixtures.push(checkout);
    }
    expected.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    assert_eq!(
        source_keys(),
        expected[..4]
            .iter()
            .map(|(_, key)| key.clone())
            .collect::<Vec<_>>(),
    );
}

#[test]
fn startup_owner_prewarms_cached_head_without_activation_or_document_inference() {
    let state = tempfile::tempdir().unwrap();
    let checkout_root = tempfile::tempdir().unwrap();
    git2::Repository::init(checkout_root.path()).unwrap();
    fs::write(checkout_root.path().join("orchard.rs"), "fn orchard() {}\n").unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(state.path().join("cache"), state.path().join("data"));
    let store = Store::open_for_tests(state.path(), checkout_root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(checkout_root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &store.writer().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(store);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let checkouts = Store::recent_search_checkouts(&roots, now, Duration::MAX, 3).unwrap();
    assert_eq!(checkouts.len(), 1);
    let expected_head =
        Store::read_existing_search_head(&roots, &checkouts[0], &mut || Ok(())).unwrap();
    assert_eq!(expected_head.0, pin);
    assert_eq!(expected_head.1.len(), 1);

    let loads = Arc::new(AtomicUsize::new(0));
    let documents = Arc::new(AtomicUsize::new(0));
    let service = Arc::new(
        SearchService::with_embedder_for_tests(
            roots.clone(),
            Arc::new(CountingFactory {
                loads: loads.clone(),
                documents: documents.clone(),
            }),
        )
        .unwrap(),
    );
    let model_id = model_id_for_card_format(CardFormat::V1.as_str());
    // Initialize the service's own writer before startup so its epoch is stable.
    service
        .with_vector_writer(|writer| {
            for card in expected_head.1.iter() {
                writer.put(&model_id, &card.card_hash, &[1; DIMENSIONS])?;
            }
            Ok(())
        })
        .unwrap();
    let cache_epoch = service.cache_epoch();
    assert_eq!(loads.load(Ordering::Relaxed), 0);

    let identity =
        WorkspaceIdentity::discover_unattached(Some(checkout_root.path()), checkout_root.path())
            .unwrap()
            .attach_existing_marker_readonly()
            .unwrap();
    let mut registry = CheckoutRegistry::with_roots(roots.clone());
    registry
        .register(&identity, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    registry.set_search_service(service.clone());
    let owner = SearchFill::start(service.clone()).unwrap();
    registry.set_search_fill(owner.clone());
    assert_eq!(registry.active_count(), 0);
    assert!(registry.runtime(&identity.root_key).is_none());
    assert_eq!(owner.matrix_storage_bytes(), 0);

    // Stat only: a separate open/close of a live SQLite file can drop its locks.
    let index_path = roots.index_db(&identity);
    let index_state = || {
        let metadata = fs::metadata(&index_path).unwrap();
        (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    };
    let before_index = index_state();
    let index_entries = || {
        let mut entries = fs::read_dir(roots.cache.join("indexes"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    };
    let before_entries = index_entries();

    owner.queue_startup(roots.clone());
    owner.wait_until_idle_for_tests();

    assert!(owner.matrix_storage_bytes() > 0);
    assert_eq!(loads.load(Ordering::Relaxed), 1);
    assert_eq!(documents.load(Ordering::Relaxed), 0);
    assert_eq!(service.cache_epoch(), cache_epoch);
    assert_eq!(registry.active_count(), 0);
    assert!(registry.runtime(&identity.root_key).is_none());
    assert_eq!(index_state(), before_index);
    assert_eq!(index_entries(), before_entries);
    assert_eq!(
        Store::recent_search_checkouts(&roots, now, Duration::MAX, 3).unwrap(),
        checkouts,
    );
    assert_eq!(
        Store::read_existing_search_head(&roots, &checkouts[0], &mut || Ok(())).unwrap(),
        expected_head,
    );
}
