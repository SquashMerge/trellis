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
    fixture_with_sources(&[
        (
            "src/a.js",
            "function first() { return 1; }\nfunction second() { first(); first(); return 2; }\n",
        ),
        ("src/b.js", "function third() { return 3; }\n"),
        ("empty.js", "// empty\n"),
    ])
}
fn fixture_with_sources(
    sources: &[(&str, &str)],
) -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, source) in sources {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
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
fn top_cognitive_is_bounded_raw_and_scope_selected() {
    let (_state, _root, store, pin) = fixture_with_sources(&[
        (
            "src/a.js",
            "function earlier(x) { if (x) return 1; return 0; }\nfunction later(x) { if (x) return 1; return 0; }\n",
        ),
        (
            "src/b.js",
            "function tied(x) { if (x) return 1; return 0; }\nconst unnamed = x => { if (x) { if (x > 1) return 2; } return 0; };\n",
        ),
        (
            "outside.js",
            "function outside(x) { if (x) { if (x > 1) { if (x > 2) return 3; } } return 0; }\n",
        ),
        ("empty.js", "// empty\n"),
    ]);
    let response = store.evidence_response().unwrap();
    let ordinary = response
        .metrics_at(&MetricsOptions::default(), &mut || Ok(()))
        .unwrap();
    assert!(
        serde_json::to_value(&ordinary)
            .unwrap()
            .get("topCognitive")
            .is_none()
    );
    let source_set_id = ordinary.files[0].source_set_id.clone();
    let revision_top = response
        .metrics_at(
            &MetricsOptions {
                pin: Some(pin),
                top_cognitive: Some(1),
                ..Default::default()
            },
            &mut || Ok(()),
        )
        .unwrap();
    assert_eq!(
        revision_top.top_cognitive.as_ref().unwrap()[0]
            .name
            .as_deref(),
        Some("outside")
    );
    assert!(revision_top.declarations.is_empty());
    let mut selected = MetricsOptions {
        pin: Some(pin),
        scope: "directory".into(),
        source_set_id: Some(source_set_id.clone()),
        path: Some("src".into()),
        top_cognitive: Some(3),
        metrics: vec!["loc".into()],
        ..Default::default()
    };
    let mut raw_top = None;
    for aggregate in ["sum", "max", "mean"] {
        selected.aggregate = aggregate.into();
        let payload = response.metrics_at(&selected, &mut || Ok(())).unwrap();
        assert!(payload.declarations.is_empty());
        let top = payload.top_cognitive.as_ref().unwrap();
        assert_eq!(top.len(), 3);
        assert_eq!(top[0].name, None);
        assert_eq!(top[0].kind, "anonymousFunction");
        assert_eq!(top[0].path, "src/b.js");
        assert!(top[0].metrics.cognitive > top[1].metrics.cognitive);
        assert_eq!(top[0].metrics.cyclomatic, 3);
        assert_eq!(top[0].metrics.max_nesting, 2);
        assert_eq!(top[1].name.as_deref(), Some("earlier"));
        assert_eq!(top[2].name.as_deref(), Some("later"));
        assert!(top.iter().all(|row| row.source_set_id == source_set_id));
        assert_eq!(payload.index_generation, pin.index_generation.to_string());
        assert_eq!(payload.index_revision, pin.index_revision);
        let encoded = bytes(top);
        if let Some(expected) = &raw_top {
            assert_eq!(&encoded, expected);
        }
        raw_top = Some(encoded);
    }
    selected.scope = "file".into();
    selected.path = Some("src/b.js".into());
    selected.language = Some("javascript".into());
    selected.metrics.clear();
    let file = response.metrics_at(&selected, &mut || Ok(())).unwrap();
    assert_eq!(file.declarations.len(), 2);
    assert!(
        serde_json::to_value(&file.declarations[0])
            .unwrap()
            .get("name")
            .is_none()
    );
    let callable = &file.top_cognitive.as_ref().unwrap()[0];
    let measured = file
        .declarations
        .iter()
        .find(|row| row.sid == callable.sid && row.range == callable.range)
        .unwrap();
    assert_eq!(measured.metrics["cognitive"], callable.metrics.cognitive);
    assert_eq!(measured.metrics["cyclomatic"], callable.metrics.cyclomatic);
    assert_eq!(measured.metrics["maxNesting"], callable.metrics.max_nesting);
    selected.scope = "declaration".into();
    selected.language = None;
    selected.sid = Some(file.top_cognitive.as_ref().unwrap()[1].sid.clone());
    let declaration = response.metrics_at(&selected, &mut || Ok(())).unwrap();
    assert_eq!(declaration.top_cognitive.as_ref().unwrap().len(), 1);
    assert_eq!(
        declaration.top_cognitive.as_ref().unwrap()[0]
            .name
            .as_deref(),
        Some("tied")
    );
    selected.scope = "file".into();
    selected.language = Some("javascript".into());
    selected.sid = None;
    selected.path = Some("empty.js".into());
    assert!(
        response
            .metrics_at(&selected, &mut || Ok(()))
            .unwrap()
            .top_cognitive
            .unwrap()
            .is_empty()
    );
    selected.pin = Some(IndexPin {
        index_revision: pin.index_revision + 1,
        ..pin
    });
    assert!(response.metrics_at(&selected, &mut || Ok(())).is_err());
    for count in [0, 11] {
        selected.top_cognitive = Some(count);
        assert!(selected.validate().is_err());
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
        format!("{error:#}").contains("orphan native call owner"),
        "{error:#}"
    );
    assert!(store.recovery_pending_for_tests());
    assert!(response.finish(()).is_err());
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

#[test]
fn file_metadata_uses_captured_physical_lines_and_stored_root_roles() {
    use trellis::native_evidence::DeclarationRole;

    const MIXED: &str = "fn production() {}\n\n// inline tests count toward physical area\n#[cfg(test)]\nmod tests { fn check() {} }\n";
    let sources = [
        ("src/empty.rs", "", 0, DeclarationRole::Production),
        ("tests/empty.rs", "", 0, DeclarationRole::Test),
        (
            "src/no_final.rs",
            "// comment",
            1,
            DeclarationRole::Production,
        ),
        (
            "src/final.rs",
            "// comment\n",
            1,
            DeclarationRole::Production,
        ),
        (
            "src/blanks.rs",
            "// comment\n\n",
            2,
            DeclarationRole::Production,
        ),
        (
            "src/crlf.rs",
            "// comment\r\n\r\n",
            2,
            DeclarationRole::Production,
        ),
        (
            "src/no_callables.rs",
            "const VALUE: usize = 1;\n",
            1,
            DeclarationRole::Production,
        ),
        ("src/stored_test.rs", "", 0, DeclarationRole::Test),
        ("src/mixed.rs", MIXED, 5, DeclarationRole::Production),
    ];
    let indexed: Vec<_> = sources
        .iter()
        .map(|(path, text, _, _)| (*path, *text))
        .collect();
    let (state, root, store, pin) = fixture_with_sources(&indexed);
    // A valid stored role is authoritative even when neither path nor callable
    // population identifies a test file.
    let db = rusqlite::Connection::open(index_path(state.path())).unwrap();
    db.execute(
        "UPDATE native_version_declarations SET role='test' WHERE kind='module' AND vid IN (SELECT vid FROM document_versions WHERE path='src/stored_test.rs')",
        [],
    )
    .unwrap();
    drop(db);
    for (path, _, _, _) in &sources {
        fs::write(
            root.path().join(path),
            "// changed checkout\n// not indexed\n",
        )
        .unwrap();
    }
    let response = store.evidence_response().unwrap();
    for role in ["all", "production", "test"] {
        let payload = response
            .metrics_at(
                &MetricsOptions {
                    pin: Some(pin),
                    role: role.into(),
                    metrics: vec!["cognitive".into()],
                    ..Default::default()
                },
                &mut || Ok(()),
            )
            .unwrap();
        assert_eq!(payload.index_generation, pin.index_generation.to_string());
        assert_eq!(payload.index_revision, pin.index_revision);
        assert_eq!(payload.files.len(), sources.len());
        for (path, _, lines, file_role) in &sources {
            let row = payload.files.iter().find(|row| row.path == *path).unwrap();
            assert_eq!(row.physical_lines, *lines, "{role}: {path}");
            assert_eq!(row.file_role, *file_role, "{role}: {path}");
            let serialized = serde_json::to_value(row).unwrap();
            assert_eq!(serialized["physicalLines"], *lines);
            assert_eq!(
                serialized["fileRole"],
                serde_json::to_value(file_role).unwrap()
            );
            if *path == "src/mixed.rs" {
                assert_eq!((row.role_counts.production, row.role_counts.test), (1, 1));
                assert_eq!(row.declaration_count, if role == "all" { 2 } else { 1 });
            } else {
                assert_eq!((row.role_counts.production, row.role_counts.test), (0, 0));
                assert_eq!(row.declaration_count, 0);
                assert!(row.metrics["cognitive"].is_null());
            }
        }
    }
    response.finish(()).unwrap();
}

#[test]
fn role_selection_folds_raw_callables_at_one_pin_and_preserves_empty_files() {
    use trellis::native_evidence::DeclarationRole;

    const SOURCE: &str = "pub fn production(x: bool) { if x {} }\n#[cfg(feature = \"test-support\")]\npub fn seam() {}\n#[cfg(test)]\nmod tests {\n    fn check(x: bool) { if x { if x {} } }\n}\n";
    let (_state, _root, store, pin) = fixture_with_sources(&[
        ("src/mixed.rs", SOURCE),
        ("src/tests/only.rs", "fn helper() {}\n"),
        ("src/empty.rs", "// no callables\n"),
    ]);
    let response = store.evidence_response().unwrap();
    let manifest = response
        .metrics_at(
            &MetricsOptions {
                pin: Some(pin),
                ..Default::default()
            },
            &mut || Ok(()),
        )
        .unwrap();
    let source_set = manifest.files[0].source_set_id.clone();
    let mut file = MetricsOptions {
        pin: Some(pin),
        scope: "file".into(),
        source_set_id: Some(source_set.clone()),
        language: Some("rust".into()),
        path: Some("src/mixed.rs".into()),
        top_cognitive: Some(10),
        ..Default::default()
    };
    let all = response.metrics_at(&file, &mut || Ok(())).unwrap();
    let source_set = all.files[0].source_set_id.clone();
    file.source_set_id = Some(source_set.clone());
    let expected = [
        ("production", DeclarationRole::Production, 2, 1),
        ("seam", DeclarationRole::Production, 1, 0),
        ("check", DeclarationRole::Test, 3, 3),
    ];
    for (name, role, cyclomatic, cognitive) in expected {
        let top = all
            .top_cognitive
            .as_ref()
            .unwrap()
            .iter()
            .find(|row| row.name.as_deref() == Some(name))
            .unwrap();
        let row = all
            .declarations
            .iter()
            .find(|row| row.sid == top.sid)
            .unwrap();
        assert_eq!(row.role, role, "{name}");
        assert_eq!(row.metrics["cyclomatic"], cyclomatic, "{name}");
        assert_eq!(row.metrics["cognitive"], cognitive, "{name}");
    }
    let test_sid = all
        .declarations
        .iter()
        .find(|row| row.role == DeclarationRole::Test)
        .unwrap()
        .sid
        .clone();
    for (role, mixed_count, total_count, sums, maxima, means, top_name) in [
        (
            "production",
            2,
            2,
            (3.0, 3.0),
            (2.0, 2.0),
            (1.5, 1.5),
            "production",
        ),
        ("all", 3, 4, (6.0, 7.0), (3.0, 3.0), (2.0, 1.75), "check"),
        ("test", 1, 2, (3.0, 4.0), (3.0, 3.0), (3.0, 2.0), "check"),
    ] {
        for (aggregate, (mixed_value, total_value)) in
            [("sum", sums), ("max", maxima), ("mean", means)]
        {
            file.role = role.into();
            file.aggregate = aggregate.into();
            let measured = response.metrics_at(&file, &mut || Ok(())).unwrap();
            assert_eq!(measured.role, role);
            assert_eq!(measured.index_generation, pin.index_generation.to_string());
            assert_eq!(measured.index_revision, pin.index_revision);
            assert_eq!(measured.declarations.len(), mixed_count);
            assert_eq!(measured.files[0].declaration_count, mixed_count);
            assert_eq!(measured.files[0].role_counts.production, 2);
            assert_eq!(measured.files[0].role_counts.test, 1);
            assert_eq!(measured.files[0].physical_lines, SOURCE.lines().count());
            assert_eq!(measured.files[0].file_role, DeclarationRole::Production);
            let serialized = serde_json::to_value(&measured).unwrap();
            assert_eq!(serialized["role"], role);
            assert_eq!(
                serialized["files"][0]["roleCounts"],
                serde_json::json!({"production": 2, "test": 1})
            );
            assert_eq!(
                measured.files[0].metrics["cyclomatic"].as_f64(),
                Some(mixed_value)
            );
            let top = measured.top_cognitive.as_ref().unwrap();
            assert_eq!(top[0].name.as_deref(), Some(top_name));
            assert!(top.iter().all(|row| role == "all"
                || match row.role {
                    DeclarationRole::Production => role == "production",
                    DeclarationRole::Test => role == "test",
                }));
            let revision_options = MetricsOptions {
                pin: Some(pin),
                role: role.into(),
                aggregate: aggregate.into(),
                top_cognitive: Some(10),
                ..Default::default()
            };
            let revision = response
                .metrics_at(&revision_options, &mut || Ok(()))
                .unwrap();
            let directory = response
                .metrics_at(
                    &MetricsOptions {
                        scope: "directory".into(),
                        source_set_id: Some(source_set.clone()),
                        path: Some("src".into()),
                        ..revision_options
                    },
                    &mut || Ok(()),
                )
                .unwrap();
            for payload in [&revision, &directory] {
                assert_eq!(payload.files.len(), 3);
                let fold = payload
                    .directories
                    .iter()
                    .find(|row| row.path == "src")
                    .unwrap();
                assert_eq!(fold.declaration_count, total_count);
                assert_eq!(fold.metrics["cyclomatic"].as_f64(), Some(total_value));
                assert_eq!(
                    payload.top_cognitive.as_ref().unwrap()[0].name.as_deref(),
                    Some(top_name)
                );
                let mixed = payload
                    .files
                    .iter()
                    .find(|row| row.path == "src/mixed.rs")
                    .unwrap();
                assert_eq!(mixed.metrics, measured.files[0].metrics);
                let empty = payload
                    .files
                    .iter()
                    .find(|row| row.path == "src/empty.rs")
                    .unwrap();
                assert_eq!(empty.declaration_count, 0);
                assert_eq!(
                    (empty.role_counts.production, empty.role_counts.test),
                    (0, 0)
                );
                assert!(empty.metrics["cyclomatic"].is_null());
                let test_only = payload
                    .files
                    .iter()
                    .find(|row| row.path == "src/tests/only.rs")
                    .unwrap();
                assert_eq!(
                    (test_only.role_counts.production, test_only.role_counts.test),
                    (0, 1)
                );
                if role == "production" {
                    assert_eq!(test_only.declaration_count, 0);
                    assert!(test_only.metrics["cyclomatic"].is_null());
                    let empty_directory = payload
                        .directories
                        .iter()
                        .find(|row| row.path == "src/tests")
                        .unwrap();
                    assert_eq!(empty_directory.declaration_count, 0);
                    assert!(empty_directory.metrics["cyclomatic"].is_null());
                }
            }
            let root_fold = revision
                .directories
                .iter()
                .find(|row| row.path.is_empty())
                .unwrap();
            assert_eq!(root_fold.declaration_count, total_count);
            assert_eq!(root_fold.metrics["cyclomatic"].as_f64(), Some(total_value));
        }
    }
    let mut declaration = MetricsOptions {
        pin: Some(pin),
        scope: "declaration".into(),
        source_set_id: Some(source_set),
        path: Some("src/mixed.rs".into()),
        sid: Some(test_sid),
        role: "production".into(),
        ..Default::default()
    };
    assert!(
        response
            .metrics_at(&declaration, &mut || Ok(()))
            .unwrap_err()
            .to_string()
            .starts_with("not_found:")
    );
    declaration.role = "test".into();
    let selected = response.metrics_at(&declaration, &mut || Ok(())).unwrap();
    assert_eq!(selected.declarations.len(), 1);
    assert_eq!(selected.declarations[0].role, DeclarationRole::Test);
    file.path = Some("src/tests/only.rs".into());
    file.role = "production".into();
    let empty = response.metrics_at(&file, &mut || Ok(())).unwrap();
    assert!(empty.declarations.is_empty());
    assert!(empty.top_cognitive.unwrap().is_empty());
    assert!(empty.files[0].metrics["cyclomatic"].is_null());
    response.finish(()).unwrap();
}

#[test]
fn stored_roles_cover_four_languages_and_warm_source_memo_is_role_independent() {
    use trellis::native_evidence::DeclarationRole;

    let (_state, _root, store, pin) = fixture_with_sources(&[
        ("src/Example.java", "class Example { void calc() {} }\n"),
        ("tests/Example.java", "class Example { void calc() {} }\n"),
        ("src/example.py", "def calc():\n    return 1\n"),
        ("tests/example.py", "def calc():\n    return 1\n"),
        ("src/example.js", "function calc() { return 1; }\n"),
        ("tests/example.js", "function calc() { return 1; }\n"),
        ("src/example.rs", "fn calc() {}\n"),
        ("tests/example.rs", "fn calc() {}\n"),
    ]);
    let response = store.evidence_response().unwrap();
    let manifest = response
        .metrics_at(
            &MetricsOptions {
                pin: Some(pin),
                ..Default::default()
            },
            &mut || Ok(()),
        )
        .unwrap();
    let source_set = manifest.files[0].source_set_id.clone();
    for (language, name) in [
        ("java", "Example.java"),
        ("python", "example.py"),
        ("javascript", "example.js"),
        ("rust", "example.rs"),
    ] {
        let mut selected = MetricsOptions {
            pin: Some(pin),
            scope: "file".into(),
            source_set_id: Some(source_set.clone()),
            language: Some(language.into()),
            path: Some(format!("src/{name}")),
            top_cognitive: Some(1),
            ..Default::default()
        };
        let production = response.metrics_at(&selected, &mut || Ok(())).unwrap();
        assert_eq!(production.declarations.len(), 1, "{language}");
        assert_eq!(production.declarations[0].role, DeclarationRole::Production);
        assert_eq!(production.files[0].file_role, DeclarationRole::Production);
        let memo_entries = store.metrics_memo_entries_for_tests();
        selected.path = Some(format!("tests/{name}"));
        let tests = response.metrics_at(&selected, &mut || Ok(())).unwrap();
        assert_eq!(
            store.metrics_memo_entries_for_tests(),
            memo_entries,
            "{language}"
        );
        assert_eq!(tests.declarations.len(), 1, "{language}");
        assert_eq!(tests.declarations[0].role, DeclarationRole::Test);
        assert_eq!(tests.files[0].file_role, DeclarationRole::Test);
        assert_eq!(
            tests.files[0].physical_lines,
            production.files[0].physical_lines
        );
        assert_eq!(
            tests.top_cognitive.as_ref().unwrap()[0].role,
            DeclarationRole::Test
        );
        assert_eq!(
            production.declarations[0].metrics,
            tests.declarations[0].metrics
        );
        assert_ne!(production.declarations[0].sid, tests.declarations[0].sid);
        selected.role = "production".into();
        let excluded = response.metrics_at(&selected, &mut || Ok(())).unwrap();
        assert_eq!(excluded.files[0].declaration_count, 0);
        assert!(excluded.files[0].metrics["cyclomatic"].is_null());
        assert_eq!(excluded.files[0].role_counts.test, 1);
        assert_eq!(excluded.files[0].role_counts.production, 0);
        selected.role = "test".into();
        let included = response.metrics_at(&selected, &mut || Ok(())).unwrap();
        assert_eq!(included.declarations[0].role, DeclarationRole::Test);
        assert_eq!(included.files[0].metrics, tests.files[0].metrics);
    }
    response.finish(()).unwrap();
}

#[test]
fn metrics_role_selection_is_closed_and_defaults_to_all() {
    assert_eq!(MetricsOptions::default().role, "all");
    for role in ["production", "all", "test"] {
        assert!(
            MetricsOptions {
                role: role.into(),
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
    }
    for role in ["", "tests", "Production", "production,test"] {
        assert!(
            MetricsOptions {
                role: role.into(),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
}

#[test]
fn missing_stored_file_module_is_typed_corruption_for_an_empty_file() {
    let (state, _root, store, pin) = fixture_with_sources(&[("tests/empty.rs", "")]);
    let db = rusqlite::Connection::open(index_path(state.path())).unwrap();
    db.execute(
        "UPDATE native_version_declarations SET kind='class' WHERE kind='module'",
        [],
    )
    .unwrap();
    drop(db);
    let response = store.evidence_response().unwrap();
    let error = response
        .metrics_at(
            &MetricsOptions {
                pin: Some(pin),
                role: "production".into(),
                ..Default::default()
            },
            &mut || Ok(()),
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("metrics file module"),
        "{error:#}"
    );
    assert!(store.recovery_pending_for_tests());
    assert!(response.finish(()).is_err());
}

#[test]
fn invalid_stored_role_is_corruption_even_when_role_selection_would_exclude_the_file() {
    let (state, _root, store, pin) =
        fixture_with_sources(&[("tests/example.js", "function calc() { return 1; }\n")]);
    let db = rusqlite::Connection::open(index_path(state.path())).unwrap();
    db.execute_batch("PRAGMA ignore_check_constraints=ON")
        .unwrap();
    db.execute("UPDATE native_version_declarations SET role='unknown'", [])
        .unwrap();
    drop(db);
    let response = store.evidence_response().unwrap();
    let error = response
        .metrics_at(
            &MetricsOptions {
                pin: Some(pin),
                role: "production".into(),
                ..Default::default()
            },
            &mut || Ok(()),
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("invalid declaration role"),
        "{error:#}"
    );
    assert!(store.recovery_pending_for_tests());
    assert!(response.finish(()).is_err());
}
