#![cfg(feature = "test-support")]
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    model::{CancelFlag, IndexPin},
    store::{MetricsOptions, Store},
};

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(
        root.path().join("src/a.js"),
        "function first() { return 1; }\nfunction second() { first(); return 2; }\n",
    )
    .unwrap();
    fs::write(
        root.path().join("src/b.js"),
        "function third() { return 3; }\n",
    )
    .unwrap();
    fs::write(root.path().join("empty.js"), "// empty\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let pin = store
        .publish_native(&graph, &capture, &native, &leader, baseline, &cancel)
        .unwrap();
    drop(leader);
    (state, root, store, pin)
}
fn options(scope: &str, path: &str) -> MetricsOptions {
    MetricsOptions {
        scope: scope.into(),
        source_set_id: Some("main".into()),
        path: Some(path.into()),
        ..Default::default()
    }
}
#[test]
fn revision_and_scopes_include_zero_callable_files_and_mean_over_declarations() {
    let (_state, _root, store, pin) = fixture();
    let response = store.evidence_response().unwrap();
    let revision = response
        .metrics_at(
            &MetricsOptions {
                pin: Some(pin),
                aggregate: "mean".into(),
                ..Default::default()
            },
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(revision.metrics_version, "metrics:v1");
    assert_eq!(revision.index_revision, pin.index_revision);
    assert!(revision.declarations.is_empty());
    assert_eq!(revision.files.len(), 3);
    assert_eq!(
        revision
            .files
            .iter()
            .map(|f| f.declaration_count)
            .sum::<usize>(),
        3
    );
    assert_eq!(
        revision
            .files
            .iter()
            .find(|f| f.path == "empty.js")
            .unwrap()
            .metrics["loc"],
        serde_json::Value::Null
    );
    let source_set = revision.files[0].source_set_id.clone();
    assert!(
        revision
            .directories
            .iter()
            .any(|d| d.path.is_empty() && d.declaration_count == 3)
    );
    let mut directory = options("directory", "src");
    directory.source_set_id = Some(source_set.clone());
    let subtree = response.metrics_at(&directory, &mut || Ok(())).unwrap();
    assert_eq!(subtree.files.len(), 2);
    assert_eq!(
        subtree
            .directories
            .iter()
            .find(|d| d.path == "src")
            .unwrap()
            .declaration_count,
        3
    );
    assert!(subtree.directories.iter().all(|d| !d.path.is_empty()));
    let mut file = options("file", "src/a.js");
    file.source_set_id = Some(source_set.clone());
    file.language = Some("javascript".into());
    let single = response.metrics_at(&file, &mut || Ok(())).unwrap();
    assert_eq!(single.declarations.len(), 2);
    assert_eq!(single.files.len(), 1);
    let mut declaration = options("declaration", "src/a.js");
    declaration.source_set_id = Some(source_set.clone());
    declaration.sid = Some(single.declarations[0].sid.clone());
    let one = response.metrics_at(&declaration, &mut || Ok(())).unwrap();
    assert_eq!(one.declarations.len(), 1);
    assert!(one.files.is_empty());
    let mut missing = file.clone();
    missing.path = Some("src/missing.js".into());
    assert!(
        response
            .metrics_at(&missing, &mut || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("not_found")
    );
    directory.path = Some("sr".into());
    assert!(
        response
            .metrics_at(&directory, &mut || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("not_found")
    );
    let scores: Vec<_> = single
        .declarations
        .iter()
        .map(|d| d.metrics["loc"].as_f64().unwrap())
        .collect();
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    file.aggregate = "mean".into();
    file.metrics = vec!["loc".into()];
    let averaged = response.metrics_at(&file, &mut || Ok(())).unwrap();
    assert_eq!(averaged.files[0].metrics["loc"].as_f64().unwrap(), mean);
    assert_eq!(
        averaged.declarations[0].metrics.as_object().unwrap().len(),
        1
    );
}
#[test]
fn cancellation_and_pins_are_atomic() {
    let (_state, _root, store, pin) = fixture();
    let response = store.evidence_response().unwrap();
    let mut wrong = pin;
    wrong.index_revision += 1;
    assert!(
        response
            .metrics_at(
                &MetricsOptions {
                    pin: Some(wrong),
                    ..Default::default()
                },
                &mut || Ok(())
            )
            .is_err()
    );
    let flag = AtomicBool::new(false);
    let err = response
        .metrics_at(&MetricsOptions::default(), &mut || {
            if flag.swap(true, Ordering::SeqCst) {
                anyhow::bail!("cancelled")
            }
            Ok(())
        })
        .unwrap_err();
    assert!(err.to_string().contains("cancelled"));
    assert_eq!(
        response
            .metrics_at(&MetricsOptions::default(), &mut || Ok(()))
            .unwrap()
            .files
            .len(),
        3
    );
}

#[test]
fn pinned_source_is_not_reread_from_checkout() {
    let (_state, root, store, pin) = fixture();
    fs::write(root.path().join("src/a.js"), "invalid new source {{{").unwrap();
    let response = store.evidence_response().unwrap();
    let actual = response
        .metrics_at(
            &MetricsOptions {
                pin: Some(pin),
                ..Default::default()
            },
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(
        actual
            .files
            .iter()
            .find(|f| f.path == "src/a.js")
            .unwrap()
            .declaration_count,
        2
    );
}
#[test]
fn selected_corruption_fails_entire_projection() {
    let (state, _root, store, _pin) = fixture();
    let index = std::fs::read_dir(state.path().join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db");
    let db = rusqlite::Connection::open(index).unwrap();
    db.execute(
        "UPDATE document_versions SET source_bytes=zeroblob(byte_length) WHERE path='src/b.js'",
        [],
    )
    .unwrap();
    drop(db);
    let response = store.evidence_response().unwrap();
    assert!(
        response
            .metrics_at(&MetricsOptions::default(), &mut || Ok(()))
            .is_err()
    );
}
