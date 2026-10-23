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
        "function first() { return 1; }\nfunction second() { first(); first(); return 2; }\n",
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

fn index_path(state: &std::path::Path) -> std::path::PathBuf {
    fs::read_dir(state.join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db")
}
fn unavailable_fan_in() -> serde_json::Value {
    serde_json::json!({"value": null, "unavailable": "call resolution not yet produced (#40/#68)"})
}
fn assert_owned_calls(
    db: &rusqlite::Connection,
    path: &str,
    rows: &[trellis::store::MetricsDeclarationRow],
) {
    let source =
        "function first() { return 1; }\nfunction second() { first(); first(); return 2; }\n";
    let persisted: Vec<(String, Vec<u8>, i64, i64)> = db.prepare(
        "SELECT d.name,d.syntax_id,d.start_byte,d.end_byte FROM native_version_declarations d          JOIN document_versions v ON v.vid=d.vid WHERE v.path=?1 AND d.name IN ('first','second') ORDER BY d.name"
    ).unwrap().query_map([path], |r| Ok((r.get(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_,i64>(2)?, r.get::<_,i64>(3)?)))
        .unwrap().map(Result::unwrap).collect();
    assert_eq!(persisted.len(), 2);
    assert_eq!(rows.len(), 2);
    for (name, sid_blob, start, end) in persisted {
        let sid = format!("sid:v1:{}", hex::encode(sid_blob));
        let expected_start = source.find(&format!("function {name}")).unwrap();
        let expected_end = source[expected_start..].find('}').unwrap() + expected_start + 1;
        assert_eq!(
            (start, end),
            (expected_start as i64, expected_end as i64),
            "{name} persisted range"
        );
        let row = rows
            .iter()
            .find(|row| {
                row.sid == sid && row.range.start == start as usize && row.range.end == end as usize
            })
            .unwrap_or_else(|| panic!("missing persisted {name} sid/range"));
        assert_eq!(row.path, path);
        assert_eq!(
            row.metrics["fanOut"],
            if name == "first" { 0 } else { 2 },
            "{name} call ownership"
        );
        assert_eq!(
            row.metrics["fanIn"],
            unavailable_fan_in(),
            "{name} unavailable fan-in"
        );
    }
}
fn bytes(value: &impl serde::Serialize) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
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
    let index = index_path(state.path());
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

#[test]
fn pinned_payloads_native_ids_and_aggregate_values() {
    let (state, _root, store, pin) = fixture();
    let response = store.evidence_response().unwrap();
    let revision_options = MetricsOptions {
        pin: Some(pin),
        aggregate: "mean".into(),
        ..Default::default()
    };
    let revision = response
        .metrics_at(&revision_options, &mut || Ok(()))
        .unwrap();
    assert_eq!(
        bytes(&revision),
        bytes(
            &response
                .metrics_at(&revision_options, &mut || Ok(()))
                .unwrap()
        )
    );
    let source_set = revision.files[0].source_set_id.clone();
    let mut file = options("file", "src/a.js");
    file.pin = Some(pin);
    file.source_set_id = Some(source_set.clone());
    file.language = Some("javascript".into());
    let rows = response.metrics_at(&file, &mut || Ok(())).unwrap();
    assert_eq!(
        bytes(&rows),
        bytes(&response.metrics_at(&file, &mut || Ok(())).unwrap())
    );
    let db = rusqlite::Connection::open(index_path(state.path())).unwrap();
    assert_owned_calls(&db, "src/a.js", &rows.declarations);
    assert_eq!(rows.files[0].metrics["fanOut"], 2);
    assert_eq!(rows.files[0].metrics["fanIn"], unavailable_fan_in());
    let mut directory = options("directory", "src");
    directory.pin = Some(pin);
    directory.source_set_id = Some(source_set.clone());
    directory.aggregate = "mean".into();
    let subtree = response.metrics_at(&directory, &mut || Ok(())).unwrap();
    assert_eq!(
        bytes(&subtree),
        bytes(&response.metrics_at(&directory, &mut || Ok(())).unwrap())
    );
    let dir = subtree
        .directories
        .iter()
        .find(|d| d.path == "src")
        .unwrap();
    assert_eq!(dir.declaration_count, 3);
    assert_eq!(dir.metrics["fanOut"], serde_json::json!(0.666667));
    assert_eq!(dir.metrics["fanIn"], unavailable_fan_in());
    assert_eq!(
        revision
            .directories
            .iter()
            .find(|d| d.path.is_empty())
            .unwrap()
            .metrics["fanOut"],
        serde_json::json!(0.666667)
    );
    assert_eq!(
        revision
            .directories
            .iter()
            .find(|d| d.path.is_empty())
            .unwrap()
            .metrics["fanIn"],
        unavailable_fan_in()
    );
    assert!(
        subtree
            .files
            .iter()
            .all(|f| f.metrics["fanIn"] == unavailable_fan_in())
    );
    let all_locs: f64 = subtree
        .files
        .iter()
        .flat_map(|f| {
            let mut selected = file.clone();
            selected.path = Some(f.path.clone());
            response
                .metrics_at(&selected, &mut || Ok(()))
                .unwrap()
                .declarations
        })
        .map(|d| d.metrics["loc"].as_f64().unwrap())
        .sum();
    assert_eq!(
        dir.metrics["loc"].as_f64().unwrap(),
        (all_locs / 3.0 * 1_000_000.0).round() / 1_000_000.0
    );
    let mut absent = options("declaration", "src/a.js");
    absent.pin = Some(pin);
    absent.source_set_id = Some(source_set);
    absent.sid = Some("sid:v1:absent".into());
    assert!(
        response
            .metrics_at(&absent, &mut || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("not_found")
    );
}
#[test]
fn work_check_expires_inside_selected_source_read_with_original_error() {
    let (_state, _root, store, _pin) = fixture();
    let response = store.evidence_response().unwrap();
    let mut calls = 0;
    // One admission, three manifest rows, one post-manifest and one document
    // check precede selected-source progress callbacks.
    let err = response
        .metrics_at(&MetricsOptions::default(), &mut || {
            calls += 1;
            if calls == 10 {
                anyhow::bail!("deadline_exceeded: inside selected source read")
            }
            Ok(())
        })
        .unwrap_err();
    assert_eq!(calls, 10);
    assert_eq!(
        err.to_string(),
        "deadline_exceeded: inside selected source read"
    );
}
#[test]
fn selected_fact_corruption_is_typed_and_atomic() {
    let (state, _root, store, _pin) = fixture();
    let db = rusqlite::Connection::open(index_path(state.path())).unwrap();
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    db.execute("UPDATE native_version_calls SET owner_syntax_id=X'00000000000000000000000000000000' WHERE vid IN (SELECT vid FROM document_versions WHERE path='src/a.js')", []).unwrap();
    drop(db);
    let response = store.evidence_response().unwrap();
    let error = response
        .metrics_at(&MetricsOptions::default(), &mut || Ok(()))
        .unwrap_err();
    assert!(
        error.to_string().contains("incompatible_index"),
        "{error:#}"
    );
}

#[test]
fn source_only_memo_rebinds_native_calls_for_new_path_and_pin() {
    let (state, root, store, first_pin) = fixture();
    let first = store.evidence_response().unwrap();
    let mut file = options("file", "src/a.js");
    file.source_set_id = Some(
        first
            .metrics_at(&MetricsOptions::default(), &mut || Ok(()))
            .unwrap()
            .files[0]
            .source_set_id
            .clone(),
    );
    file.language = Some("javascript".into());
    file.pin = Some(first_pin);
    let original = first.metrics_at(&file, &mut || Ok(())).unwrap();
    let db = rusqlite::Connection::open(index_path(state.path())).unwrap();
    assert_owned_calls(&db, "src/a.js", &original.declarations);
    assert_eq!(original.files[0].metrics["fanIn"], unavailable_fan_in());
    drop(db);
    let memo_before = store.metrics_memo_entries_for_tests();
    assert!(memo_before >= 1);
    drop(first);
    fs::write(
        root.path().join("src/c.js"),
        fs::read(root.path().join("src/a.js")).unwrap(),
    )
    .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let leader = store.publication_for_tests().unwrap();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let second_pin = store
        .publish_native(&graph, &capture, &native, &leader, first_pin, &cancel)
        .unwrap();
    drop(leader);
    let second = store.evidence_response().unwrap();
    file.path = Some("src/c.js".into());
    file.pin = Some(second_pin);
    let rebound = second.metrics_at(&file, &mut || Ok(())).unwrap();
    assert_eq!(store.metrics_memo_entries_for_tests(), memo_before);
    assert_eq!(rebound.declarations.len(), original.declarations.len());
    assert_ne!(rebound.declarations[0].sid, original.declarations[0].sid);
    assert_eq!(rebound.files[0].metrics["fanOut"], 2);
    let db = rusqlite::Connection::open(index_path(state.path())).unwrap();
    assert_owned_calls(&db, "src/c.js", &rebound.declarations);
    assert_eq!(rebound.files[0].metrics["fanIn"], unavailable_fan_in());
    let mut directory = options("directory", "src");
    directory.source_set_id = file.source_set_id.clone();
    directory.pin = Some(second_pin);
    directory.aggregate = "mean".into();
    let subtree = second.metrics_at(&directory, &mut || Ok(())).unwrap();
    let aggregate = subtree
        .directories
        .iter()
        .find(|d| d.path == "src")
        .unwrap();
    assert_eq!(aggregate.declaration_count, 5);
    assert_eq!(aggregate.metrics["fanOut"], serde_json::json!(0.8));
    assert_eq!(aggregate.metrics["fanIn"], unavailable_fan_in());
    assert!(
        subtree
            .files
            .iter()
            .all(|row| row.metrics["fanIn"] == unavailable_fan_in())
    );
    let revision = second
        .metrics_at(
            &MetricsOptions {
                pin: Some(second_pin),
                ..Default::default()
            },
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(
        revision
            .directories
            .iter()
            .find(|d| d.path.is_empty())
            .unwrap()
            .metrics["fanIn"],
        unavailable_fan_in()
    );
}
