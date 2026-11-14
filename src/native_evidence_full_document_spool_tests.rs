use super::*;
use crate::{indexer::IndexOptions, store::Store};
use std::{fs, sync::Arc, sync::atomic::AtomicBool};

#[test]
fn spooled_full_documents_match_plain_native_and_graph_with_one_claim() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("A.java"),
        "class A { int go(int n) { if (n > 0) return helper(n); return 0; } int helper(int n) { return n; } }\n").unwrap();
    fs::write(
        workspace.path().join("b.py"),
        "def f(x):\n    if x:\n        return f(x - 1)\n    return 0\n",
    )
    .unwrap();
    fs::write(
        workspace.path().join("c.js"),
        "const café = 1; function use() { return caf\\u00e9 + café; }\n",
    )
    .unwrap();
    fs::write(
        workspace.path().join("d.rs"),
        "fn f(r#type: i32) -> i32 { r#type }\n",
    )
    .unwrap();
    fs::write(workspace.path().join("empty.js"), "").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let baseline_capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let baseline = from_capture(&baseline_capture, &root, store.root_id(), &cancel).unwrap();
    assert!(!baseline.references.is_empty());
    assert!(baseline.references.iter().any(|reference| {
        reference.spelling == "caf\\u00e9" && reference.lookup_key == "café"
    }));
    assert!(
        baseline
            .references
            .iter()
            .any(|reference| { reference.spelling == "r#type" && reference.lookup_key == "type" })
    );
    let plain =
        crate::indexer::project_native(&options, &baseline_capture, &baseline, &cancel, &|_| {})
            .unwrap();
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let stages = std::cell::RefCell::new(Vec::new());
    let spool = spool_full_capture(
        &options,
        &capture,
        &root,
        store.root_id(),
        &cancel,
        &|_| {},
        |key, stage| stages.borrow_mut().push((key.path.clone(), stage)),
        &store.native_spool_dir().unwrap(),
    )
    .unwrap();
    let unlinked_path = spool._file.path().to_owned();
    assert!(
        !unlinked_path.exists(),
        "active SQLite spool must be anonymous"
    );
    // Extraction and graph writes happened after unlink. Read through the
    // still-open SQLite connection, then drop it without leaving a stage.
    assert!(!spool.read(0).unwrap().1.files.is_empty());
    let stages = stages.into_inner();
    assert_eq!(capture.graph_projection_count(), 1);
    assert!(
        capture
            .source_operations
            .values()
            .all(|ops| ops.opens == 1 && ops.complete_reads == 1 && ops.hashes == 1)
    );
    assert_eq!(spool.header().revision, baseline.revision);
    assert_eq!(spool.header().producer, baseline.producer);
    assert_eq!(spool.stats(), &plain.stats);
    assert_eq!(spool.diagnostics(), plain.diagnostics.as_slice());
    assert_eq!(stages.len(), 2 * spool.len());
    for (index, pair) in stages.as_chunks::<2>().0.iter().enumerate() {
        let path = &spool.header().revision.documents[index].key.path;
        assert_eq!(
            pair,
            &[
                (path.clone(), FullNativeStage::Measured),
                (path.clone(), FullNativeStage::Validated),
            ]
        );
    }
    for index in 0..spool.len() {
        let path = &spool.header().revision.documents[index].key.path;
        let (native, graph) = spool.read(index).unwrap();
        assert_eq!(spool.read(index).unwrap(), (native.clone(), graph.clone()));
        let expected_references: Vec<_> = baseline
            .references
            .iter()
            .filter(|reference| &reference.document.path == path)
            .cloned()
            .collect();
        assert_eq!(native.references, expected_references);
        let selected = measure_captured_document(
            &baseline_capture,
            &root,
            store.root_id(),
            path,
            &cancel,
            |_| {},
        )
        .unwrap();
        assert_eq!(native.references, selected.references);
        let encoded: Vec<u8> = spool
            .conn
            .query_row(
                "SELECT facts FROM documents WHERE ordinal=?1",
                [i64::try_from(index).unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        let encoded: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(encoded["artifact"]["references"], json!([]));
        let tuples = encoded["references"].as_array().unwrap();
        assert_eq!(tuples.len(), expected_references.len());
        for (tuple, reference) in tuples.iter().zip(&expected_references) {
            assert_eq!(tuple.as_array().unwrap().len(), 7);
            assert_eq!(tuple[5], json!(reference.spelling));
            assert_eq!(
                tuple[6],
                if reference.lookup_key == reference.spelling {
                    Value::Null
                } else {
                    json!(reference.lookup_key)
                }
            );
        }
        if path == "empty.js" {
            assert!(native.references.is_empty());
        }
        assert_eq!(
            native.revision,
            Revision {
                documents: vec![baseline.revision.documents[index].clone()],
                ..baseline.revision.clone()
            }
        );
        assert_eq!(
            native.coverage,
            baseline
                .coverage
                .iter()
                .filter(|r| &r.document_path == path)
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            native.provenance,
            baseline
                .provenance
                .iter()
                .filter(|r| &r.document.path == path)
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            native.declarations,
            baseline
                .declarations
                .iter()
                .filter(|r| &r.document.path == path)
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            native.calls,
            baseline
                .calls
                .iter()
                .filter(|r| &r.document.path == path)
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            native.control_regions,
            baseline
                .control_regions
                .iter()
                .filter(|r| &r.document.path == path)
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            graph.files,
            plain
                .files
                .iter()
                .filter(|r| &r.path == path)
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            graph.nodes,
            plain
                .nodes
                .iter()
                .filter(|r| &r.path == path)
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            graph.calls,
            plain
                .calls
                .iter()
                .filter(|r| &r.path == path)
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            graph.regions,
            plain
                .regions
                .iter()
                .filter(|r| &r.path == path)
                .cloned()
                .collect::<Vec<_>>()
        );
    }
    let js = spool
        .header()
        .revision
        .documents
        .iter()
        .position(|document| document.key.path == "c.js")
        .unwrap();
    let encoded: Vec<u8> = spool
        .conn
        .query_row(
            "SELECT facts FROM documents WHERE ordinal=?1",
            [i64::try_from(js).unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    let payload: Value = serde_json::from_slice(&encoded).unwrap();
    let replace_facts = |bytes: &[u8]| {
        spool
            .conn
            .execute(
                "UPDATE documents SET facts=?1 WHERE ordinal=?2",
                rusqlite::params![bytes, i64::try_from(js).unwrap()],
            )
            .unwrap();
    };
    let (native, _) = spool.read(js).unwrap();
    for (path, value) in [
        ("/unexpected", json!(true)),
        ("/references", json!([["truncated"]])),
        ("/references/0/2", json!(-1)),
        ("/references/0/6", json!(false)),
        ("/artifact/references", json!([native.references[0]])),
        (
            "/artifact/revision/documents",
            json!([native.revision.documents[0]]),
        ),
        ("/artifact/producer/id", json!("wrong")),
        ("/artifact/coverage/0/documentPath", json!("wrong.js")),
        ("/artifact/provenance", json!([])),
        ("/artifact/provenance/0/id", json!("native-proof:v1:wrong")),
        ("/artifact/provenance/0/document/path", json!("wrong.js")),
        ("/artifact/provenance/0/revisionId", json!("wrong")),
        ("/artifact/provenance/0/contentHash", json!("0".repeat(64))),
    ] {
        let mut corrupt = payload.clone();
        if path == "/unexpected" {
            corrupt["unexpected"] = value;
        } else {
            *corrupt.pointer_mut(path).unwrap() = value;
        }
        replace_facts(&serde_json::to_vec(&corrupt).unwrap());
        assert!(spool.read(js).is_err(), "accepted corrupt spool {path}");
    }
    let mut extra_tuple_field = payload.clone();
    extra_tuple_field["references"][0]
        .as_array_mut()
        .unwrap()
        .push(Value::Null);
    replace_facts(&serde_json::to_vec(&extra_tuple_field).unwrap());
    assert!(
        spool.read(js).is_err(),
        "accepted oversized reference tuple"
    );
    replace_facts(&encoded[..encoded.len() - 1]);
    assert!(spool.read(js).is_err(), "accepted truncated spool");
    replace_facts(&encoded);
    assert_eq!(spool.read(js).unwrap().0.references, native.references);
    spool.corrupt_graph_json_for_tests(js);
    assert!(spool.read(js).is_err(), "malformed scratch graph is typed");
    assert_eq!(spool._file.as_file().metadata().unwrap().nlink(), 0);
    drop(spool);
    assert!(!unlinked_path.exists());
    assert!(
        fs::read_dir(store.native_spool_dir().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("native-spool-")),
        "dropped spool must leave no pathname"
    );
}

#[test]
fn native_multifile_spool_ignores_unselected_scip_documents() {
    use protobuf::Message;
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let sources = [
        ("A.java", "class A {}\n", vec![0, 6, 7], "A"),
        ("b.py", "def g():\n    return 1\n", vec![0, 4, 5], "g"),
        ("c.js", "function f() {}\n", vec![0, 9, 10], "f"),
    ];
    let mut manifest = BTreeMap::new();
    let mut index = scip::types::Index::new();
    for (path, source, range, name) in &sources {
        fs::write(workspace.path().join(path), source).unwrap();
        manifest.insert(*path, hash(source.as_bytes()));
        let mut doc = scip::types::Document::new();
        doc.relative_path = (*path).into();
        doc.position_encoding =
            scip::types::PositionEncoding::UTF16CodeUnitOffsetFromLineStart.into();
        let mut occurrence = scip::types::Occurrence::new();
        occurrence.range = range.clone();
        occurrence.symbol_roles = 1;
        occurrence.symbol = format!("scip display {name}()");
        doc.occurrences.push(occurrence);
        index.documents.push(doc);
    }
    fs::write(
        workspace.path().join("index.scip"),
        index.write_to_bytes().unwrap(),
    )
    .unwrap();
    fs::write(
        workspace.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let baseline_capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let baseline_native = from_capture(&baseline_capture, &root, store.root_id(), &cancel).unwrap();
    let baseline = crate::indexer::project_native(
        &options,
        &baseline_capture,
        &baseline_native,
        &cancel,
        &|_| {},
    )
    .unwrap();
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let spool = spool_full_capture(
        &options,
        &capture,
        &root,
        store.root_id(),
        &cancel,
        &|_| {},
        |_, _| {},
        &store.native_spool_dir().unwrap(),
    )
    .unwrap();
    let mut nodes = Vec::new();
    for index in 0..spool.len() {
        let (_, graph) = spool.read(index).unwrap();
        nodes.extend(graph.nodes);
    }
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(nodes, baseline.nodes);
    assert!(nodes.iter().all(|node| node.display_label.is_none()));
    let mut duplicate = scip::types::Document::new();
    duplicate.relative_path = "c.js".into();
    duplicate.language = "java".into();
    index.documents.push(duplicate);
    fs::write(
        workspace.path().join("index.scip"),
        index.write_to_bytes().unwrap(),
    )
    .unwrap();
    let duplicate_capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let duplicate_native =
        from_capture(&duplicate_capture, &root, store.root_id(), &cancel).unwrap();
    let duplicate_graph = crate::indexer::project_native(
        &options,
        &duplicate_capture,
        &duplicate_native,
        &cancel,
        &|_| {},
    )
    .unwrap();
    let duplicate_spool_capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let duplicate_spool = spool_full_capture(
        &options,
        &duplicate_spool_capture,
        &root,
        store.root_id(),
        &cancel,
        &|_| {},
        |_, _| {},
        &store.native_spool_dir().unwrap(),
    )
    .unwrap();
    let mut duplicate_nodes = Vec::new();
    for index in 0..duplicate_spool.len() {
        let (_, graph) = duplicate_spool.read(index).unwrap();
        duplicate_nodes.extend(graph.nodes);
    }
    duplicate_nodes.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(duplicate_nodes, duplicate_graph.nodes);
    assert!(
        duplicate_nodes
            .iter()
            .all(|node| node.display_label.is_none())
    );
    assert_eq!(
        nodes.iter().map(|node| &node.id).collect::<Vec<_>>(),
        duplicate_nodes
            .iter()
            .map(|node| &node.id)
            .collect::<Vec<_>>()
    );
}

#[test]
fn cancellation_after_first_spooled_document_skips_next_chunk() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.py"), "def first(): return 1\n").unwrap();
    fs::write(workspace.path().join("b.py"), "def second(): return 2\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let stages = std::cell::RefCell::new(Vec::new());
    let error = spool_full_capture_with(
        &options,
        &capture,
        &root,
        store.root_id(),
        &cancel,
        &|progress| {
            if progress.phase == "parse" && progress.completed == 1 {
                cancel.store(true, std::sync::atomic::Ordering::Release);
            }
        },
        |key, stage| stages.borrow_mut().push((key.path.clone(), stage)),
        ExtractionPlan {
            workers: 2,
            chunk: 1,
        },
        &|_, _| Ok(()),
        &|_, _| Ok(()),
        &|_, _, _| Ok(()),
        &|_, _| Ok(()),
        &store.native_spool_dir().unwrap(),
    )
    .err()
    .expect("cancellation must stop the next chunk");
    assert!(
        error.to_string().contains("indexing cancelled"),
        "{error:#}"
    );
    assert_eq!(
        stages.into_inner(),
        vec![
            ("a.py".into(), FullNativeStage::Measured),
            ("a.py".into(), FullNativeStage::Validated),
        ]
    );
    assert!(
        fs::read_dir(store.native_spool_dir().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("native-spool-")),
        "failed spool must not leave a scratch pathname"
    );
}

#[test]
fn earliest_validation_error_precedes_later_extraction_error() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.py"), "def first(): return 1\n").unwrap();
    fs::write(workspace.path().join("b.py"), "def second(): return 2\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let stages = std::cell::RefCell::new(Vec::new());
    let (completed, observed) = std::sync::mpsc::sync_channel(1);
    let observed = std::sync::Mutex::new(observed);
    let err = spool_full_capture_with(
        &options,
        &capture,
        &root,
        store.root_id(),
        &cancel,
        &|_| {},
        |key, stage| stages.borrow_mut().push((key.path.clone(), stage)),
        ExtractionPlan {
            workers: 2,
            chunk: 2,
        },
        &|file, _ids| {
            if file.path == "b.py" {
                completed.send(()).unwrap();
                anyhow::bail!("injected second extraction failure")
            }
            Ok(())
        },
        &|document, facts| {
            if document.key.path == "a.py" {
                observed
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(60))
                    .expect("later worker failed before earlier validation");
                facts.declarations[0].syntax_id = "sid:v1:invalid".into();
            }
            Ok(())
        },
        &|_, _, _| Ok(()),
        &|_, _| Ok(()),
        &store.native_spool_dir().unwrap(),
    )
    .err()
    .expect("ordered failure");
    assert!(
        format!("{err:#}").contains("native stable ID collision/duplicate"),
        "unexpected earlier validation error: {err:#}"
    );
    assert!(
        !format!("{err:#}").contains("injected second extraction failure"),
        "{err:#}"
    );
    // Validation runs on the worker, before the ordered owner writes it.
    assert_eq!(stages.into_inner(), vec![]);
    assert_eq!(capture.graph_projection_count(), 1);
}

#[test]
fn earlier_class_failure_beats_completed_later_native_extraction_failure() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.py"), "class First:\n    pass\n").unwrap();
    fs::write(workspace.path().join("b.py"), "def second(): return 2\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let (completed, observed) = std::sync::mpsc::sync_channel(1);
    let observed = std::sync::Mutex::new(observed);
    let stages = std::cell::RefCell::new(Vec::new());
    let err = spool_full_capture_with(
        &options,
        &capture,
        &root,
        store.root_id(),
        &cancel,
        &|_| {},
        |key, stage| stages.borrow_mut().push((key.path.clone(), stage)),
        ExtractionPlan {
            workers: 2,
            chunk: 2,
        },
        &|file, _ids| {
            if file.path == "b.py" {
                completed.send(()).unwrap();
                anyhow::bail!("injected later native extraction failure")
            }
            Ok(())
        },
        &|_, _| Ok(()),
        &|_, _, _| Ok(()),
        &|document, _extraction| {
            if document.key.path == "a.py" {
                // Hold this earlier file until the later worker has failed.
                observed
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(60))
                    .expect("later worker completed before ordered class check");
                anyhow::bail!("injected earlier class extraction failure")
            }
            Ok(())
        },
        &store.native_spool_dir().unwrap(),
    )
    .err()
    .expect("earliest sorted file failure must be reported");
    assert!(
        err.to_string()
            .contains("injected earlier class extraction failure"),
        "{err:#}"
    );
    assert_eq!(stages.into_inner(), vec![]);
}

#[test]
fn earliest_extraction_error_precedes_later_validation_error() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.py"), "def first(): return 1\n").unwrap();
    fs::write(workspace.path().join("b.py"), "def second(): return 2\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let err = spool_full_capture_with(
        &options,
        &capture,
        &root,
        store.root_id(),
        &cancel,
        &|_| {},
        |_, _| {},
        ExtractionPlan {
            workers: 2,
            chunk: 2,
        },
        &|file, _ids| {
            if file.path == "a.py" {
                anyhow::bail!("injected first extraction failure")
            }
            Ok(())
        },
        &|_, _| anyhow::bail!("injected later validation failure"),
        &|_, _, _| Ok(()),
        &|_, _| Ok(()),
        &store.native_spool_dir().unwrap(),
    )
    .err()
    .expect("ordered failure");
    assert!(
        err.to_string()
            .contains("injected first extraction failure"),
        "{err:#}"
    );
}

#[test]
fn failed_file_reports_extraction_error_before_its_minted_ids_collide() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.py"), "def first(): return 1\n").unwrap();
    fs::write(workspace.path().join("b.py"), "def second(): return 2\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    // Pre-extract the same captured later file only to select its real
    // first minted identity. This fixture creates no index.db rows.
    let (mut shell, files) = build_native_header(&capture, &root, store.root_id()).unwrap();
    let later = shell
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == "b.py")
        .unwrap()
        .clone();
    shell.revision.documents.clear();
    let mut later_ids = IdentityRegistry::logged();
    extract_known(
        &mut shell,
        files.iter().find(|f| f.path == "b.py").unwrap(),
        &later,
        &mut later_ids,
    )
    .unwrap();
    let minted_in_failed_file = Arc::new(AtomicBool::new(false));
    let seen = minted_in_failed_file.clone();
    let err = spool_full_capture_with(
        &options,
        &capture,
        &root,
        store.root_id(),
        &cancel,
        &|_| {},
        |_, _| {},
        ExtractionPlan {
            workers: 2,
            chunk: 2,
        },
        &|file, ids| {
            if file.path == "b.py" {
                assert!(ids.same_first_identity_for_tests(&later_ids));
                seen.store(true, std::sync::atomic::Ordering::SeqCst);
                anyhow::bail!("injected in-file extraction failure")
            }
            Ok(())
        },
        &|_, _| Ok(()),
        &|document, global, _local| {
            if document.key.path == "a.py" {
                global.inject_collision_for_tests(&later_ids);
                let mut probe = global.clone();
                assert!(
                    probe
                        .absorb(later_ids.clone())
                        .unwrap_err()
                        .to_string()
                        .contains("identity handle collision")
                );
            }
            Ok(())
        },
        &|_, _| Ok(()),
        &store.native_spool_dir().unwrap(),
    )
    .err()
    .expect("failed file must be rejected");
    assert!(minted_in_failed_file.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        err.to_string()
            .contains("injected in-file extraction failure"),
        "{err:#}"
    );
}

fn private_spool_failure_preserves_prior_head(allocation: bool) {
    use crate::index_coordinator::IndexJobCoordinator;
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.py"), "def first(): return 1\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let prior = IndexJobCoordinator::prepare(&store, None)
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    fs::write(workspace.path().join("b.py"), "def second(): return 2\n").unwrap();
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let root = fs::canonicalize(workspace.path()).unwrap();
    let stages = std::cell::RefCell::new(Vec::new());
    let err = spool_full_capture_with(
        &options,
        &capture,
        &root,
        store.root_id(),
        &cancel,
        &|_| {},
        |key, stage| stages.borrow_mut().push((key.path.clone(), stage)),
        ExtractionPlan {
            workers: 2,
            chunk: 2,
        },
        &|_, _| Ok(()),
        &|_, _| Ok(()),
        &|document, global, local| {
            if document.key.path == "b.py" {
                if allocation {
                    global.fail_next_reservation_for_tests();
                } else {
                    global.inject_collision_for_tests(local);
                }
            }
            Ok(())
        },
        &|_, _| Ok(()),
        &store.native_spool_dir().unwrap(),
    )
    .err()
    .expect("the digest collision or allocation failure must reject scratch");
    if allocation {
        assert!(err.to_string().contains("memory allocation"), "{err:#}");
    } else {
        assert!(
            err.to_string().contains("identity handle collision"),
            "{err:#}"
        );
    }
    assert!(
        stages
            .borrow()
            .contains(&("b.py".into(), FullNativeStage::Measured)),
        "a disposable spool write can precede its collision verdict"
    );
    assert_eq!(store.index_baseline().unwrap(), prior);
    let previous = store.graph_at(Some(prior)).unwrap();
    assert_eq!(previous.files.len(), 1);
    assert_eq!(previous.files[0].path, "a.py");
}

#[test]
fn collision_after_private_spool_write_cannot_change_compatible_prior_head() {
    private_spool_failure_preserves_prior_head(false);
}

#[test]
fn fallible_allocation_after_private_spool_write_preserves_prior_head() {
    private_spool_failure_preserves_prior_head(true);
}

#[test]
fn spool_packing_never_hides_corrupt_reference_fields() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.py"), "def f(x):\n    return x\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    for field in ["document", "revision", "proof", "range", "owner", "lookup"] {
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let error = spool_full_capture_with(
            &options,
            &capture,
            &root,
            store.root_id(),
            &cancel,
            &|_| {},
            |_, _| {},
            ExtractionPlan {
                workers: 1,
                chunk: 1,
            },
            &|_, _| Ok(()),
            &|_, artifact| {
                assert!(!artifact.references.is_empty());
                let reference = &mut artifact.references[0];
                match field {
                    "document" => reference.document.path = "wrong.py".into(),
                    "revision" => reference.revision_id = "wrong".into(),
                    "proof" => reference.provenance_id = "wrong".into(),
                    "range" => reference.range.end = usize::MAX,
                    "owner" => reference.owner_syntax_id = "sid:v1:missing".into(),
                    "lookup" => reference.lookup_key = "wrong".into(),
                    _ => unreachable!(),
                }
                Ok(())
            },
            &|_, _, _| Ok(()),
            &|_, _| Ok(()),
            &store.native_spool_dir().unwrap(),
        )
        .err()
        .unwrap_or_else(|| panic!("packing hid corrupt reference {field}"));
        assert!(!error.to_string().is_empty());
    }

    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let native = from_capture(&capture, &root, store.root_id(), &cancel).unwrap();
    for fault in [
        ExtractionAuthFault::Undeclared,
        ExtractionAuthFault::Mismatched,
    ] {
        let _guard = inject_extraction_auth_fault(&native.revision.id, "python", fault);
        assert!(native.validate_structure(&capture.files).is_err());
    }
    let document = &native.revision.documents[0];
    for field in ["document", "revision", "proof"] {
        let mut corrupt = native.clone();
        match field {
            "document" => corrupt.references[0].document.path = "wrong.py".into(),
            "revision" => corrupt.references[0].revision_id = "wrong".into(),
            "proof" => corrupt.references[0].provenance_id = "wrong".into(),
            _ => unreachable!(),
        }
        assert!(
            encode_spool_facts(corrupt, document).is_err(),
            "encoder elided corrupt reference {field}"
        );
    }
}

#[test]
fn spooled_document_validator_rejects_corrupt_range_and_owner() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.py"), "def f():\n    return f()\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let spool = spool_full_capture(
        &options,
        &capture,
        &root,
        store.root_id(),
        &cancel,
        &|_| {},
        |_, _| {},
        &store.native_spool_dir().unwrap(),
    )
    .unwrap();
    let (mut native, _) = spool.read(0).unwrap();
    let document = native.revision.documents[0].clone();
    let file = &capture.files[0];
    native.revision.documents.clear();
    native.calls[0].owner_syntax_id = "sid:v1:missing".into();
    assert!(validate_spooled_document(&mut native, &document, file).is_err());
    let (mut native, _) = spool.read(0).unwrap();
    native.revision.documents.clear();
    native.calls[0].range.end = file.text.len() + 1;
    assert!(validate_spooled_document(&mut native, &document, file).is_err());
    let (mut native, _) = spool.read(0).unwrap();
    native.revision.documents.clear();
    let mut changed = file.clone();
    changed.text = "def f():\n    return z()\n".into();
    assert!(validate_spooled_document(&mut native, &document, &changed).is_err());
}
