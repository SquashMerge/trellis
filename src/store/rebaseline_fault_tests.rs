use super::*;
use crate::indexer::{IndexOptions, index_workspace_bundle};
use std::{fs, sync::atomic::AtomicBool};

/// Zero a selected document's stored source bytes in place (same length,
/// wrong hash), through the store's own connection.
fn corrupt_stored_source(store: &Store, pin: IndexPin, path: &str) {
    let mut db = store.cache_write().unwrap();
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let selected = store.read_revision_for(&tx, Some(pin), true).unwrap();
    let changed = tx
        .execute(
            "UPDATE document_versions SET source_bytes=zeroblob(length(source_bytes))
                 WHERE id=(SELECT m.document_version_id FROM revision_documents m
                 WHERE m.revision_id=?1 AND m.path=?2)",
            params![selected.key, path],
        )
        .unwrap();
    assert_eq!(changed, 1);
    tx.commit().unwrap();
}

fn local_edit_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Store,
    IndexOptions,
    IndexPin,
) {
    use crate::index_coordinator::IndexJobCoordinator;
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("local.js"),
        "function local(){return 1;}\n",
    )
    .unwrap();
    fs::write(
        work.path().join("A.java"),
        "class A { int run(){return 1;} }\n",
    )
    .unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let first = job.run(&options, &cancel, |_| {}).unwrap();
    (state, work, store, options, first)
}

fn prepare_one_file_edit(store: &Store, options: &IndexOptions) -> Result<Option<LocalPrepared>> {
    use crate::capture::Capture;
    fs::write(
        options.workspace_root.join("local.js"),
        "function local(){return 2;}\n",
    )
    .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(options, &cancel, &|_| {}).unwrap();
    let baseline = store.recovery_index_baseline().unwrap();
    store.prepare_local_revision(&capture, &baseline, &cancel)
}

/// #141 (owner ruling): an unchanged prior document is trusted by its
/// stored content hash; its bytes are not read or re-hashed on a local
/// edit of another document.
#[test]
fn a_local_edit_does_not_reread_unchanged_prior_documents() {
    let (_state, _work, store, options, pin) = local_edit_fixture();
    corrupt_stored_source(&store, pin, "A.java");
    let prepared = prepare_one_file_edit(&store, &options).unwrap();
    assert_eq!(
        prepared
            .expect("the one-file edit stays local")
            .changed_path,
        "local.js"
    );
}

/// The changed document's stored bytes are still read and witness-checked.
#[test]
fn a_local_edit_still_checks_the_changed_documents_stored_bytes() {
    let (_state, _work, store, options, pin) = local_edit_fixture();
    corrupt_stored_source(&store, pin, "local.js");
    let Err(error) = prepare_one_file_edit(&store, &options) else {
        panic!("a corrupt changed document must not prepare");
    };
    assert!(
        format!("{error:#}").contains("prior captured source witness mismatch"),
        "{error:#}"
    );
}

#[test]
fn selective_publication_faults_rollback_and_retry_same_expected_pin() {
    use crate::{capture::Capture, index_coordinator::IndexJobCoordinator, indexer};
    for failure_at in [PublishStage::AfterFile, PublishStage::BeforeCommit] {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        fs::write(
            work.path().join("local.js"),
            "function local(){return 1;}\n",
        )
        .unwrap();
        fs::write(
            work.path().join("A.java"),
            "class A { int run(){return 1;} }\n",
        )
        .unwrap();
        let options = IndexOptions::new(work.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let store = Store::open_for_tests(state.path(), work.path()).unwrap();
        let job = IndexJobCoordinator::prepare(&store, None).unwrap();
        let session = job.session();
        let first = job.run(&options, &cancel, |_| {}).unwrap();
        fs::write(
            work.path().join("local.js"),
            "function local(){return 2;}\n",
        )
        .unwrap();
        store.begin_writer_publication(&session).unwrap();
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let baseline = store.recovery_index_baseline().unwrap();
        let native = store
            .prepare_local_native(&capture, &baseline, &cancel)
            .unwrap()
            .expect("same-leaf source edit selects one native document");
        let graph = indexer::project_native(&options, &capture, &native, &cancel, &|_| {}).unwrap();
        store
            .validate_native_bundle_with_mode(&graph, &capture, &native, &cancel, true)
            .unwrap();
        let prepared = store
            .prepare_local_revision(&capture, &baseline, &cancel)
            .unwrap()
            .expect("same-leaf edit has local selected projection");
        let inventory_fault = crate::native_evidence::inject_extraction_auth_fault(
            &prepared.native.revision.id,
            "java",
            crate::native_evidence::ExtractionAuthFault::Mismatched,
        );
        let mismatch = store
            .preflight_local_reuse(&graph, &prepared)
            .err()
            .expect("synthetic mismatched declared component must refuse local reuse");
        assert!(
            format!("{mismatch:#}").contains("mismatched native extraction component: config"),
            "local reuse ignored producer's declared extraction inventory: {mismatch:#}"
        );
        drop(inventory_fault);
        assert_eq!(
            ReadRevision::current(&store.cache().unwrap()).unwrap().pin,
            first,
            "refused local preflight installed partial metadata"
        );
        let err = store
            .publish_inner_checked_expected(
                (&graph, &capture, &native),
                session.writer_guard().unwrap(),
                ExpectedPublication::Recovery(Box::new(baseline)),
                &cancel,
                256 * 1024 * 1024 + 16 * 1024,
                |stage, _db| {
                    if stage == failure_at {
                        // A separate reader must still see the committed old pin.
                        let outside = store.cache()?;
                        let before: i64 = outside.query_row(
                            "SELECT index_revision FROM index_metadata",
                            [],
                            |r| r.get(0),
                        )?;
                        assert_eq!(
                            before, first.index_revision as i64,
                            "uncommitted selected rows remain invisible"
                        );
                        anyhow::bail!("injected selective fault");
                    }
                    Ok(())
                },
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("injected selective fault"),
            "{err:#}"
        );
        let db = store.cache().unwrap();
        let unchanged = ReadRevision::current(&db).unwrap();
        assert_eq!(unchanged.pin, first, "failure preserves old current pin");
        let rows: i64 = db
            .query_row("SELECT count(*) FROM native_revisions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "no pending revision survives rollback");
        assert_eq!(
            db.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert!(
            db.prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query([])
                .unwrap()
                .next()
                .unwrap()
                .is_none(),
            "selective rollback leaves all foreign keys valid"
        );
        drop(db);
        let retry = IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone())
            .unwrap()
            .run(&options, &cancel, |_| {})
            .unwrap();
        assert_eq!(retry.index_revision, first.index_revision + 1);
        assert!(
            store
                .source_at("local.js", Some(first))
                .unwrap()
                .unwrap()
                .1
                .text
                .contains("return 1")
        );
        assert!(
            store
                .source_at("local.js", Some(retry))
                .unwrap()
                .unwrap()
                .1
                .text
                .contains("return 2")
        );
        assert_eq!(
            store
                .last_writer_counters()
                .unwrap()
                .reused_occurrence_reads,
            0
        );
    }
}

#[test]
fn persisted_local_classifier_falls_back_on_affected_uncertainty_not_unrelated_ambiguity() {
    use crate::{capture::Capture, index_coordinator::IndexJobCoordinator};
    let cases = [
        ("unrelated lexical duplicate", "body", true),
        ("added source", "add", false),
        ("deleted source", "delete", false),
        ("renamed path", "rename", false),
        ("parser recovery", "parse", false),
        ("changed scope", "scope", false),
        ("changed import", "import", false),
        ("changed reexport", "export", false),
        ("changed supertype", "supertype", false),
        ("affected unproved lookup", "lookup", false),
        (
            "affected lexical duplicate candidate",
            "ambiguous_lookup",
            false,
        ),
    ];
    for (label, change, local) in cases {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path();
        fs::write(path.join("target.js"), "function target() { return 1; }\n").unwrap();
        // Duplicate lexical candidates are #22 advisory facts, NOT semantic bindings.
        fs::write(
            path.join("duplicate.js"),
            "function target() { return 3; }\nfunction target() { return 4; }\n",
        )
        .unwrap();
        fs::write(
            path.join("A.java"),
            "class A extends Base { int run(){ return 1; } }\nclass Base {}\n",
        )
        .unwrap();
        let options = IndexOptions::new(path.to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let store = Store::open_for_tests(state.path(), path).unwrap();
        let job = IndexJobCoordinator::prepare(&store, None).unwrap();
        let session = job.session();
        let first = job.run(&options, &cancel, |_| {}).unwrap();
        let old = store.graph_at(Some(first)).unwrap();
        assert_eq!(
            old.nodes
                .iter()
                .filter(|node| node.name == "target")
                .count(),
            3,
            "#22 lexical duplicate candidates are advisory, not semantic bindings"
        );
        match change {
            "body" => {
                fs::write(path.join("target.js"), "function target() { return 2; }\n").unwrap()
            }
            "add" => fs::write(path.join("added.js"), "function added() { return 1; }\n").unwrap(),
            "delete" => fs::remove_file(path.join("duplicate.js")).unwrap(),
            "rename" => fs::rename(path.join("duplicate.js"), path.join("moved.js")).unwrap(),
            "parse" => {
                fs::write(path.join("target.js"), "function target() { return ( ; }\n").unwrap()
            }
            "scope" => fs::write(
                path.join("target.js"),
                "function target() { { return 2; } }\n",
            )
            .unwrap(),
            "import" => fs::write(
                path.join("target.js"),
                "import { foo } from './missing.js';\nfunction target() { return 1; }\n",
            )
            .unwrap(),
            "export" => fs::write(
                path.join("target.js"),
                "export { target };\nfunction target() { return 1; }\n",
            )
            .unwrap(),
            "supertype" => fs::write(
                path.join("A.java"),
                "class A extends Other { int run(){ return 1; } }\nclass Base {}\n",
            )
            .unwrap(),
            "lookup" => fs::write(
                path.join("target.js"),
                "function target() { return unknown(); }\n",
            )
            .unwrap(),
            "ambiguous_lookup" => fs::write(
                path.join("target.js"),
                "function target() { return target(); }\n",
            )
            .unwrap(),
            _ => unreachable!(),
        }
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let expected = store.recovery_index_baseline().unwrap();
        assert_eq!(
            store
                .prepare_local_native(&capture, &expected, &cancel)
                .unwrap()
                .is_some(),
            local,
            "{label}: persisted authenticated classifier"
        );
        let pin = IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone())
            .unwrap()
            .run(&options, &cancel, |_| {})
            .unwrap();
        assert_eq!(
            store.graph_at(Some(first)).unwrap().nodes,
            old.nodes,
            "{label}: old pin preserved"
        );
        let cold_state = tempfile::tempdir().unwrap();
        let cold = Store::open_for_tests(cold_state.path(), path).unwrap();
        let cold_job = IndexJobCoordinator::prepare(&cold, None).unwrap();
        let _cold_session = cold_job.session();
        let cold_pin = cold_job.run(&options, &cancel, |_| {}).unwrap();
        let published = store.graph_at(Some(pin)).unwrap();
        let oracle = cold.graph_at(Some(cold_pin)).unwrap();
        assert_eq!(published.nodes, oracle.nodes, "{label}: cold node parity");
        assert_eq!(published.calls, oracle.calls, "{label}: cold call parity");
        assert_eq!(
            published.regions, oracle.regions,
            "{label}: cold region parity"
        );
    }
}

#[test]
#[ignore = "resource-gated exact canonical medium local-update writer counters"]
fn canonical_medium_local_update_writes_only_changed_fact_families() {
    use crate::{
        capture::Capture,
        index_coordinator::IndexJobCoordinator,
        indexer::{self, CapturedChange},
    };
    let generated = tempfile::tempdir().unwrap();
    let output = generated.path().join("canonical");
    let status = std::process::Command::new("node")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/synthetic-cohorts/generate.mjs"))
        .arg("--out")
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        fs::read(output.join("manifest.json")).unwrap(),
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/synthetic-cohorts/manifest-v1.json")
        )
        .unwrap()
    );
    let root = output.join("medium");
    let state = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = job.session();
    let first_capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let first = job.run(&options, &cancel, |_| {}).unwrap();
    let full = store.last_writer_counters().unwrap();
    let path = root.join("python/Cmedium0000.py");
    let original = fs::read_to_string(&path).unwrap();
    let marker = "def f0(): return ";
    let offset = original.find(marker).unwrap() + marker.len();
    let mut source = original.as_bytes().to_vec();
    assert!(source[offset].is_ascii_digit());
    source[offset] = if source[offset] == b'1' { b'2' } else { b'1' };
    fs::write(&path, source).unwrap();
    let updated_capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let mut visits = vec![];
    let staged = indexer::measure_captured_native_change(
        &first_capture,
        &updated_capture,
        &root,
        store.root_id(),
        &cancel,
        |key| visits.push(key.path.clone()),
    )
    .unwrap();
    assert!(
        matches!(staged.decision,CapturedChange::DocumentLocal{ref path}
            if path=="python/Cmedium0000.py")
    );
    assert_eq!(
        visits,
        ["python/Cmedium0000.py"],
        "selected native extraction visits exactly one document"
    );
    let baseline = store.recovery_index_baseline().unwrap();
    assert!(
        store
            .prepare_local_native(&updated_capture, &baseline, &cancel)
            .unwrap()
            .is_some(),
        "authenticated persisted head also selects one-document native assembly"
    );
    let second = IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone())
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    let local = store.last_writer_counters().unwrap();
    assert!(
        local.native.rows > 0 && local.native.rows < full.native.rows / 10,
        "selected native rows {local:?} vs full {full:?}"
    );
    assert!(local.graph.rows > 0 && local.graph.rows < full.graph.rows / 10);
    assert!(local.class.rows > 0 && local.class.rows < full.class.rows / 10);
    assert_eq!(local.reused_occurrence_reads, 0);
    assert_eq!(
        local.total.rows,
        local.native.rows + local.graph.rows + local.class.rows + local.manifest.rows
    );
    assert_eq!(
        local.total.bytes,
        local.native.bytes + local.graph.bytes + local.class.bytes + local.manifest.bytes
    );
    assert_eq!(
        store
            .source_at("python/Cmedium0000.py", Some(first))
            .unwrap()
            .unwrap()
            .1
            .text,
        original
    );
    let cold_state = tempfile::tempdir().unwrap();
    let cold = Store::open_for_tests(cold_state.path(), &root).unwrap();
    let cold_job = IndexJobCoordinator::prepare(&cold, None).unwrap();
    let _cold_session = cold_job.session();
    let cold_pin = cold_job.run(&options, &cancel, |_| {}).unwrap();
    let actual = store.graph_at(Some(second)).unwrap();
    let oracle = cold.graph_at(Some(cold_pin)).unwrap();
    assert_eq!(actual.nodes, oracle.nodes);
    assert_eq!(actual.calls, oracle.calls);
    assert_eq!(actual.regions, oracle.regions);
    eprintln!("canonical medium initial writer {full:?}; selected update writer {local:?}");
}

#[test]
#[ignore = "resource-gated canonical medium and large produced-index fact floors"]
fn canonical_cohort_produced_native_fact_floors() {
    use crate::index_coordinator::IndexJobCoordinator;
    let generated = tempfile::tempdir().unwrap();
    let output = generated.path().join("canonical");
    let generator =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/synthetic-cohorts/generate.mjs");
    let status = std::process::Command::new("node")
        .arg(generator)
        .arg("--out")
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success(), "canonical cohort generator failed");
    assert_eq!(
        fs::read(output.join("manifest.json")).unwrap(),
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/synthetic-cohorts/manifest-v1.json")
        )
        .unwrap(),
        "only exact pinned cohort bytes are eligible"
    );
    let cancel = Arc::new(AtomicBool::new(false));
    for size in ["medium", "large"] {
        // Explicit diagnostic only; the default proof always measures both pinned cohorts.
        if size == "large" && std::env::var_os("TRELLIS_ONLY_MEDIUM_DIAGNOSTIC").is_some() {
            break;
        }
        let state = tempfile::tempdir().unwrap();
        let root = output.join(size);
        let store = Store::open_for_tests(state.path(), &root).unwrap();
        let job = IndexJobCoordinator::prepare(&store, None).unwrap();
        let _session = job.session();
        let started = std::time::Instant::now();
        let pin = job
            .run_observed(
                &IndexOptions::new(root),
                &cancel,
                |_| {},
                |_| {
                    eprintln!(
                        "{size} prepublication capture/native/graph {:?}",
                        started.elapsed()
                    );
                },
            )
            .unwrap();
        eprintln!("{size} committed publisher {:?}", started.elapsed());
        let db = Connection::open(store.roots.index_db(&store.identity)).unwrap();
        let revision = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
        let mut counts = BTreeMap::new();
        for row in db.prepare("SELECT m.language,SUM(
                (SELECT count(*) FROM native_version_declarations d WHERE d.version_id=m.document_version_id)+
                (SELECT count(*) FROM native_version_calls c WHERE c.version_id=m.document_version_id)+
                (SELECT count(*) FROM native_version_control_regions r WHERE r.version_id=m.document_version_id))
                FROM revision_documents m WHERE m.revision_id=?1 GROUP BY m.language ORDER BY m.language")
                .unwrap().query_map([&revision],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))
                .unwrap() {
                let (language,facts)=row.unwrap();counts.insert(language,facts);
            }
        assert_eq!(counts.len(), 4, "{size}: count each produced language");
        if size == "medium" {
            for (language, facts) in &counts {
                assert!(
                    *facts >= 50_000,
                    "{size}/{language}: produced native facts {facts} below floor"
                );
            }
        } else {
            assert!(
                counts.values().sum::<i64>() >= 500_000,
                "{size}: produced native facts {counts:?} below overall floor"
            );
        }
        let measured = store.last_writer_counters().unwrap();
        assert!(
            measured.native.rows > 0
                && measured.graph.rows > 0
                && measured.class.rows > 0
                && measured.manifest.rows >= counts.len() as u64
        );
        assert_eq!(
            measured.total.rows,
            measured.native.rows
                + measured.graph.rows
                + measured.class.rows
                + measured.manifest.rows
        );
        assert_eq!(
            measured.total.bytes,
            measured.native.bytes
                + measured.graph.bytes
                + measured.class.bytes
                + measured.manifest.bytes
        );
        assert_eq!(measured.reused_occurrence_reads, 0);
        eprintln!("{size} produced facts {counts:?}; writer rows/bytes {measured:?}");
    }
}

#[test]
fn local_class_catalog_cap_uses_selected_full_revision_symbol_summary() {
    let limits = crate::classes::Limits::default();
    let mut local = Graph::default();
    // Materialized graph has only one changed document, but the trusted
    // selected-revision summary is above the unchanged 1M-symbol cap.
    local.stats.symbols = limits.symbols + 1;
    let actual = Store::compose_selected_class_catalog(&local, &BTreeMap::new(), limits).unwrap();
    let full = crate::classes::Catalog::compose(&[], 0, local.stats.symbols, limits).unwrap();
    let incorrect_changed_only =
        crate::classes::Catalog::compose(&[], 0, local.nodes.len(), limits).unwrap();
    assert_eq!(actual.warnings, full.warnings);
    assert!(actual.truncated && full.truncated);
    assert_ne!(actual.warnings, incorrect_changed_only.warnings);
    assert!(!incorrect_changed_only.truncated);
}

#[test]
fn local_writer_counters_exclude_unchanged_occurrence_access() {
    use crate::index_coordinator::IndexJobCoordinator;
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("local.js"),
        "function local() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        work.path().join("A.java"),
        "class A { int run() { return 1; } }\n",
    )
    .unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let first_job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = first_job.session();
    let first = first_job.run(&options, &cancel, |_| {}).unwrap();
    fs::write(
        work.path().join("local.js"),
        "function local() { return 2; }\n",
    )
    .unwrap();
    let second = IndexJobCoordinator::prepare_with_session(&store, Some(first), session)
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    assert_eq!(second.index_revision, first.index_revision + 1);
    let counted = store.last_writer_counters().unwrap();
    assert_eq!(
        counted.reused_occurrence_reads, 0,
        "unchanged occurrence must never be compared under BEGIN IMMEDIATE"
    );
    assert!(counted.native.rows > 0 && counted.graph.rows > 0 && counted.class.rows > 0);
    assert!(
        counted.manifest.rows >= 3,
        "full manifest plus header must be written"
    );
    assert_eq!(
        counted.total.rows,
        counted.manifest.rows + counted.native.rows + counted.graph.rows + counted.class.rows
    );
    assert_eq!(
        counted.total.bytes,
        counted.manifest.bytes + counted.native.bytes + counted.graph.bytes + counted.class.bytes
    );
}

#[test]
fn wal_header_without_sidecars_is_not_corruption_authority() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let index = store.roots.index_db(&store.identity);
    drop(store);
    let db = Connection::open(&index).unwrap();
    let mode: String = db
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let _: (i64, i64, i64) = db
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    drop(db);
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!index.with_file_name(format!("index.db{suffix}")).exists());
    }
    let before = fs::read(&index).unwrap();
    assert_eq!((before[18], before[19]), (2, 2));
    let error = verify_index_file(&index).unwrap_err();
    assert_eq!(recovery_class(&error), RecoveryClass::Hard);
    assert!(
        error.to_string().contains("incompatible_index"),
        "{error:#}"
    );
    assert!(Store::open_for_tests(state.path(), work.path()).is_err());
    assert_eq!(fs::read(&index).unwrap(), before);
}

#[test]
fn only_short_or_invalid_magic_headers_request_typed_recreation() {
    for bytes in [
        b"short".as_slice(),
        b"not-a-sqlite-header-with-at-least-20".as_slice(),
    ] {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        fs::write(work.path().join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), work.path()).unwrap();
        let index = store.roots.index_db(&store.identity);
        drop(store);
        fs::write(&index, bytes).unwrap();
        let error = verify_index_file(&index).unwrap_err();
        assert!(error.is::<ExceptionalIndexFormat>());
        assert_eq!(recovery_class(&error), RecoveryClass::RecreatePending);
        let recovering = Store::open_for_tests(state.path(), work.path()).unwrap();
        assert_eq!(
            recovering.disposition(),
            RecoveryDisposition::RecreatePending
        );
        let exclusive = recovering
            .roots
            .index_use_exclusive_existing(&recovering.identity)
            .unwrap();
        let mut leader = recovering
            .roots
            .writer_under_exclusive(recovering.identity.clone(), exclusive)
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let pin = recovering
            .recreate_index_exclusive(
                &IndexOptions::new(work.path().to_owned()),
                &mut leader,
                &cancel,
            )
            .unwrap();
        assert_eq!(pin.index_revision, 1);
        assert_eq!(recovering.status().unwrap().revision, pin);
    }
}

#[test]
fn exceptional_corrupt_recreate_refuses_busy_use_without_unlinking_index() {
    use std::os::unix::fs::MetadataExt;
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let path = store.roots.index_db(&store.identity);
    drop(store);
    fs::write(&path, b"short").unwrap();
    let inode = fs::metadata(&path).unwrap().ino();
    let recovering = Store::open_for_tests(state.path(), work.path()).unwrap();
    assert_eq!(
        recovering.disposition(),
        RecoveryDisposition::RecreatePending
    );
    let busy = recovering
        .roots
        .index_use_existing_readonly(&recovering.identity)
        .unwrap();
    let result = recovering.recreate_pending_writer_session(
        &IndexOptions::new(work.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
    );
    assert!(
        result.is_err(),
        "protected SH user must block exceptional EX recreation"
    );
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(fs::read(&path).unwrap(), b"short");
    drop(busy);
}

#[test]
fn obsolete_metadata_marker_recreates_new_file_and_preserves_requests() {
    use std::os::unix::fs::MetadataExt;
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("a.js"), "function a() {}\n").unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let owner = store.writer().unwrap();
    let original = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &owner,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(owner);
    let request = store.enqueue_request(&options, Some(original)).unwrap();
    let index = store.roots.index_db(&store.identity);
    drop(store);
    let db = Connection::open(&index).unwrap();
    db.execute_batch(
        "PRAGMA ignore_check_constraints=ON;
            UPDATE index_metadata SET schema_version=7;",
    )
    .unwrap();
    drop(db);
    let inode = fs::metadata(&index).unwrap().ino();
    let recovering = Store::open_for_tests(state.path(), work.path()).unwrap();
    assert_eq!(
        recovering.disposition(),
        RecoveryDisposition::RecreatePending
    );
    assert!(recovering.status().is_err());
    let (pin, session) = recovering
        .recreate_pending_writer_session(&options, &cancel)
        .unwrap();
    assert_ne!(
        fs::metadata(&index).unwrap().ino(),
        inode,
        "obsolete index must recreate as a new file"
    );
    assert_eq!(recovering.status().unwrap().revision, pin);
    assert_eq!(pin.index_revision, 1);
    assert_ne!(pin.index_generation, original.index_generation);
    let queued = recovering.request_by_id(&request.id).unwrap().unwrap();
    assert_eq!(queued.expected, Some(original));
    assert_eq!(queued.state, "queued");
    assert!(
        recovering
            .graph_at(Some(original))
            .unwrap_err()
            .to_string()
            .contains("revision conflict")
    );
    drop(session);
}

#[test]
fn private_stage_uses_paired_writer_without_admitting_corrupt_live_evidence() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("a.js"),
        "function changed() { return 1; }\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let owner = store.writer().unwrap();
    let previous = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &owner,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    assert_eq!(previous.index_revision, 1);
    let live_path = store.roots.index_db(&store.identity);
    drop(owner);
    drop(store);
    fs::write(&live_path, b"corrupt index, not sqlite").unwrap();
    let original = fs::read(&live_path).unwrap();
    let recovering = Store::open_for_tests(state.path(), work.path()).unwrap();
    assert_eq!(
        recovering.disposition(),
        RecoveryDisposition::RecreatePending
    );
    let clone = recovering.clone();
    let exclusive = recovering
        .roots
        .index_use_exclusive_existing(&recovering.identity)
        .unwrap();
    let leader = recovering
        .roots
        .writer_under_exclusive(recovering.identity.clone(), exclusive)
        .unwrap();
    let stage = recovering.create_staged_index(&leader, true).unwrap();
    let (graph, native, capture) =
        index_workspace_bundle(&options, recovering.root_id(), &cancel, |_| {}).unwrap();
    let replacement = recovering
        .publish_native_to_stage(
            (&graph, &capture, &native),
            &stage,
            &leader,
            &cancel,
            |_, _| Ok(()),
        )
        .unwrap();
    assert_eq!(replacement.index_revision, previous.index_revision);
    assert_ne!(replacement.index_generation, previous.index_generation);
    let db = open_index(&stage.path, false).unwrap();
    let stage_status = recovering.decode_control_status_raw(&db).unwrap();
    assert_eq!(stage_status.revision, replacement);
    validate_paired_metadata(&db, recovering.root_id(), PairedManifestScope::Full).unwrap();
    validate_paired_rows(&db).unwrap();
    validate_reconcile_inventory(&db).unwrap();
    let integrity: String = db
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    drop(db);
    assert_eq!(fs::read(&live_path).unwrap(), original);
    assert_eq!(
        recovering.disposition(),
        RecoveryDisposition::RecreatePending
    );
    assert!(clone.recovery_required.load(Ordering::Acquire));
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("recovery_required")
    );

    let failing = recovering.create_staged_index(&leader, true).unwrap();
    let error = recovering
        .publish_native_to_stage(
            (&graph, &capture, &native),
            &failing,
            &leader,
            &cancel,
            |phase, _| {
                if phase == PublishStage::BeforeCommit {
                    anyhow::bail!("injected stage precommit refusal");
                }
                Ok(())
            },
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected stage precommit refusal")
    );
    let unchanged = open_index(&failing.path, false).unwrap();
    let baseline = recovering.recovery_baseline(&unchanged).unwrap();
    assert_eq!(baseline.pin().unwrap().index_revision, 0);
    assert!(
        baseline.compatible,
        "private stage remains an empty v8 bootstrap"
    );
    validate_v8_bootstrap(&unchanged).unwrap();
    assert_eq!(
        unchanged
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        DATABASE_SCHEMA_VERSION
    );
    drop(unchanged);
    assert_eq!(fs::read(&live_path).unwrap(), original);
    assert_eq!(
        recovering.disposition(),
        RecoveryDisposition::RecreatePending
    );
    assert!(clone.recovery_required.load(Ordering::Acquire));
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("recovery_required")
    );
    recovering
        .cleanup_failed_staged_index(failing, Some(&leader))
        .unwrap();
    recovering
        .cleanup_failed_staged_index(stage, Some(&leader))
        .unwrap();
}

#[test]
fn staged_hot_journal_after_validation_refuses_without_deleting_sidecar() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let index = store.roots.index_db(&store.identity);
    let dir = store.roots.index_dir(&store.identity);
    drop(store);
    fs::write(&index, b"short").unwrap();
    let before = fs::read(&index).unwrap();
    let recovering = Store::open_for_tests(state.path(), work.path()).unwrap();
    let exclusive = recovering
        .roots
        .index_use_exclusive_existing(&recovering.identity)
        .unwrap();
    let mut leader = recovering
        .roots
        .writer_under_exclusive(recovering.identity.clone(), exclusive)
        .unwrap();
    let mut journal = None;
    let cancel = Arc::new(AtomicBool::new(false));
    let error = recovering
        .recreate_index_exclusive_with_hook(
            &IndexOptions::new(work.path().to_owned()),
            &mut leader,
            &cancel,
            |phase| {
                if phase == ActivationStage::BeforeRename {
                    let stage = fs::read_dir(&dir)?
                        .map(|entry| entry.map(|entry| entry.path()))
                        .collect::<std::io::Result<Vec<_>>>()?
                        .into_iter()
                        .find(|path| {
                            path.file_name()
                                .and_then(|name| name.to_str())
                                .is_some_and(|name| name.starts_with("index.db.tmp-"))
                        })
                        .context("missing private stage at activation hook")?;
                    let sidecar = stage.with_file_name(format!(
                        "{}-journal",
                        stage.file_name().unwrap().to_string_lossy()
                    ));
                    fs::write(&sidecar, b"hot staged journal")?;
                    journal = Some(sidecar);
                }
                Ok(())
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("recovery_required"), "{error:#}");
    assert_eq!(fs::read(&index).unwrap(), before);
    assert_eq!(fs::read(journal.unwrap()).unwrap(), b"hot staged journal");
    assert_eq!(
        recovering.disposition(),
        RecoveryDisposition::RecreatePending
    );
    leader
        .verify_exclusive_use(&recovering.roots.index_use_lock(&recovering.identity))
        .unwrap();
}

#[test]
fn exceptional_activation_is_atomic_and_keeps_pending_on_ambiguous_failure() {
    use std::os::unix::fs::PermissionsExt;
    for fault in [
        Some(ActivationStage::BeforeRename),
        Some(ActivationStage::AfterJournalBackupBeforeDirFsync),
        Some(ActivationStage::AfterRenameBeforeDirFsync),
        None,
    ] {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        fs::write(work.path().join("a.js"), "function a() { return 1; }\n").unwrap();
        let store = Store::open_for_tests(state.path(), work.path()).unwrap();
        let options = IndexOptions::new(work.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let (graph, native, capture) =
            index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
        let original_owner = store.writer().unwrap();
        let old_pin = store
            .publish_native(
                &graph,
                &capture,
                &native,
                &original_owner,
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        store
            .put_annotation(&Annotation {
                id: "saved".into(),
                node_id: "unattached".into(),
                body: "keep me".into(),
            })
            .unwrap();
        let index = store.roots.index_db(&store.identity);
        let dir = store.roots.index_dir(&store.identity);
        let record = store.roots.record_db(&store.identity);
        let requests = dir.join("requests.db");
        let facts = dir.join("facts.db");
        let neighbor = dir.join("unrelated-record");
        fs::write(&requests, b"retained requests").unwrap();
        fs::write(&facts, b"retained facts").unwrap();
        fs::write(&neighbor, b"unrelated").unwrap();
        let durable_bytes = fs::read(&record).unwrap();
        drop(original_owner);
        drop(store);
        fs::write(&index, b"corrupt-index-header").unwrap();
        let old_bytes = fs::read(&index).unwrap();
        let recovering = Store::open_for_tests(state.path(), work.path()).unwrap();
        assert_eq!(
            recovering.disposition(),
            RecoveryDisposition::RecreatePending
        );
        let shared = recovering
            .roots
            .index_use_existing(&recovering.identity)
            .unwrap();
        assert!(
            recovering
                .roots
                .index_use_exclusive_existing(&recovering.identity)
                .is_err()
        );
        drop(shared);
        let exclusive = recovering
            .roots
            .index_use_exclusive_existing(&recovering.identity)
            .unwrap();
        let mut leader = recovering
            .roots
            .writer_under_exclusive(recovering.identity.clone(), exclusive)
            .unwrap();
        if fault.is_none() || fault == Some(ActivationStage::AfterJournalBackupBeforeDirFsync) {
            let journal = index.with_file_name("index.db-journal");
            fs::write(&journal, b"old journal bytes").unwrap();
            fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let result = recovering.recreate_index_exclusive_with_hook(
            &options,
            &mut leader,
            &cancel,
            |phase| {
                if Some(phase) == fault {
                    anyhow::bail!("injected activation boundary");
                }
                Ok(())
            },
        );
        match fault {
            Some(ActivationStage::BeforeRename) => {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("injected activation boundary")
                );
                assert_eq!(fs::read(&index).unwrap(), old_bytes);
                let staged = fs::read_dir(&dir)
                    .unwrap()
                    .filter_map(|entry| {
                        let name = entry.unwrap().file_name().into_string().ok()?;
                        name.starts_with("index.db.tmp-").then_some(name)
                    })
                    .count();
                assert_eq!(staged, 0);
                let wal = index.with_file_name("index.db-wal");
                fs::write(&wal, b"unsafe sidecar").unwrap();
                assert!(
                    recovering
                        .recreate_index_exclusive(&options, &mut leader, &cancel)
                        .is_err()
                );
                assert_eq!(fs::read(&wal).unwrap(), b"unsafe sidecar");
                assert_eq!(fs::read(&index).unwrap(), old_bytes);
            }
            Some(ActivationStage::AfterJournalBackupBeforeDirFsync) => {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("injected activation boundary")
                );
                assert_eq!(fs::read(&index).unwrap(), old_bytes);
                assert_eq!(
                    fs::read(index.with_file_name("index.db-journal")).unwrap(),
                    b"old journal bytes"
                );
                assert!(!fs::read_dir(&dir).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("index.db-journal.tmp-")
                }));
            }
            Some(ActivationStage::AfterRenameBeforeDirFsync) => {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("injected activation boundary")
                );
                assert_ne!(fs::read(&index).unwrap(), old_bytes);
                assert!(
                    leader
                        .verify_exclusive_use(
                            &recovering.roots.index_use_lock(&recovering.identity)
                        )
                        .is_ok()
                );
            }
            None => {
                let pin = result.unwrap();
                assert_eq!(pin.index_revision, old_pin.index_revision);
                assert_ne!(pin.index_generation, old_pin.index_generation);
                assert_eq!(recovering.status().unwrap().revision, pin);
                assert!(
                    recovering
                        .source_at("a.js", Some(old_pin))
                        .unwrap_err()
                        .to_string()
                        .contains("revision conflict")
                );
                assert!(!index.with_file_name("index.db-journal").exists());
                assert!(
                    leader
                        .verify_exclusive_use(
                            &recovering.roots.index_use_lock(&recovering.identity)
                        )
                        .is_err()
                );
            }
        }
        assert_eq!(fs::read(&requests).unwrap(), b"retained requests");
        assert_eq!(fs::read(&facts).unwrap(), b"retained facts");
        assert_eq!(fs::read(&neighbor).unwrap(), b"unrelated");
        assert_eq!(fs::read(&record).unwrap(), durable_bytes);
        if fault.is_some() {
            assert_eq!(
                recovering.disposition(),
                RecoveryDisposition::RecreatePending
            );
            assert!(
                recovering
                    .status()
                    .unwrap_err()
                    .to_string()
                    .contains("recovery_required")
            );
        }
    }
}

#[test]
fn cancellation_at_before_commit_rolls_back_entire_native_class_graph_pair() {
    use crate::class_diagram::ClassDiagramRequest;
    use std::{
        sync::{atomic::Ordering, mpsc},
        thread::JoinHandle,
        time::Duration,
    };

    // The guard cancels, releases the callback, and joins on every early
    // failure, including a timeout or an assertion panic on the main thread.
    struct PublisherGuard {
        cancel: CancelFlag,
        release: Option<mpsc::Sender<()>>,
        worker: Option<JoinHandle<Result<IndexPin>>>,
    }
    impl Drop for PublisherGuard {
        fn drop(&mut self) {
            self.cancel.store(true, Ordering::Release);
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
    fn worker_outcome(result: std::thread::Result<Result<IndexPin>>) -> String {
        match result {
            Ok(Ok(pin)) => format!("unexpected publisher success: {pin:?}"),
            Ok(Err(error)) => format!("publisher error: {error:#}"),
            Err(panic) => format!(
                "publisher panic: {}",
                panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("non-string panic payload")
            ),
        }
    }

    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join("Types.java");
    let original = "class A { void go() { helper(); } void helper() {} }\nclass B {}\n";
    fs::write(&path, original).unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let initial_cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &initial_cancel, |_| {}).unwrap();
    let session = store.writer_session().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.writer_guard().unwrap(),
            store.index_baseline().unwrap(),
            &initial_cancel,
        )
        .unwrap();
    let seed = graph
        .nodes
        .iter()
        .find(|node| node.name == "A" && node.kind == SymbolKind::Class)
        .unwrap()
        .id
        .clone();
    let question = ClassDiagramRequest {
        seed,
        expected_revision: pin,
        expanded: vec![],
        include_unmatched: false,
        include_hierarchy: false,
    };
    let old_graph = store.graph().unwrap();
    let old_diagram = serde_json::to_value(store.class_diagram_at(&question).unwrap()).unwrap();
    let old_key = native
        .revision
        .documents
        .iter()
        .find(|doc| doc.key.path == "Types.java")
        .unwrap()
        .key
        .clone();
    let old_source = store.source_at("Types.java", Some(pin)).unwrap().unwrap();
    let old_native_source = store.native_source_at(pin, &old_key).unwrap().unwrap();
    let old_coverage = store.native_coverage_at(pin, &old_key).unwrap().unwrap();
    let old_declarations = store.native_declarations_at(pin, "java", "go").unwrap();
    assert!(!old_declarations.is_empty());
    let go_owner = &old_declarations[0].syntax_id;
    let old_calls = store.native_calls_at(pin, go_owner).unwrap();
    assert!(
        !old_calls.is_empty(),
        "real Java call is paired native evidence"
    );
    let old_regions = store.native_control_regions_at(pin, go_owner).unwrap();

    let changed = format!("{original}class Added {{ void added() {{}} }}\n");
    fs::write(&path, &changed).unwrap();
    let (next, next_native, next_capture) =
        index_workspace_bundle(&options, store.root_id(), &initial_cancel, |_| {}).unwrap();
    assert!(
        next.nodes
            .iter()
            .any(|node| node.name == "Added" && node.kind == SymbolKind::Class)
    );
    // The private seam skips the public publish_native wrapper: retain each
    // wrapper admission check on this genuine immutable captured bundle.
    let canonical_work = fs::canonicalize(work.path()).unwrap();
    next_native
        .validate(
            &next_capture,
            &canonical_work,
            store.root_id(),
            &initial_cancel,
        )
        .unwrap();
    assert!(
        next_capture.graph_projection_count() == 1,
        "capture must contain exactly one graph projection"
    );
    assert!(
        next_capture.source_operations.len() == next_capture.files.len()
            && next_capture
                .source_operations
                .values()
                .all(|counts| counts.opens == 1
                    && counts.complete_reads == 1
                    && counts.hashes == 1),
        "each captured source must open/read/hash exactly once"
    );
    crate::indexer::validate_native_graph(&next, &next_capture, &next_native, &initial_cancel)
        .unwrap();

    // The leader was acquired before r1; snapshot after all leader activity.
    let baseline_db = fs::read(store.roots.index_db(&store.identity)).unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel::<()>(1);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let worker_store = store.clone();
    let worker_session = session.clone();
    let worker = std::thread::spawn(move || {
        worker_store.publish_inner_checked(
                (&next, &next_capture, &next_native),
                worker_session.writer_guard().unwrap(),
                pin,
                &worker_cancel,
                |stage, tx| {
                    if stage != PublishStage::BeforeCommit {
                        return Ok(());
                    }
                    // BeforeCommit runs after graph/native/class inserts and
                    // paired validation, before the final cancellation check.
                    let new_revision: i64 = tx.query_row(
                        "SELECT index_revision FROM index_metadata WHERE singleton=1",
                        [],
                        |row| row.get(0),
                    )?;
                    ensure!(
                        new_revision == i64::try_from(pin.index_revision + 1)?,
                        "new revision not staged inside writer transaction"
                    );
                    let staged_pin = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision + 1);
                    let added: i64 = tx.query_row(
                        "SELECT count(*) FROM classes c JOIN revision_documents m ON m.class_projection_id=c.projection_id WHERE m.revision_id=?1 AND c.path='Types.java' AND c.name='Added'",
                        [&staged_pin],
                        |row| row.get(0),
                    )?;
                    ensure!(added == 1, "new class projection not staged");
                    let raw: Vec<u8> = tx.query_row(
                        "SELECT v.source_bytes FROM document_versions v JOIN revision_documents m ON m.document_version_id=v.id WHERE m.revision_id=?1 AND m.path='Types.java' AND v.path='Types.java'",
                        [&staged_pin],
                        |row| row.get(0),
                    )?;
                    ensure!(raw == changed.as_bytes(), "new native source not staged");
                    validate_paired_metadata(tx, worker_store.root_id(), PairedManifestScope::Full)?;
                    validate_paired_rows(tx)?;
                    entered_tx
                        .send(())
                        .context("failed to signal BeforeCommit stage")?;
                    release_rx
                        .recv_timeout(crate::testing::HANG_BACKSTOP)
                        .context("bounded BeforeCommit release timed out")?;
                    Ok(())
                },
            )
    });
    let mut guard = PublisherGuard {
        cancel,
        release: Some(release_tx),
        worker: Some(worker),
    };
    if let Err(wait) = entered_rx.recv_timeout(crate::testing::HANG_BACKSTOP) {
        let detail = if guard.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            worker_outcome(guard.worker.take().unwrap().join())
        } else {
            format!("publisher still before BeforeCommit stage after bounded wait: {wait}")
        };
        panic!("publisher did not enter the staged writer transaction: {detail}");
    }
    // The worker is blocked inside its own BeforeCommit callback. Probe on
    // a second connection, not on the publisher's Connection reference.
    let probe = Connection::open(store.roots.index_db(&store.identity)).unwrap();
    probe.busy_timeout(Duration::ZERO).unwrap();
    match probe.execute_batch("BEGIN IMMEDIATE") {
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::DatabaseBusy => {}
        Ok(()) => {
            probe.execute_batch("ROLLBACK").unwrap();
            panic!("independent writer obtained lock while publisher paused BeforeCommit");
        }
        Err(error) => panic!("independent writer probe failed unexpectedly: {error}"),
    }
    guard.cancel.store(true, Ordering::Release);
    guard.release.take().unwrap().send(()).unwrap();
    let outcome = guard.worker.take().unwrap().join();
    match outcome {
        Ok(Err(error)) if error.to_string().contains("cancelled") => {}
        other => panic!(
            "publisher failed cancellation contract: {}",
            worker_outcome(other)
        ),
    }
    assert_eq!(
        fs::read(store.roots.index_db(&store.identity)).unwrap(),
        baseline_db,
        "cancelled staged r2 must leave every retained r1 SQLite byte unchanged"
    );
    assert_eq!(store.status().unwrap().revision, pin);
    assert_eq!(store.graph().unwrap(), old_graph);
    assert_eq!(
        serde_json::to_value(store.class_diagram_at(&question).unwrap()).unwrap(),
        old_diagram
    );
    assert_eq!(
        store.source_at("Types.java", Some(pin)).unwrap().unwrap(),
        old_source
    );
    assert_eq!(
        store.native_source_at(pin, &old_key).unwrap().unwrap(),
        old_native_source
    );
    assert_eq!(
        store.native_coverage_at(pin, &old_key).unwrap().unwrap(),
        old_coverage
    );
    assert_eq!(
        store.native_declarations_at(pin, "java", "go").unwrap(),
        old_declarations
    );
    assert_eq!(store.native_calls_at(pin, go_owner).unwrap(), old_calls);
    assert_eq!(
        store.native_control_regions_at(pin, go_owner).unwrap(),
        old_regions
    );
}

#[test]
fn large_real_native_pair_cancels_at_before_commit_without_partial_rows() {
    use std::{
        sync::{atomic::Ordering, mpsc},
        thread::JoinHandle,
        time::Duration,
    };
    // Never leave a live writer or paused callback behind on timeout,
    // assertion failure, unexpected worker error, or worker panic.
    struct PublisherGuard {
        cancel: CancelFlag,
        release: Option<mpsc::Sender<()>>,
        worker: Option<JoinHandle<Result<IndexPin>>>,
    }
    impl Drop for PublisherGuard {
        fn drop(&mut self) {
            self.cancel.store(true, Ordering::Release);
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
    fn outcome(result: std::thread::Result<Result<IndexPin>>) -> String {
        match result {
            Ok(Ok(pin)) => format!("unexpected publisher success: {pin:?}"),
            Ok(Err(error)) => format!("publisher error: {error:#}"),
            Err(panic) => format!(
                "publisher panic: {}",
                panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("non-string panic payload")
            ),
        }
    }

    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join("a.js");
    let original = "function a() { b(); c(); }\nfunction b() { c(); }\nfunction c() { a(); }\n";
    fs::write(&path, original).unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let initial_cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &initial_cancel, |_| {}).unwrap();
    let session = store.writer_session().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.writer_guard().unwrap(),
            store.index_baseline().unwrap(),
            &initial_cancel,
        )
        .unwrap();
    let old_graph = store.graph().unwrap();
    let old_source = store.source_at("a.js", Some(pin)).unwrap().unwrap();
    let old_key = native
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == "a.js")
        .unwrap()
        .key
        .clone();
    let old_native_source = store.native_source_at(pin, &old_key).unwrap().unwrap();
    let old_coverage = store.native_coverage_at(pin, &old_key).unwrap().unwrap();
    let old_declarations = store
        .native_declarations_at(pin, "javascript", "a")
        .unwrap();
    assert!(!old_declarations.is_empty());
    let old_owner = &old_declarations[0].syntax_id;
    let old_calls = store.native_calls_at(pin, old_owner).unwrap();
    assert!(
        !old_calls.is_empty(),
        "old JavaScript function has measured native calls"
    );
    let old_regions = store.native_control_regions_at(pin, old_owner).unwrap();

    // Preserve the old test's 5,000 genuine JavaScript function definitions
    // and call sites rather than replacing them with synthetic graph rows.
    let large_source = (0..5_000)
        .map(|i| {
            format!(
                "function node_{i:05}() {{ node_{:05}(); }}\n",
                (i + 1) % 5_000
            )
        })
        .collect::<String>();
    fs::write(&path, &large_source).unwrap();
    let (next, next_native, next_capture) =
        index_workspace_bundle(&options, store.root_id(), &initial_cancel, |_| {}).unwrap();
    assert_eq!(
        next.nodes
            .iter()
            .filter(|n| n.kind == SymbolKind::Function)
            .count(),
        5_000
    );
    assert_eq!(next.calls.len(), 5_000);
    assert_eq!(
        next_native
            .declarations
            .iter()
            .filter(|d| d.kind == "function")
            .count(),
        5_000
    );
    assert_eq!(next_native.calls.len(), 5_000);
    // The private publish seam skips the public wrapper. Apply each native,
    // captured-source and graph parity check explicitly before the worker.
    let canonical_work = fs::canonicalize(work.path()).unwrap();
    next_native
        .validate(
            &next_capture,
            &canonical_work,
            store.root_id(),
            &initial_cancel,
        )
        .unwrap();
    assert_eq!(next_capture.graph_projection_count(), 1);
    assert_eq!(
        next_capture.source_operations.len(),
        next_capture.files.len()
    );
    assert!(
        next_capture
            .source_operations
            .values()
            .all(|operations| operations.opens == 1
                && operations.complete_reads == 1
                && operations.hashes == 1)
    );
    crate::indexer::validate_native_graph(&next, &next_capture, &next_native, &initial_cancel)
        .unwrap();

    // The baseline is captured after the sole leader acquisition and r1 commit.
    let baseline_db = fs::read(store.roots.index_db(&store.identity)).unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel::<()>(1);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let worker_store = store.clone();
    let worker_session = session.clone();
    let worker = std::thread::spawn(move || {
        worker_store.publish_inner_checked(
                (&next,&next_capture,&next_native),worker_session.writer_guard().unwrap(),pin,&worker_cancel,
                |stage,tx|{
                    if stage!=PublishStage::BeforeCommit {return Ok(());}
                    // All graph/native/class inserts and paired checks precede
                    // this callback; the final cancellation check follows it.
                    let revision:i64=tx.query_row(
                        "SELECT index_revision FROM index_metadata WHERE singleton=1",[],|r|r.get(0))?;
                    ensure!(revision==i64::try_from(pin.index_revision+1)?,
                        "new revision not staged in publisher transaction");
                    let staged_pin = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision + 1);
                    let graph_functions:i64=tx.query_row(
                        "SELECT count(*) FROM graph_nodes n JOIN revision_documents m ON m.graph_projection_id=n.projection_id WHERE m.revision_id=?1 AND m.path='a.js' AND n.kind='function'",[&staged_pin],|r|r.get(0))?;
                    let native_functions:i64=tx.query_row(
                        "SELECT count(*) FROM native_version_declarations d JOIN document_versions v ON v.id=d.version_id JOIN revision_documents m ON m.document_version_id=v.id WHERE m.revision_id=?1 AND m.path='a.js' AND v.path='a.js' AND d.kind='function'",[&staged_pin],|r|r.get(0))?;
                    let native_calls:i64=tx.query_row(
                        "SELECT count(*) FROM native_version_calls c JOIN document_versions v ON v.id=c.version_id JOIN revision_documents m ON m.document_version_id=v.id WHERE m.revision_id=?1 AND m.path='a.js' AND v.path='a.js'",[&staged_pin],|r|r.get(0))?;
                    ensure!((graph_functions,native_functions,native_calls)==(5_000,5_000,5_000),
                        "5,000 measured graph/native function and call rows not staged for active pin: graph={graph_functions} native={native_functions}/{native_calls}");
                    let source_bytes:Vec<u8>=tx.query_row(
                        "SELECT v.source_bytes FROM document_versions v JOIN revision_documents m ON m.document_version_id=v.id WHERE m.revision_id=?1 AND m.path='a.js' AND v.path='a.js'",[&staged_pin],|r|r.get(0))?;
                    ensure!(source_bytes==large_source.as_bytes(),
                        "new captured native source bytes not staged");
                    validate_paired_metadata(tx,worker_store.root_id(),PairedManifestScope::Full)?;
                    validate_paired_rows(tx)?;
                    entered_tx.send(()).context("cannot signal staged BeforeCommit")?;
                    release_rx.recv_timeout(crate::testing::HANG_BACKSTOP)
                        .context("bounded BeforeCommit release timed out")?;
                    Ok(())
                },
            )
    });
    let mut guard = PublisherGuard {
        cancel,
        release: Some(release_tx),
        worker: Some(worker),
    };
    // This generous bounded wait is only failure diagnosis/cleanup for the
    // real 5,000-function pre-lock work. It is NOT lock observation.
    if let Err(wait) = entered_rx.recv_timeout(Duration::from_secs(180)) {
        let detail = if guard.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            outcome(guard.worker.take().unwrap().join())
        } else {
            format!("large-source publisher still before BeforeCommit: {wait}")
        };
        panic!("publisher never reached its staged transaction: {detail}");
    }
    let probe = Connection::open(store.roots.index_db(&store.identity)).unwrap();
    probe.busy_timeout(Duration::ZERO).unwrap();
    match probe.execute_batch("BEGIN IMMEDIATE") {
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::DatabaseBusy => {}
        Ok(()) => {
            probe.execute_batch("ROLLBACK").unwrap();
            panic!("independent writer acquired lock while publisher paused BeforeCommit");
        }
        Err(error) => panic!("independent SQLite writer probe failed: {error}"),
    }
    guard.cancel.store(true, Ordering::Release);
    guard.release.take().unwrap().send(()).unwrap();
    match guard.worker.take().unwrap().join() {
        Ok(Err(error)) if error.to_string().contains("cancelled") => {}
        other => panic!(
            "large paired publisher failed cancellation contract: {}",
            outcome(other)
        ),
    }
    assert_eq!(
        fs::read(store.roots.index_db(&store.identity)).unwrap(),
        baseline_db,
        "cancelled staged 5,000-row r2 must leave every retained r1 SQLite byte unchanged"
    );
    assert_eq!(store.status().unwrap().revision, pin);
    assert_eq!(store.graph().unwrap(), old_graph);
    assert_eq!(
        store.source_at("a.js", Some(pin)).unwrap().unwrap(),
        old_source
    );
    assert_eq!(
        store.native_source_at(pin, &old_key).unwrap().unwrap(),
        old_native_source
    );
    assert_eq!(
        store.native_coverage_at(pin, &old_key).unwrap().unwrap(),
        old_coverage
    );
    assert_eq!(
        store
            .native_declarations_at(pin, "javascript", "a")
            .unwrap(),
        old_declarations
    );
    assert_eq!(store.native_calls_at(pin, old_owner).unwrap(), old_calls);
    assert_eq!(
        store.native_control_regions_at(pin, old_owner).unwrap(),
        old_regions
    );
}

#[test]
fn captured_long_call_exceeds_injected_small_graph_cap_and_remains_selectable() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let source = format!("function hello() {{ obj.{}(); }}\n", "a".repeat(48 * 1024));
    fs::write(work.path().join("flow.js"), &source).unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(work.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    assert!(!graph.calls.is_empty(), "test must exercise long call JSON");
    let leader = store.writer().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let call_bytes = graph
        .calls
        .iter()
        .map(|call| json(call).unwrap().len())
        .max()
        .unwrap();
    assert!(
        call_bytes > 32 * 1024,
        "genuine call JSON exceeds an injected 32KiB row cap"
    );
    assert!(
        !store
            .native_declarations_at(pin, "javascript", "hello")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .source_at("flow.js", Some(pin))
            .unwrap()
            .unwrap()
            .1
            .text,
        source
    );
}

#[test]
fn one_byte_over_encoded_source_cap_never_enters_paired_cas() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let source_path = work.path().join("flow.js");
    fs::write(&source_path, "function old() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(work.path().to_owned());
    let (old_graph, old_native, old_capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let leader = store.writer().unwrap();
    let pin = store
        .publish_native(
            &old_graph,
            &old_capture,
            &old_native,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let db_path = store.roots.index_db(&store.identity);
    let before = fs::read(&db_path).unwrap();
    // Raw capture stays far below the injected budget; sixfold JSON
    // escaping puts its actual canonical stored payload one byte over.
    let source = format!("/*{}*/", "\u{0001}".repeat(5450));
    fs::write(&source_path, &source).unwrap();
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let encoded = json(&graph.files[0]).unwrap();
    assert!(encoded.len() > 32 * 1024 && encoded.len() < 36 * 1024);
    let injected_cap = encoded.len() - 1;
    assert!(source.len() < injected_cap);
    let error = store
        .publish_inner_checked_with_source_cap(
            (&graph, &capture, &native),
            &leader,
            pin,
            &cancel,
            injected_cap,
            |_, _| panic!("encoded source must reject before opening the transaction"),
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("selected source byte budget exceeded before publication"),
        "{error:#}"
    );
    assert_eq!(
        fs::read(&db_path).unwrap(),
        before,
        "pre-CAS budget refusal must preserve the exact prior SQLite bytes"
    );
    assert_eq!(store.status().unwrap().revision, pin);
    assert_eq!(
        store
            .source_at("flow.js", Some(pin))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function old() {}\n"
    );
}

#[test]
fn captured_sixfold_escaped_source_is_selectable_below_explicit_json_ceiling() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let source = format!("/*{}*/", "\u{0001}".repeat(64 * 1024));
    fs::write(work.path().join("flow.js"), &source).unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(work.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let leader = store.writer().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let db = Connection::open(store.roots.index_db(&store.identity)).unwrap();
    let json_bytes: i64 = db
        .query_row(
            "SELECT length(source_bytes) FROM document_versions WHERE path='flow.js'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        json_bytes == source.len() as i64,
        "v8 source bytes must be stored without JSON expansion"
    );
    assert_eq!(
        store
            .source_at("flow.js", Some(pin))
            .unwrap()
            .unwrap()
            .1
            .text,
        source
    );
}

#[test]
fn real_source_above_injected_ancillary_cap_keeps_selected_source_available() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let source = format!("/*{}*/", "a".repeat(64 * 1024));
    fs::write(work.path().join("flow.js"), &source).unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(work.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let leader = store.writer().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let db = Connection::open(store.roots.index_db(&store.identity)).unwrap();
    let (version_id,class_id):(String,String)=db.query_row(
            "SELECT document_version_id,class_projection_id FROM revision_documents WHERE path='flow.js'",[],
            |r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    let (max_ancillary, total_ancillary) =
        Store::selected_ancillary_byte_usage(&db, &version_id, &class_id).unwrap();
    let source_json_bytes: i64 = db
        .query_row(
            "SELECT length(source_bytes) FROM document_versions WHERE path='flow.js'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // Small injected 32KiB ancillary cap models a captured source above a
    // narrower ancillary limit without a memory-heavy 33MiB fixture.
    assert!(
        source_json_bytes > 32 * 1024 && max_ancillary < 32 * 1024 && total_ancillary < 32 * 1024
    );
    assert!(
        Store::selected_source_row_bounded(&db, "flow.js", 32 * 1024).is_err(),
        "an explicit narrower source API cap must fail closed"
    );
    assert_eq!(
        store
            .source_at("flow.js", Some(pin))
            .unwrap()
            .unwrap()
            .1
            .text,
        source
    );
}
