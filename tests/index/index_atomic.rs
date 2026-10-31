use std::{fs, os::unix::fs::PermissionsExt, path::Path, sync::mpsc};
use tempfile::TempDir;
use trellis::store::Store;

fn fixture() -> (TempDir, TempDir) {
    let state = tempfile::tempdir().unwrap();
    fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).unwrap();
    (state, tempfile::tempdir().unwrap())
}
fn projection_fixture() -> (
    TempDir,
    TempDir,
    Store,
    trellis::model::IndexPin,
    std::sync::Arc<trellis::store::topology::WriterSession>,
) {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{index_coordinator::IndexJobCoordinator, indexer::IndexOptions};
    let (state, workspace) = fixture();
    fs::write(
        workspace.path().join("flow.js"),
        "function go() { measured(); }\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let coordinator = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = coordinator.session();
    let pin = coordinator
        .run(
            &IndexOptions::new(workspace.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
    (state, workspace, store, pin, session)
}
#[test]
fn v8_bootstrap_admits_only_empty_unpublished_evidence() {
    let (state, workspace) = fixture();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    assert_eq!(store.index_baseline().unwrap().index_revision, 0);
    assert!(store.recorded_index_options().unwrap().is_none());
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("index_not_ready"),
        "an empty bootstrap must not serve evidence"
    );
    let path = index_dir(state.path()).join("index.db");
    drop(store);
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute(
        "INSERT INTO native_source_sets(id,root_id) VALUES('forged-bootstrap','forged-root')",
        [],
    )
    .unwrap();
    drop(db);
    let reopened = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    assert!(
        reopened
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index"),
        "partial v8 bootstrap must fail closed"
    );
    let error = reopened
        .recorded_index_options()
        .expect_err("partial bootstrap cannot become an absent-option fallback");
    let reason = format!("{error:#}");
    assert!(reason.contains("partial v8 bootstrap"), "{reason}");
    #[cfg(feature = "test-support")]
    assert!(reopened.recovery_pending_for_tests());
}

#[test]
fn published_v8_missing_reconcile_options_never_looks_like_a_bootstrap() {
    let (state, _workspace, store, pin, _session) = projection_fixture();
    assert_eq!(store.status().unwrap().revision, pin);
    let db = rusqlite::Connection::open(index_dir(state.path()).join("index.db")).unwrap();
    db.execute("UPDATE index_metadata SET reconcile_options=NULL", [])
        .unwrap();
    let error = store
        .recorded_index_options()
        .expect_err("published v8 cannot omit recorded options");
    let reason = format!("{error:#}");
    assert!(reason.contains("missing reconcile options"), "{reason}");
    #[cfg(feature = "test-support")]
    assert!(store.recovery_pending_for_tests());
}

fn index_dir(state: &Path) -> std::path::PathBuf {
    fs::read_dir(state.join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
}
fn staged_files(dir: &Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("index.db.tmp-")
        })
        .collect()
}

#[test]
fn first_index_is_invisible_until_validated_and_synced() {
    let (state, workspace) = fixture();
    let (ready, arrived) = mpsc::channel();
    let (release, proceed) = mpsc::channel();
    let state_path = state.path().to_owned();
    let workspace_path = workspace.path().to_owned();
    let opener = std::thread::spawn(move || {
        Store::open_for_tests_with_index_stage_hook(&state_path, &workspace_path, |staged| {
            ready.send(staged.to_owned()).unwrap();
            proceed.recv().unwrap();
            Ok(())
        })
    });
    let staged = arrived.recv().unwrap();
    let dir = index_dir(state.path());
    assert_eq!(staged.parent(), Some(dir.as_path()));
    assert!(!dir.join("index.db").exists());
    assert_eq!(staged_files(&dir), vec![staged.clone()]);
    assert!(fs::metadata(&staged).unwrap().len() > 0);
    // The daemon serializes first-writer activation. A second direct Store
    // opener here would model an unsupported competing writer, not a read.
    release.send(()).unwrap();
    let store = opener.join().unwrap().unwrap();
    assert_eq!(store.index_baseline().unwrap().index_revision, 0);
    assert_eq!(staged_files(&dir), Vec::<std::path::PathBuf>::new());
    drop(store);
    let reopened = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    assert_eq!(reopened.index_baseline().unwrap().index_revision, 0);
}

#[test]
fn failed_stage_does_not_publish_or_leave_its_temp_file() {
    let (state, workspace) = fixture();
    let error = Store::open_for_tests_with_index_stage_hook(state.path(), workspace.path(), |_| {
        anyhow::bail!("injected stage failure")
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "injected stage failure");
    let dir = index_dir(state.path());
    assert!(!dir.join("index.db").exists());
    assert!(staged_files(&dir).is_empty());
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    assert_eq!(store.index_baseline().unwrap().index_revision, 0);
    assert!(staged_files(&dir).is_empty());
}

#[test]
fn truncated_stage_is_rejected_before_publication() {
    let (state, workspace) = fixture();
    let error =
        Store::open_for_tests_with_index_stage_hook(state.path(), workspace.path(), |staged| {
            fs::OpenOptions::new()
                .write(true)
                .open(staged)?
                .set_len(0)?;
            Ok(())
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("exceptional index format recovery deferred"),
        "{error:#}"
    );
    let dir = index_dir(state.path());
    assert!(!dir.join("index.db").exists());
    assert!(staged_files(&dir).is_empty());
    assert_eq!(
        Store::open_for_tests(state.path(), workspace.path())
            .unwrap()
            .index_baseline()
            .unwrap()
            .index_revision,
        0
    );
}

#[test]
fn crash_left_partial_stage_is_not_published_or_mistaken_for_the_index() {
    let (state, workspace) = fixture();
    let _ = Store::open_for_tests_with_index_stage_hook(state.path(), workspace.path(), |staged| {
        // Model a previous process stopping before publish. Its private staging file
        // cannot be confused with the final index pathname on a later open.
        fs::write(
            staged.with_file_name("index.db.tmp-crashed"),
            b"SQLite form",
        )
        .unwrap();
        anyhow::bail!("injected crash")
    });
    let dir = index_dir(state.path());
    assert!(!dir.join("index.db").exists());
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    assert_eq!(store.index_baseline().unwrap().index_revision, 0);
    assert_eq!(
        fs::read(dir.join("index.db.tmp-crashed")).unwrap(),
        b"SQLite form"
    );
    assert_eq!(staged_files(&dir).len(), 1);
}

#[test]
fn preexisting_corrupt_index_is_not_reinitialized() {
    use std::os::unix::fs::MetadataExt;

    let (state, workspace) = fixture();
    drop(Store::open_for_tests(state.path(), workspace.path()).unwrap());
    let dir = index_dir(state.path());
    let index = dir.join("index.db");
    fs::OpenOptions::new()
        .write(true)
        .open(&index)
        .unwrap()
        .set_len(0)
        .unwrap();
    let before = fs::metadata(&index).unwrap();
    assert_eq!(before.len(), 0);
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    #[allow(unused_mut)] // Test-only writer refusal is appended under test-support.
    let mut refusals = vec![
        store.status().unwrap_err(),
        store.index_baseline().unwrap_err(),
    ];
    #[cfg(feature = "test-support")]
    refusals.push(
        store
            .publication_for_tests()
            .expect_err("exceptional format writer refusal"),
    );
    for refusal in refusals {
        assert!(
            refusal
                .to_string()
                .contains("exceptional index recovery deferred"),
            "{refusal:#}"
        );
    }
    let after = fs::metadata(&index).unwrap();
    assert_eq!(after.ino(), before.ino());
    assert_eq!(after.len(), 0);
    assert!(staged_files(&dir).is_empty());
}

#[test]
fn published_sqlite_header_with_missing_pages_refuses_reads_until_explicit_recovery() {
    use std::{
        os::unix::fs::MetadataExt,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{index_coordinator::reconcile_workspace, indexer::IndexOptions};

    let (state, workspace, store, old_pin, session) = projection_fixture();
    // Queue bootstrap publishes under index-use EX. Retire this fixture's
    // writer SH before accepting the pre-recovery request.
    drop(session);
    // An accepted queue survives exceptional index recovery. Its file cannot
    // disappear and be silently replaced while publication is retried.
    let accepted = store
        .enqueue_request(&IndexOptions::new(workspace.path().to_owned()), None)
        .unwrap();
    let queue = store.request_db_path();
    let queue_inode = fs::metadata(&queue).unwrap().ino();
    drop(store);
    let dir = index_dir(state.path());
    let index = dir.join("index.db");
    let file = fs::OpenOptions::new().write(true).open(&index).unwrap();
    file.set_len(128).unwrap();
    drop(file);
    let old_bytes = fs::read(&index).unwrap();
    assert_eq!(&old_bytes[..16], b"SQLite format 3\0");
    assert!(old_bytes.len() > 20);
    let old_inode = fs::metadata(&index).unwrap().ino();

    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    assert!(
        store.status().is_err(),
        "a damaged published index must not be served"
    );
    assert!(
        store.source_at("flow.js", Some(old_pin)).is_err(),
        "a damaged source must not be served"
    );
    assert_eq!(fs::read(&index).unwrap(), old_bytes);
    assert_eq!(fs::metadata(&index).unwrap().ino(), old_inode);
    assert!(staged_files(&dir).is_empty());

    let options = IndexOptions::new(workspace.path().to_owned());
    let cancelled = Arc::new(AtomicBool::new(true));
    let failure = reconcile_workspace(&store, &options, &cancelled, |_| {}).unwrap_err();
    assert!(failure.to_string().contains("cancelled"), "{failure:#}");
    assert!(store.status().is_err());
    assert_eq!(fs::read(&index).unwrap(), old_bytes);
    assert_eq!(fs::metadata(&index).unwrap().ino(), old_inode);
    assert!(staged_files(&dir).is_empty());
    assert_eq!(store.request_db_path(), queue);
    assert_eq!(fs::metadata(&queue).unwrap().ino(), queue_inode);
    assert_eq!(
        store.request_by_id(&accepted.id).unwrap().unwrap().state,
        "queued"
    );

    let ready = Arc::new(AtomicBool::new(false));
    let (recovered, _session) = reconcile_workspace(&store, &options, &ready, |_| {}).unwrap();
    assert_eq!(recovered.index_revision, 1);
    assert_ne!(recovered.index_generation, old_pin.index_generation);
    assert!(store.source_at("flow.js", Some(old_pin)).is_err());
    assert!(
        store
            .source_at("flow.js", Some(recovered))
            .unwrap()
            .is_some()
    );
    assert!(staged_files(&dir).is_empty());
    assert_eq!(store.request_db_path(), queue);
    assert_eq!(fs::metadata(&queue).unwrap().ino(), queue_inode);
    assert_eq!(
        store.request_by_id(&accepted.id).unwrap().unwrap().state,
        "queued"
    );
}

#[test]
fn failed_capture_leaves_existing_index_revision_unchanged() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use trellis::{
        indexer::{IndexOptions, index_workspace},
        model::CancelFlag,
    };
    let (state, workspace) = fixture();
    fs::write(workspace.path().join("main.js"), "f();").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let before = store.index_baseline().unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let error = index_workspace(
        &IndexOptions::new(workspace.path().to_owned()),
        &cancel,
        |progress| {
            if progress.phase == "parse" {
                cancel.store(true, Ordering::Relaxed);
            }
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert_eq!(store.index_baseline().unwrap(), before);
    assert_eq!(
        Store::open_for_tests(state.path(), workspace.path())
            .unwrap()
            .index_baseline()
            .unwrap(),
        before
    );
}

#[test]
fn drift_refusal_preserves_populated_store_pair_and_graph() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        indexer::{IndexOptions, index_workspace},
        model::CancelFlag,
    };
    let (state, workspace) = fixture();
    let root = workspace.path();
    fs::write(root.join("main.js"), "function f() {} f();").unwrap();
    let options = IndexOptions::new(root.to_owned());
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), root).unwrap();
    let graph = index_workspace(&options, &cancel, |_| {}).unwrap();
    let (before, _writer) =
        trellis::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    assert_eq!(store.graph().unwrap().files, graph.files);
    assert_eq!(before.index_revision, 1);
    let error = index_workspace(&options, &cancel, |progress| {
        if progress.phase == "parse" {
            fs::write(root.join("main.js"), "function f() {} g();").unwrap();
        }
    })
    .unwrap_err();
    assert!(error.to_string().contains("drift"), "{error:#}");
    let reopened = Store::open_for_tests(state.path(), root).unwrap();
    assert_eq!(reopened.status().unwrap().revision, before);
    assert_eq!(store.index_baseline().unwrap(), before);
}

#[test]
fn coordinator_rejects_drift_cancel_and_stale_pair_without_partial_publication() {
    use sha2::{Digest, Sha256};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };
    let (state, workspace) = fixture();
    let root = workspace.path();
    fs::write(
        root.join("main.js"),
        "function seed() { sink(); } function sink() {}
",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), root).unwrap();
    let options = IndexOptions::new(root.to_owned());
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = job.session();
    let first = job.run(&options, &cancel, |_| {}).unwrap();
    let (old_pin, old_file) = store.source_at("main.js", Some(first)).unwrap().unwrap();
    assert_eq!(old_pin, first);
    let original = old_file.text;
    let original_hash = hex::encode(Sha256::digest(original.as_bytes()));
    let assert_prior_head_served = || {
        assert_eq!(store.status().unwrap().revision, first);
        for selected in [Some(first), None] {
            let (served_pin, served_source) =
                store.source_at("main.js", selected).unwrap().unwrap();
            assert_eq!(served_pin, first);
            assert_eq!(served_source.text, original);
        }
    };
    let index = index_dir(state.path()).join("index.db");
    let persisted_source = || {
        let db = rusqlite::Connection::open(&index).unwrap();
        let (native_bytes, native_hash): (Vec<u8>, String) = db
            .query_row(
                "SELECT v.source_bytes,v.content_hash FROM document_versions v JOIN revision_documents m ON m.vid=v.vid WHERE m.path='main.js'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        (
            String::from_utf8(native_bytes.clone()).unwrap(),
            native_bytes,
            native_hash,
        )
    };
    let cancelled =
        IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone()).unwrap();
    cancel.store(true, Ordering::Release);
    assert!(
        cancelled
            .run(&options, &cancel, |_| {})
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    cancel.store(false, Ordering::Release);
    assert_eq!(store.index_baseline().unwrap(), first);
    assert_eq!(store.status().unwrap().revision, first);
    let (served_pin, served_source) = store.source_at("main.js", Some(first)).unwrap().unwrap();
    assert_eq!(served_pin, first);
    assert_eq!(served_source.text, original);
    assert_eq!(
        persisted_source(),
        (
            original.clone(),
            original.as_bytes().to_vec(),
            original_hash.clone()
        )
    );

    // Cancellation after admission and projection, before publication, also rolls back.
    fs::write(root.join("main.js"), "function late() {}\n").unwrap();
    let late =
        IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone()).unwrap();
    let late_cancel = cancel.clone();
    let mut phases = Vec::new();
    let phase_log = std::sync::Mutex::new(&mut phases);
    let error = late
        .run(&options, &cancel, |p| {
            phase_log.lock().unwrap().push(p.phase.clone());
            if p.phase == "complete" {
                late_cancel.store(true, Ordering::Release);
            }
        })
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    assert!(
        phases.iter().any(|p| p == "scan") && phases.iter().any(|p| p == "complete"),
        "cancel must follow capture: {phases:?}"
    );
    cancel.store(false, Ordering::Release);
    assert_eq!(store.index_baseline().unwrap(), first);
    assert_prior_head_served();
    assert_eq!(
        persisted_source(),
        (
            original.clone(),
            original.as_bytes().to_vec(),
            original_hash.clone()
        )
    );
    fs::write(root.join("main.js"), &original).unwrap();

    let drift =
        IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone()).unwrap();
    let error = drift
        .run(&options, &cancel, |p| {
            if p.phase == "parse" {
                fs::write(
                    root.join("main.js"),
                    "function changed() {}
",
                )
                .unwrap();
            }
        })
        .unwrap_err();
    assert!(error.to_string().contains("drift"), "{error:#}");
    assert_eq!(store.index_baseline().unwrap(), first);
    assert_prior_head_served();
    assert_eq!(
        persisted_source(),
        (
            original.clone(),
            original.as_bytes().to_vec(),
            original_hash.clone()
        )
    );
    let stale = IndexJobCoordinator::prepare_with_session(
        &store,
        Some(trellis::model::IndexPin {
            index_generation: first.index_generation,
            index_revision: first.index_revision - 1,
        }),
        session.clone(),
    );
    assert!(
        matches!(stale, Err(error) if error.to_string().contains("revision conflict")),
        "stale pair must refuse before work"
    );
    assert_eq!(store.index_baseline().unwrap(), first);
    assert_prior_head_served();
    assert_eq!(
        persisted_source(),
        (
            original.clone(),
            original.as_bytes().to_vec(),
            original_hash.clone()
        )
    );
    let next = IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone())
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    assert_eq!(next.index_generation, first.index_generation);
    assert_eq!(next.index_revision, first.index_revision + 1);
    assert_eq!(
        store
            .source_at("main.js", Some(first))
            .unwrap()
            .unwrap()
            .1
            .text,
        original,
        "retained P remains readable after Q publication"
    );
    assert_eq!(
        store
            .source_at("main.js", Some(next))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function changed() {}
"
    );
}

#[test]
fn full_reconcile_replaces_persisted_source_and_input_inventory() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };
    let (state, workspace) = fixture();
    fs::write(workspace.path().join("one.js"), "function one() {}\n").unwrap();
    fs::write(
        workspace.path().join("package.json"),
        "{\"name\":\"first\"}\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(workspace.path().to_owned());
    let first = IndexJobCoordinator::prepare(&store, None)
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    fs::remove_file(workspace.path().join("one.js")).unwrap();
    fs::write(workspace.path().join("two.js"), "function two() {}\n").unwrap();
    fs::write(
        workspace.path().join("package.json"),
        "{\"name\":\"second\"}\n",
    )
    .unwrap();
    let second = IndexJobCoordinator::prepare(&store, Some(first))
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    assert_eq!(second.index_generation, first.index_generation);
    assert_eq!(second.index_revision, first.index_revision + 1);

    let db = rusqlite::Connection::open(index_dir(state.path()).join("index.db")).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        30
    );
    let files = db
        .prepare("SELECT path FROM revision_documents WHERE revision_id=?1 ORDER BY path")
        .unwrap()
        .query_map(
            [format!(
                "pin:v1:{}:{}",
                second.index_generation, second.index_revision
            )],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(files, vec!["two.js"]);
    let capture_stat: String = db
        .query_row(
            "SELECT capture_stat FROM revision_documents WHERE revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1) AND path='two.js'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&capture_stat).unwrap()["version"],
        1
    );
    let package: String = db
        .query_row(
            "SELECT payload FROM revision_capture_inputs WHERE revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1) AND input_key='config:package.json'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&package).unwrap()["state"],
        "present"
    );
    let absent: i64 = db
        .query_row(
            "SELECT count(*) FROM revision_capture_inputs WHERE revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1) AND payload='{\"state\":\"absent\"}'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(absent > 0);
    let options_json: String = db
        .query_row("SELECT reconcile_options FROM index_metadata", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&options_json).unwrap()["maxFileBytes"],
        2 * 1024 * 1024
    );
}

fn sqlite_snapshot(
    path: &Path,
    pin: trellis::model::IndexPin,
    normalize_current_publication: bool,
) -> Vec<(String, Vec<Vec<String>>)> {
    use rusqlite::types::Value;
    let db = rusqlite::Connection::open(path).unwrap();
    let (generation, published_revision): (String, i64) = db
        .query_row(
            "SELECT index_generation,index_revision FROM index_metadata",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(uuid::Uuid::parse_str(&generation).is_ok());
    assert_eq!(pin.index_generation.to_string(), generation);
    if normalize_current_publication {
        assert_eq!(pin.index_revision as i64, published_revision);
    }
    let revision_id = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
    let header_revision: i64 = db
        .query_row(
            "SELECT published_index_revision FROM native_revisions WHERE id=?1",
            [&revision_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(header_revision, pin.index_revision as i64);
    let violations: i64 = db
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0, "all retained revision FKs must remain valid");
    // The rowids depend on when retained records were inserted. Compare every
    // surrogate column by its immutable natural key, without dropping any rows
    // or normalizing the rest of each stored row.
    let natural_keys = |sql: &str| -> std::collections::BTreeMap<i64, String> {
        db.prepare(sql)
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<(i64, String)>>>()
            .unwrap()
            .into_iter()
            .collect()
    };
    let versions = natural_keys("SELECT vid,id FROM document_versions");
    let graphs = natural_keys("SELECT gid,'graph:v1:'||graph_hash FROM graph_projections");
    let classes =
        natural_keys("SELECT cid,'class-projection:v1:'||content_hash FROM class_projections");
    let tables: Vec<String> = db
        // Decision 0013's trigram prefilter tables are derived search state, not evidence.
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name!='index_metadata' AND name NOT LIKE '%trigrams%' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    // Check exact schema-30 names before comparing evidence and the logical
    // search corpus. FTS shadow bytes depend on publication/merge history.
    const REQUIRED_V28_TABLES: [&str; 74] = [
        "class_projections",
        "class_relations",
        "classes",
        "document_versions",
        "graph_display_labels",
        "graph_nodes",
        "graph_projections",
        "head_search_fts",
        "head_search_fts_config",
        "head_search_fts_content",
        "head_search_fts_data",
        "head_search_fts_docsize",
        "head_search_fts_idx",
        "native_binding_epoch",
        "native_producer_inputs",
        "native_producer_languages",
        "native_producers",
        "native_release_candidate_classes",
        "native_release_candidate_graphs",
        "native_release_candidate_versions",
        "native_revision_release_debt",
        "native_revision_supersessions",
        "native_revisions",
        "native_source_set_dependencies",
        "native_source_set_languages",
        "native_source_sets",
        "native_version_ancestor_signature_types",
        "native_version_call_regions",
        "native_version_calls",
        "native_version_capability_families",
        "native_version_capability_gaps",
        "native_version_control_regions",
        "native_version_coverage_roles",
        "native_version_declaration_ancestors",
        "native_version_declarations",
        "native_version_header_items",
        "native_version_headers",
        "native_version_own_signature_types",
        "native_version_parameters",
        "native_version_references",
        "native_version_search_cards",
        "revision_capture_inputs",
        "revision_documents",
        "revision_producer_bindings",
        "semantic_basis",
        "semantic_basis_lookup_dependencies",
        "semantic_call_bindings",
        "semantic_call_candidates",
        "semantic_call_join_candidates",
        "semantic_call_possible_dispatch",
        "semantic_coverage",
        "semantic_coverage_roles",
        "semantic_declaration_binding_symbols",
        "semantic_declaration_bindings",
        "semantic_declaration_join_candidates",
        "semantic_import_reports",
        "semantic_producer_languages",
        "semantic_producers",
        "semantic_provenance",
        "semantic_raw_batches",
        "semantic_raw_captures",
        "semantic_raw_documents",
        "semantic_raw_exclusions",
        "semantic_raw_proofs",
        "semantic_reference_candidates",
        "semantic_reference_roles",
        "semantic_references",
        "semantic_requested_producer_languages",
        "semantic_requested_producers",
        "semantic_revision_producers",
        "semantic_revision_raw_selections",
        "semantic_symbol_targets",
        "semantic_symbols",
        "semantic_type_relationships",
    ];
    assert_eq!(
        tables.iter().map(String::as_str).collect::<Vec<_>>(),
        REQUIRED_V28_TABLES,
        "exact schema-30 evidence and search table inventory"
    );
    if tables.contains(&"native_binding_epoch".to_string()) {
        let (first, bound): (i64, i64) = db
            .query_row(
                "SELECT (SELECT first_revision FROM native_binding_epoch),
                    (SELECT count(*) FROM revision_producer_bindings WHERE revision_id=?1)",
                [&revision_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            bound,
            i64::from(pin.index_revision as i64 >= first),
            "new selected revision must have its own producer consistency binding"
        );
    }
    let corpus = |sql: &str, revision: &str| {
        let mut rows = db
            .prepare(sql)
            .unwrap()
            .query_map([revision], |row| {
                let vid: i64 = row.get(1)?;
                Ok(vec![
                    format!("t:{}", row.get::<_, String>(0)?),
                    format!("t:{}", versions.get(&vid).unwrap()),
                    format!("b:{}", hex::encode(row.get::<_, Vec<u8>>(2)?)),
                ])
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows.sort();
        rows
    };
    let card_corpus_sql = "SELECT c.terms,c.vid,c.syntax_id FROM native_version_search_cards c
        JOIN revision_documents m ON m.vid=c.vid WHERE m.revision_id=?1";
    let head_revision = format!("pin:v1:{generation}:{published_revision}");
    assert_eq!(
        corpus(
            "SELECT terms,vid,syntax_id FROM head_search_fts WHERE ?1 IS NOT NULL",
            &head_revision
        ),
        corpus(card_corpus_sql, &head_revision),
        "physical head search corpus must contain exactly the current version cards"
    );
    // The FTS corpus belongs only to the head, while old pins keep their version
    // cards. Compare the selected pin's logical corpus, never FTS segment bytes.
    let selected_corpus = (
        "head_search_fts".into(),
        corpus(card_corpus_sql, &revision_id),
    );
    tables
        .into_iter()
        .filter(|table| table != "head_search_fts" && !table.starts_with("head_search_fts_"))
        // Generation-specific control provenance is checked via pinned reads,
        // not compared to a cold oracle with a different generation UUID.
        .filter(|table| !matches!(table.as_str(),
            "native_binding_epoch" | "revision_producer_bindings" | "native_revision_supersessions"))
        .map(|table| {
            // Select the complete revision projection, not all archived rows.
            // Every table remains in the independent cold-source comparison.
            let (where_clause, alias) = match table.as_str() {
                "native_revisions" => ("id=?1", ""),
                "revision_capture_inputs" | "revision_documents"
                | "native_revision_release_debt" | "native_release_candidate_versions"
                | "native_release_candidate_graphs" | "native_release_candidate_classes" =>
                    ("revision_id=?1", ""),
                "native_source_sets" => ("id=(SELECT source_set_id FROM native_revisions WHERE id=?1)", ""),
                "native_source_set_languages" | "native_source_set_dependencies" =>
                    ("source_set_id=(SELECT source_set_id FROM native_revisions WHERE id=?1)", ""),
                "native_producers" | "native_producer_languages" | "native_producer_inputs" =>
                    ("EXISTS(SELECT 1 FROM document_versions v JOIN revision_documents d ON d.vid=v.vid WHERE d.revision_id=?1 AND v.producer_id=selected.producer_id AND v.producer_version=selected.producer_version)", "producer"),
                "document_versions" => ("vid IN (SELECT vid FROM revision_documents WHERE revision_id=?1)", ""),
                "graph_projections" => ("gid IN (SELECT gid FROM revision_documents WHERE revision_id=?1)", ""),
                "graph_nodes" | "graph_display_labels" =>
                    ("gid IN (SELECT gid FROM revision_documents WHERE revision_id=?1)", ""),
                "class_projections" => ("cid IN (SELECT cid FROM revision_documents WHERE revision_id=?1)", ""),
                "classes" | "class_relations" =>
                    ("cid IN (SELECT cid FROM revision_documents WHERE revision_id=?1)", ""),
                name if name.starts_with("native_version_") =>
                    ("vid IN (SELECT vid FROM revision_documents WHERE revision_id=?1)", ""),
                name if name.starts_with("semantic_") => ("?1 IS NOT NULL", ""),
                _ => panic!("unhandled schema-30 evidence table: {table}"),
            };
            // Producer descriptor columns differ: only the parent table uses id/version.
            let where_clause = if table == "native_producers" {
                "EXISTS(SELECT 1 FROM document_versions v JOIN revision_documents d ON d.vid=v.vid WHERE d.revision_id=?1 AND v.producer_id=selected.id AND v.producer_version=selected.version)"
            } else {
                where_clause
            };
            let alias = if alias.is_empty() { "" } else { " selected" };
            let sql = format!("SELECT * FROM \"{table}\"{alias} WHERE {where_clause}");
            let mut statement = db.prepare(&sql).unwrap();
            let column_names: Vec<String> = statement
                .column_names()
                .map(str::to_owned)
                .collect();
            let mut rows = statement
                .query_map([&revision_id], |row| {
                    (0..column_names.len())
                        .map(|column| match row.get::<_, Value>(column)? {
                            Value::Null => Ok("null".to_owned()),
                            Value::Integer(value) if matches!(column_names[column].as_str(), "vid" | "gid" | "cid") => {
                                let natural = match column_names[column].as_str() {
                                    "vid" => &versions,
                                    "gid" => &graphs,
                                    "cid" => &classes,
                                    _ => unreachable!(),
                                };
                                Ok(format!("t:{}", natural.get(&value).unwrap_or_else(|| {
                                    panic!("unmapped {table}.{}={value}", column_names[column])
                                })))
                            }
                            Value::Integer(value) => Ok(format!("i:{value}")),
                            Value::Real(value) => Ok(format!("r:{value:?}")),
                            Value::Text(value) => Ok(format!("t:{value}")),
                            Value::Blob(value) => Ok(format!("b:{}", hex::encode(value))),
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            if table.starts_with("semantic_") {
                assert!(rows.is_empty(), "native-only selected snapshot has semantic rows in {table}");
            }
            // A retained old pin may carry its own pending debt. Compare those
            // rows raw; only the currently published head must be empty.
            if pin.index_revision as i64 == published_revision
                && matches!(
                    table.as_str(),
                    "native_revision_release_debt"
                        | "native_release_candidate_versions"
                        | "native_release_candidate_graphs"
                        | "native_release_candidate_classes"
                )
            {
                assert!(
                    rows.is_empty(),
                    "selected current head must not have pending release control rows in {table}"
                );
            }
            if table == "native_revisions" {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][0], format!("t:{revision_id}"));
                assert_eq!(rows[0][16], format!("i:{}", pin.index_revision));
                if normalize_current_publication {
                    rows[0][0] = "t:<current-pin>".into();
                    rows[0][16] = "i:<current-revision>".into();
                }
            } else if matches!(table.as_str(), "revision_capture_inputs" | "revision_documents") {
                for row in &mut rows {
                    assert_eq!(row[0], format!("t:{revision_id}"));
                    if normalize_current_publication {
                        row[0] = "t:<current-pin>".into();
                    }
                }
            }
            rows.sort();
            (table, rows)
        })
        .chain(std::iter::once(selected_corpus))
        .collect()
}

#[test]
fn parse_error_retained_pin_matches_independent_cold_and_advisory_uses_exact_token() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
        native_evidence::DocumentKey,
    };

    let (state, workspace) = fixture();
    let root = workspace.path();
    let source = "function foo() { return 1; } function caller() { obj.foo(); obj.foo.bar(); }\n";
    fs::write(root.join("calls.js"), source).unwrap();
    fs::write(root.join("broken.js"), "function sound() {}\n").unwrap();
    let options = IndexOptions::new(root.to_owned());
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), root).unwrap();
    let job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = job.session();
    let before = job.run(&options, &cancel, |_| {}).unwrap();
    let path = index_dir(state.path()).join("index.db");
    let old_rows = sqlite_snapshot(&path, before, false);
    let old_graph = store.graph_at(Some(before)).unwrap();

    // The changed document has a genuine parser error. A new isolated index in
    // this SAME workspace is the independent full-native oracle, never a copied F.
    fs::write(root.join("broken.js"), "function broken( {\n").unwrap();
    let parsed = IndexJobCoordinator::prepare_with_session(&store, Some(before), session.clone())
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    assert_eq!(sqlite_snapshot(&path, before, false), old_rows);
    assert_eq!(store.graph_at(Some(before)).unwrap(), old_graph);
    let parsed_graph = store.graph_at(Some(parsed)).unwrap();
    assert!(parsed_graph.stats.parse_error_files > 0);
    let parsed_classes =
        serde_json::to_value(store.classes_at(None, "", Some(parsed), 0, 100).unwrap()).unwrap();

    // #22 advisory is only this same-pin native primitive, never a binding.
    // Compare actual captured token bytes, then exact language/source-set key.
    let key = DocumentKey {
        source_set_id: format!("source-set:v1:{}", store.root_id()),
        language: "javascript".into(),
        path: "calls.js".into(),
    };
    let selected = store.native_source_at(parsed, &key).unwrap().unwrap();
    let captured = std::str::from_utf8(&selected.1).unwrap();
    let coverage = store.native_coverage_at(parsed, &key).unwrap().unwrap();
    let owners = store
        .native_declarations_at(parsed, "javascript", "caller")
        .unwrap();
    assert_eq!(owners.len(), 1);
    let calls = store.native_calls_at(parsed, &owners[0].syntax_id).unwrap();
    let positive = calls
        .iter()
        .find(|call| &captured[call.range.start..call.range.end] == "obj.foo()")
        .expect("positive captured call");
    let token = positive
        .callee_range
        .as_ref()
        .expect("exact identifier range");
    assert_eq!(&captured[token.start..token.end], "foo");
    assert_eq!(positive.spelling.as_deref(), Some("foo"));
    let candidates = store
        .native_declarations_at(parsed, "javascript", "foo")
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].document.source_set_id, key.source_set_id);
    assert_eq!(candidates[0].document.language, key.language);
    assert_eq!(positive.revision_id, coverage.revision_id);
    assert_eq!(candidates[0].revision_id, coverage.revision_id);
    assert!(positive.provenance_id.starts_with("native-proof:v1:"));
    assert!(candidates[0].provenance_id.starts_with("native-proof:v1:"));

    let compound = calls
        .iter()
        .find(|call| &captured[call.range.start..call.range.end] == "obj.foo.bar()")
        .expect("compound captured call");
    let compound_token = compound
        .callee_range
        .as_ref()
        .expect("terminal identifier range");
    assert_eq!(&captured[compound_token.start..compound_token.end], "bar");
    assert_ne!(
        &captured[compound_token.start..compound_token.end],
        "obj.foo.bar"
    );
    assert!(
        store
            .native_declarations_at(parsed, "javascript", "bar")
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .native_declarations_at(parsed, "javascript", "obj.foo.bar")
            .unwrap()
            .is_empty()
    );
    assert_eq!(compound.revision_id, coverage.revision_id);

    // A fresh independent full build shares the captured workspace root but not
    // state or stored native/class/graph projections. Taking its writer guard can
    // retire the first store's public reader; collect those DTOs above first.
    let cold_state = tempfile::tempdir().unwrap();
    fs::set_permissions(cold_state.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let cold = Store::open_for_tests(cold_state.path(), root).unwrap();
    let cold_job = IndexJobCoordinator::prepare(&cold, None).unwrap();
    let _cold_session = cold_job.session();
    let cold_pin = cold_job.run(&options, &cancel, |_| {}).unwrap();
    assert_eq!(
        sqlite_snapshot(&path, parsed, true),
        sqlite_snapshot(
            &index_dir(cold_state.path()).join("index.db"),
            cold_pin,
            true
        ),
        "all selected v14 evidence tables: source/hash, revision, coverage, provenance, IDs, ordered native children, graph, class/warnings/truncated, and empty current-head release control"
    );
    assert_eq!(sqlite_snapshot(&path, before, false), old_rows);
    assert_eq!(parsed_graph, cold.graph_at(Some(cold_pin)).unwrap());
    let mut cold_classes =
        serde_json::to_value(cold.classes_at(None, "", Some(cold_pin), 0, 100).unwrap()).unwrap();
    let mut parsed_classes = parsed_classes;
    assert_eq!(
        parsed_classes["revision"]["indexRevision"],
        parsed.index_revision
    );
    assert_eq!(
        cold_classes["revision"]["indexRevision"],
        cold_pin.index_revision
    );
    parsed_classes.as_object_mut().unwrap().remove("revision");
    cold_classes.as_object_mut().unwrap().remove("revision");
    assert_eq!(parsed_classes, cold_classes);
    assert_eq!(
        selected.1,
        cold.native_source_at(cold_pin, &key).unwrap().unwrap().1
    );
    assert_eq!(
        coverage,
        cold.native_coverage_at(cold_pin, &key).unwrap().unwrap()
    );
    let cold_owner = cold
        .native_declarations_at(cold_pin, "javascript", "caller")
        .unwrap();
    assert_eq!(owners, cold_owner);
    assert_eq!(
        calls,
        cold.native_calls_at(cold_pin, &cold_owner[0].syntax_id)
            .unwrap()
    );
    assert_eq!(
        candidates,
        cold.native_declarations_at(cold_pin, "javascript", "foo")
            .unwrap()
    );
    assert!(
        cold.native_declarations_at(cold_pin, "javascript", "bar")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn new_producer_binding_refuses_unilateral_executable_header_and_binding_tamper() {
    for (label, sql) in [
        (
            "executable capture",
            "UPDATE revision_capture_inputs SET payload=json_set(payload,'$.hash',printf('%064d',0)) WHERE input_key LIKE 'executable:%'",
        ),
        (
            "selector capture",
            "UPDATE revision_capture_inputs SET payload=(SELECT payload FROM revision_capture_inputs WHERE input_key LIKE 'executable:%' LIMIT 1) WHERE input_key='toolchain:rust-toolchain.toml'",
        ),
        (
            "revision header",
            "UPDATE native_revisions SET toolchain_hash=printf('%064d',0)",
        ),
        (
            "binding digest",
            "UPDATE revision_producer_bindings SET binding_sha=printf('%064d',0)",
        ),
        ("missing binding", "DELETE FROM revision_producer_bindings"),
    ] {
        let (state, _workspace, store, pin, _session) = projection_fixture();
        let db = rusqlite::Connection::open(index_dir(state.path()).join("index.db")).unwrap();
        assert_eq!(
            db.execute(sql, []).unwrap(),
            1,
            "{label}: mutate exactly one valid row"
        );
        drop(db);
        let err = store.graph_at(Some(pin)).unwrap_err();
        assert!(
            format!("{err:#}").contains("incompatible_index"),
            "{label}: {err:#}"
        );
        let db = rusqlite::Connection::open(index_dir(state.path()).join("index.db")).unwrap();
        let (generation, revision): (String, i64) = db
            .query_row(
                "SELECT index_generation,index_revision FROM index_metadata",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (generation, revision),
            (pin.index_generation.to_string(), pin.index_revision as i64),
            "{label}: refusal cannot rotate or publish a new generation"
        );
    }
}

#[cfg(feature = "test-support")]
#[test]
fn current_producer_binding_preserves_old_pin_and_queue() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };
    let (state, workspace) = fixture();
    fs::write(
        workspace.path().join("A.java"),
        "class A { int method() { return 1; } }\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let first = IndexJobCoordinator::prepare_with_session(&store, None, session.clone())
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    let first_source = store.source_at("A.java", Some(first)).unwrap().unwrap().1;
    // Publish the virgin queue without this fixture's writer SH, then resume
    // the same-generation publication test with a fresh writer session.
    drop(session);
    let _queued = store.enqueue_request(&options, None).unwrap();
    let db_path = index_dir(state.path()).join("index.db");
    let queue_path = index_dir(state.path()).join("requests.db");
    let queue_before = fs::read(&queue_path).unwrap();
    // The first publication binds the producer in the base schema. Reusing
    // this generation must retain that binding, both pins and the queue.
    let db = rusqlite::Connection::open(&db_path).unwrap();
    let (initial_bound, initial_epoch): (i64, i64) = db
        .query_row(
            "SELECT (SELECT count(*) FROM revision_producer_bindings),
                    (SELECT first_revision FROM native_binding_epoch)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (initial_bound, initial_epoch),
        (1, first.index_revision as i64)
    );
    drop(db);
    assert_eq!(
        store.source_at("A.java", Some(first)).unwrap().unwrap().1,
        first_source
    );
    let generation = first.index_generation;
    fs::write(
        workspace.path().join("A.java"),
        "class A { int method() { return 2; } }\n",
    )
    .unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let second = IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone())
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    assert_eq!(second.index_generation, generation);
    assert_eq!(second.index_revision, first.index_revision + 1);
    assert_eq!(
        store.source_at("A.java", Some(first)).unwrap().unwrap().1,
        first_source
    );
    assert!(
        store
            .source_at("A.java", Some(second))
            .unwrap()
            .unwrap()
            .1
            .text
            .contains("return 2")
    );
    let db = rusqlite::Connection::open(db_path).unwrap();
    let (bound, epoch): (i64, i64) = db
        .query_row(
            "SELECT (SELECT count(*) FROM revision_producer_bindings),
                (SELECT first_revision FROM native_binding_epoch)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((bound, epoch), (2, first.index_revision as i64));
    assert_eq!(
        fs::read(queue_path).unwrap(),
        queue_before,
        "same-generation publication must not touch durable requests.db"
    );
    // A retained selected pin still rejects a one-sided captured executable
    // SHA forgery against its immutable producer binding.
    let old_key = format!("pin:v1:{}:{}", first.index_generation, first.index_revision);
    assert_eq!(db.execute(
        "UPDATE revision_capture_inputs SET payload=json_set(payload,'$.hash',printf('%064d',0))
         WHERE revision_id=?1 AND input_key LIKE 'executable:%'", [&old_key]).unwrap(),1);
    drop(db);
    let err = store.source_at("A.java", Some(first)).unwrap_err();
    assert!(
        format!("{err:#}").contains("incompatible_index"),
        "retained pin: {err:#}"
    );
}

#[cfg(feature = "test-support")]
fn assert_selected_native_matches(
    store: &Store,
    pin: trellis::model::IndexPin,
    measured: &trellis::native_evidence::Artifact,
) {
    use std::collections::BTreeSet;
    use trellis::native_evidence::MeasurementKind;
    let lookups = measured
        .declarations
        .iter()
        .filter_map(|declaration| {
            declaration
                .lookup_key
                .as_ref()
                .map(|lookup| (declaration.document.language.clone(), lookup.clone()))
        })
        .collect::<BTreeSet<_>>();
    for (language, lookup) in lookups {
        let mut actual = store
            .native_declarations_at(pin, &language, &lookup)
            .unwrap();
        let mut expected = measured
            .declarations
            .iter()
            .filter(|declaration| {
                declaration.document.language == language
                    && declaration.lookup_key.as_deref() == Some(lookup.as_str())
            })
            .cloned()
            .collect::<Vec<_>>();
        actual.sort_by(|left, right| left.syntax_id.cmp(&right.syntax_id));
        expected.sort_by(|left, right| left.syntax_id.cmp(&right.syntax_id));
        assert_eq!(actual, expected, "native declarations {language}/{lookup}");
    }
    assert_eq!(
        measured.capability_reports.len(),
        measured.revision.documents.len()
    );
    for document in &measured.revision.documents {
        assert_eq!(
            store
                .native_source_at(pin, &document.key)
                .unwrap()
                .unwrap()
                .0,
            *document
        );
        assert_eq!(
            store
                .native_coverage_at(pin, &document.key)
                .unwrap()
                .as_ref(),
            measured.coverage.iter().find(|coverage| {
                coverage.document_path == document.key.path
                    && coverage.language == document.key.language
            })
        );
        let expected = measured
            .capability_reports
            .iter()
            .find(|report| report.document == document.key)
            .unwrap();
        assert_eq!(expected.families.len(), MeasurementKind::ALL.len());
        for kind in MeasurementKind::ALL {
            assert_eq!(
                expected
                    .families
                    .iter()
                    .filter(|family| family.kind == kind)
                    .count(),
                1
            );
        }
        assert_eq!(
            store
                .native_capability_report_at(pin, &document.key)
                .unwrap()
                .as_ref(),
            Some(expected)
        );
    }
    for declaration in &measured.declarations {
        let mut references = store
            .native_references_at(pin, &declaration.syntax_id)
            .unwrap();
        let mut expected_references = measured
            .references
            .iter()
            .filter(|reference| reference.owner_syntax_id == declaration.syntax_id)
            .cloned()
            .collect::<Vec<_>>();
        references.sort_by(|left, right| left.id.cmp(&right.id));
        expected_references.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(references, expected_references);
        let mut calls = store.native_calls_at(pin, &declaration.syntax_id).unwrap();
        let mut expected_calls = measured
            .calls
            .iter()
            .filter(|call| call.owner_syntax_id == declaration.syntax_id)
            .cloned()
            .collect::<Vec<_>>();
        calls.sort_by(|left, right| left.id.cmp(&right.id));
        expected_calls.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(calls, expected_calls);
        let mut regions = store
            .native_control_regions_at(pin, &declaration.syntax_id)
            .unwrap();
        let mut expected_regions = measured
            .control_regions
            .iter()
            .filter(|region| region.owner_syntax_id == declaration.syntax_id)
            .cloned()
            .collect::<Vec<_>>();
        regions.sort_by(|left, right| left.id.cmp(&right.id));
        expected_regions.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(regions, expected_regions);
    }
}

#[cfg(feature = "test-support")]
fn measure_native_for_parity(
    workspace: &Path,
    store: &Store,
) -> trellis::native_evidence::Artifact {
    use std::sync::{Arc, atomic::AtomicBool};
    trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(workspace.to_owned()),
        store.root_id(),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap()
    .1
}

#[cfg(feature = "test-support")]
fn assert_native_reference_and_gap_fixture(measured: &trellis::native_evidence::Artifact) {
    assert!(
        !measured.references.is_empty(),
        "reference parity must be nonvacuous"
    );
    assert!(
        measured.capability_reports.iter().any(|report| {
            report.document.language == "rust"
                && report.families.iter().any(|family| !family.gaps.is_empty())
        }),
        "Rust macro must produce a genuine measured gap"
    );
}

fn assert_selected_matches_cold(
    state: &Path,
    workspace: &Path,
    store: &Store,
    pin: trellis::model::IndexPin,
) {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{index_coordinator::IndexJobCoordinator, indexer::IndexOptions};
    let cold_state = tempfile::tempdir().unwrap();
    fs::set_permissions(cold_state.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let cold = Store::open_for_tests(cold_state.path(), workspace).unwrap();
    let cold_pin = IndexJobCoordinator::prepare(&cold, None)
        .unwrap()
        .run(
            &IndexOptions::new(workspace.to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
    assert_eq!(
        sqlite_snapshot(&index_dir(state).join("index.db"), pin, true),
        sqlite_snapshot(
            &index_dir(cold_state.path()).join("index.db"),
            cold_pin,
            true
        ),
        "complete selected SQL must equal an independent same-root cold publication"
    );
    #[cfg(feature = "test-support")]
    {
        let measured = measure_native_for_parity(workspace, &cold);
        assert_native_reference_and_gap_fixture(&measured);
        assert_selected_native_matches(store, pin, &measured);
        assert_selected_native_matches(&cold, cold_pin, &measured);
    }
    let graph = store.graph_at(Some(pin)).unwrap();
    assert_eq!(graph, cold.graph_at(Some(cold_pin)).unwrap());
    let mut status = serde_json::to_value(store.status().unwrap()).unwrap();
    let mut cold_status = serde_json::to_value(cold.status().unwrap()).unwrap();
    for value in [&mut status, &mut cold_status] {
        let object = value.as_object_mut().unwrap();
        object.remove("revision");
        object.remove("indexedAt");
    }
    assert_eq!(status, cold_status);
    for path in std::iter::once(None).chain(graph.files.iter().map(|file| Some(file.path.as_str())))
    {
        let mut page =
            serde_json::to_value(store.classes_at(path, "", Some(pin), 0, 100).unwrap()).unwrap();
        let mut cold_page =
            serde_json::to_value(cold.classes_at(path, "", Some(cold_pin), 0, 100).unwrap())
                .unwrap();
        page.as_object_mut().unwrap().remove("revision");
        cold_page.as_object_mut().unwrap().remove("revision");
        assert_eq!(page, cold_page, "class page {path:?}");
    }
    for file in &graph.files {
        assert_eq!(
            store.source_at(&file.path, Some(pin)).unwrap().unwrap().1,
            cold.source_at(&file.path, Some(cold_pin))
                .unwrap()
                .unwrap()
                .1,
            "source API {}",
            file.path
        );
    }
}

fn selected_document_ids(
    state: &Path,
    pin: trellis::model::IndexPin,
) -> std::collections::BTreeMap<String, (String, i64, i64)> {
    let db = rusqlite::Connection::open(index_dir(state).join("index.db")).unwrap();
    let mut statement = db
        .prepare(
            "SELECT m.path,v.content_hash,m.vid,m.gid FROM revision_documents m
         JOIN document_versions v ON v.vid=m.vid WHERE m.revision_id=?1 ORDER BY m.path",
        )
        .unwrap();
    statement
        .query_map(
            [format!(
                "pin:v1:{}:{}",
                pin.index_generation, pin.index_revision
            )],
            |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?, row.get(3)?))),
        )
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn document_local_any_shape_and_inventory_match_independent_cold_publications() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{index_coordinator::IndexJobCoordinator, indexer::IndexOptions};
    let (state, workspace) = fixture();
    for (path, source) in [
        (
            "local.js",
            "function local() { return 1; }\nfunction checked() { const local=1, other=2; return local+other; }\n",
        ),
        (
            "A.java",
            "class A { int run() { return 1; } int checked() { int local=1, other=2; return local+other; } }\n",
        ),
        (
            "run.py",
            "class A:\n    def run(self):\n        return 1\n    def checked(self):\n        local=1; other=2\n        return local+other\n",
        ),
        (
            "run.rs",
            "fn run()->i32 { opaque!(hidden); return 1; }\nfn checked()->i32 { let local=1; let other=2; return local+other; }\n",
        ),
        ("anchor.js", "function anchor() {}\n"),
    ] {
        fs::write(workspace.path().join(path), source).unwrap();
    }
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let first_job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = first_job.session();
    let first = first_job.run(&options, &cancel, |_| {}).unwrap();
    #[cfg(feature = "test-support")]
    let old_families = {
        let measured = measure_native_for_parity(workspace.path(), &store);
        assert_native_reference_and_gap_fixture(&measured);
        assert_selected_native_matches(&store, first, &measured);
        measured
    };
    let index = index_dir(state.path()).join("index.db");
    let old_sql = sqlite_snapshot(&index, first, false);
    let old_graph = store.graph_at(Some(first)).unwrap();
    let old_source = store.source_at("local.js", Some(first)).unwrap().unwrap().1;
    let old_classes =
        serde_json::to_value(store.classes_at(None, "", Some(first), 0, 100).unwrap()).unwrap();
    let old_native = serde_json::to_value(
        store
            .native_declarations_at(first, "javascript", "local")
            .unwrap(),
    )
    .unwrap();
    let mut latest = first;
    for stage in [
        "single body",
        "several bodies",
        "declaration surfaces",
        "all documents",
        "add",
        "delete",
        "rename",
    ] {
        let prior_ids = selected_document_ids(state.path(), latest);
        match stage {
            "single body" => {
                let source = fs::read_to_string(workspace.path().join("local.js")).unwrap();
                fs::write(
                    workspace.path().join("local.js"),
                    source.replacen("return 1", "return 10000", 1),
                )
                .unwrap();
            }
            "several bodies" => {
                for path in ["local.js", "A.java", "run.py", "run.rs"] {
                    let source = fs::read_to_string(workspace.path().join(path)).unwrap();
                    fs::write(
                        workspace.path().join(path),
                        source.replace("return local+other", "return other+other"),
                    )
                    .unwrap();
                }
            }
            "declaration surfaces" => {
                for (path, source) in [
                    (
                        "local.js",
                        "import { external } from './missing.js';\nexport function renamed(input) { if (input) { external(); } return input; }\n// appended top-level comment\n",
                    ),
                    (
                        "A.java",
                        "class Base {}\nclass Renamed extends Base { int field; int run(int value) { if (value > 0) { helper(); } return value; } void helper() {} }\n",
                    ),
                    (
                        "run.py",
                        "from missing import external\nclass Base: pass\nclass Renamed(Base):\n    def run(self, value):\n        if value:\n            external()\n        return value\n",
                    ),
                    (
                        "run.rs",
                        "use crate::external;\npub fn renamed(value: i32)->i32 { opaque!(hidden); if value > 0 { external(); } value }\n",
                    ),
                ] {
                    fs::write(workspace.path().join(path), source).unwrap();
                }
            }
            "all documents" => {
                for path in prior_ids.keys() {
                    let source = fs::read_to_string(workspace.path().join(path)).unwrap();
                    let comment = if path.ends_with(".py") {
                        "# all changed\n"
                    } else {
                        "// all changed\n"
                    };
                    fs::write(workspace.path().join(path), format!("{source}{comment}")).unwrap();
                }
            }
            "add" => fs::write(
                workspace.path().join("added.py"),
                "class Added:\n    def go(self):\n        return 1\n",
            )
            .unwrap(),
            "delete" => fs::remove_file(workspace.path().join("A.java")).unwrap(),
            "rename" => fs::rename(
                workspace.path().join("added.py"),
                workspace.path().join("moved.py"),
            )
            .unwrap(),
            _ => unreachable!(),
        }
        let phases = std::sync::Mutex::new(Vec::new());
        let next = IndexJobCoordinator::prepare_with_session(&store, Some(latest), session.clone())
            .unwrap()
            .run(&options, &cancel, |progress| {
                phases.lock().unwrap().push(progress.phase)
            })
            .unwrap_or_else(|error| panic!("{stage}: {error:#}"));
        let phases = phases.into_inner().unwrap();
        assert!(
            phases.contains(&"mode:local".to_owned()),
            "{stage}: {phases:?}"
        );
        let db = rusqlite::Connection::open(&index).unwrap();
        let stamp: i64 = db
            .query_row(
                "SELECT superseded_at FROM native_revision_supersessions WHERE revision_id=?1",
                [format!(
                    "pin:v1:{}:{}",
                    latest.index_generation, latest.index_revision
                )],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            stamp > 0,
            "{stage}: local publication stamps its predecessor"
        );
        drop(db);
        let next_ids = selected_document_ids(state.path(), next);
        for (path, (hash, vid, gid)) in prior_ids {
            if let Some((next_hash, next_vid, next_gid)) = next_ids.get(&path)
                && hash == *next_hash
            {
                assert_eq!(
                    (vid, gid),
                    (*next_vid, *next_gid),
                    "{stage}: unchanged native/graph {path}"
                );
            }
        }
        match stage {
            "add" => assert!(next_ids.contains_key("added.py")),
            "delete" => assert!(!next_ids.contains_key("A.java")),
            "rename" => {
                assert!(!next_ids.contains_key("added.py"));
                assert!(next_ids.contains_key("moved.py"));
            }
            _ => {}
        }
        assert_selected_matches_cold(state.path(), workspace.path(), &store, next);
        #[cfg(feature = "test-support")]
        {
            assert_selected_native_matches(&store, first, &old_families);
        }
        assert_eq!(
            sqlite_snapshot(&index, first, false),
            old_sql,
            "{stage}: retained SQL"
        );
        assert_eq!(store.graph_at(Some(first)).unwrap(), old_graph);
        assert_eq!(
            store.source_at("local.js", Some(first)).unwrap().unwrap().1,
            old_source
        );
        assert_eq!(
            serde_json::to_value(store.classes_at(None, "", Some(first), 0, 100).unwrap()).unwrap(),
            old_classes
        );
        assert_eq!(
            serde_json::to_value(
                store
                    .native_declarations_at(first, "javascript", "local")
                    .unwrap()
            )
            .unwrap(),
            old_native
        );
        latest = next;
    }
}

#[test]
fn local_partial_coverage_transitions_match_independent_cold_publications() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{index_coordinator::IndexJobCoordinator, indexer::IndexOptions};
    let (state, workspace) = fixture();
    for (path, source) in [
        ("a.js", "function a() { return 1; }\n"),
        ("b.js", "function b() { return 2; }\n"),
        ("unchanged.js", "function unchanged() { return ( ; }\n"),
        (
            "gap.rs",
            "fn gap(value: i32) -> i32 { opaque!(hidden); value }\n",
        ),
    ] {
        fs::write(workspace.path().join(path), source).unwrap();
    }
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = job.session();
    let mut latest = job.run(&options, &cancel, |_| {}).unwrap();
    #[cfg(feature = "test-support")]
    let old_families = {
        let measured = measure_native_for_parity(workspace.path(), &store);
        assert_native_reference_and_gap_fixture(&measured);
        measured
    };
    #[cfg(feature = "test-support")]
    let first = latest;
    assert_eq!(store.status().unwrap().stats.parse_error_files, 1);
    for (partial, errors) in [(true, 3), (false, 1)] {
        let prior_ids = selected_document_ids(state.path(), latest);
        for (path, name) in [("a.js", "a"), ("b.js", "b")] {
            let source = if partial {
                format!("function {name}() {{ return ( ; }}\n")
            } else {
                format!("function {name}(value) {{ return value; }}\n")
            };
            fs::write(workspace.path().join(path), source).unwrap();
        }
        let phases = std::sync::Mutex::new(Vec::new());
        let next = IndexJobCoordinator::prepare_with_session(&store, Some(latest), session.clone())
            .unwrap()
            .run(&options, &cancel, |progress| {
                phases.lock().unwrap().push(progress.phase)
            })
            .unwrap();
        assert!(
            phases
                .into_inner()
                .unwrap()
                .contains(&"mode:local".to_owned())
        );
        assert_eq!(store.status().unwrap().stats.parse_error_files, errors);
        assert_eq!(
            selected_document_ids(state.path(), next)["unchanged.js"],
            prior_ids["unchanged.js"]
        );
        assert_selected_matches_cold(state.path(), workspace.path(), &store, next);
        #[cfg(feature = "test-support")]
        {
            assert_selected_native_matches(&store, first, &old_families);
        }
        latest = next;
    }
}

#[test]
fn local_reuse_trusts_validated_head_and_publishes_or_fails_typed() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };
    for (family, mutation) in [
        (
            "native",
            "UPDATE native_version_declarations SET name='forged' WHERE vid=(SELECT vid FROM revision_documents WHERE path='A.java' LIMIT 1) AND name='A'",
        ),
        (
            "graph",
            "UPDATE graph_nodes SET name='forged' WHERE gid=(SELECT gid FROM revision_documents WHERE path='A.java' LIMIT 1) AND id=(SELECT id FROM graph_nodes WHERE gid=(SELECT gid FROM revision_documents WHERE path='A.java' LIMIT 1) LIMIT 1)",
        ),
        (
            "class",
            "UPDATE classes SET payload=json_set(payload,'$.qualifiedName','forged') WHERE cid=(SELECT cid FROM revision_documents WHERE path='A.java' LIMIT 1) AND id=(SELECT id FROM classes WHERE cid=(SELECT cid FROM revision_documents WHERE path='A.java' LIMIT 1) LIMIT 1)",
        ),
        (
            "file-extraction",
            "UPDATE graph_projections SET class_extraction_payload=json_set(class_extraction_payload,'$.path','forged.java') WHERE gid=(SELECT gid FROM revision_documents WHERE path='A.java' LIMIT 1)",
        ),
        (
            "manifest-language",
            "UPDATE revision_documents SET language='python' WHERE path='A.java'",
        ),
    ] {
        let (state, workspace) = fixture();
        fs::write(
            workspace.path().join("local.js"),
            "function local() { return 1; }\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("A.java"),
            "class A { int run() { return 1; } }\n",
        )
        .unwrap();
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let options = IndexOptions::new(workspace.path().to_owned());
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let job = IndexJobCoordinator::prepare(&store, None).unwrap();
        let session = job.session();
        let first = job.run(&options, &cancel, |_| {}).unwrap();
        let db = rusqlite::Connection::open(index_dir(state.path()).join("index.db")).unwrap();
        if family == "manifest-language" {
            // This selected manifest edit violates the document language FK;
            // the graph/native/class mutations above remain FK-valid.
            db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        }
        assert_eq!(
            db.execute(mutation, []).unwrap(),
            1,
            "{family}: corrupt one real row"
        );
        drop(db);
        fs::write(
            workspace.path().join("local.js"),
            "function local() { return 2; }\n",
        )
        .unwrap();
        let publication =
            IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone())
                .unwrap()
                .run(&options, &cancel, |_| {});
        match publication {
            Ok(second) => {
                // Selected reads trust what publication validated (#261).
                assert_eq!(
                    second.index_revision,
                    first.index_revision + 1,
                    "{family}: only complete publications advance the pin"
                );
            }
            Err(failure) => {
                assert!(
                    failure.to_string().contains("incompatible_index"),
                    "{family}: {failure:#}"
                );
                let db =
                    rusqlite::Connection::open(index_dir(state.path()).join("index.db")).unwrap();
                let revision: i64 = db
                    .query_row("SELECT index_revision FROM index_metadata", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                assert_eq!(
                    revision, first.index_revision as i64,
                    "{family}: precommit refusal leaves complete old pin"
                );
            }
        }
    }
}

#[test]
fn reconcile_matches_fresh_full_snapshot_after_add_edit_delete_rename_and_ignore_change() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };
    let (state, workspace) = fixture();
    for (name, source) in [
        ("keep.js", "function keep() { oldCall(); }\n"),
        ("delete.js", "function deleted() {}\n"),
        ("rename-old.js", "function renamed() {}\n"),
        ("ignored.js", "function admittedAfterRuleChange() {}\n"),
        (
            "Witness.java",
            "class Witness { void retained() { measured(); } }\n",
        ),
    ] {
        fs::write(workspace.path().join(name), source).unwrap();
    }
    fs::write(workspace.path().join(".gitignore"), "ignored.js\n").unwrap();
    fs::write(
        workspace.path().join("package.json"),
        "{\"name\":\"before\"}\n",
    )
    .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(workspace.path().to_owned());
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let first_job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let writer_session = first_job.session();
    let first = first_job.run(&options, &cancel, |_| {}).unwrap();
    let reconciled_path = index_dir(state.path()).join("index.db");
    let retained_first = sqlite_snapshot(&reconciled_path, first, false);
    for table in [
        "document_versions",
        "graph_projections",
        "class_projections",
        "native_version_declarations",
        "graph_nodes",
        "classes",
    ] {
        assert!(
            !retained_first
                .iter()
                .find(|(name, _)| name == table)
                .unwrap()
                .1
                .is_empty(),
            "{table} fixture must be nonempty"
        );
    }

    fs::write(
        workspace.path().join("keep.js"),
        "function keep() { newCall(); }\n",
    )
    .unwrap();
    fs::remove_file(workspace.path().join("delete.js")).unwrap();
    fs::rename(
        workspace.path().join("rename-old.js"),
        workspace.path().join("rename-new.js"),
    )
    .unwrap();
    fs::write(workspace.path().join("added.rs"), "fn added() {}\n").unwrap();
    fs::write(workspace.path().join(".gitignore"), "delete.js\n").unwrap();
    fs::write(
        workspace.path().join("package.json"),
        "{\"name\":\"after!\"}\n",
    )
    .unwrap();
    let second =
        IndexJobCoordinator::prepare_with_session(&store, Some(first), writer_session.clone())
            .unwrap()
            .run(&options, &cancel, |_| {})
            .unwrap();
    assert_eq!(second.index_generation, first.index_generation);
    assert_eq!(second.index_revision, first.index_revision + 1);
    assert_eq!(store.status().unwrap().revision, second);
    assert_eq!(
        sqlite_snapshot(&reconciled_path, first, false),
        retained_first,
        "r1 full evidence must remain immutable after r2"
    );
    let r2 = sqlite_snapshot(&reconciled_path, second, true);
    let paths = |snapshot: &Vec<(String, Vec<Vec<String>>)>| -> std::collections::BTreeSet<String> {
        snapshot
            .iter()
            .find(|(table, _)| table == "revision_documents")
            .unwrap()
            .1
            .iter()
            .map(|row| row[2].clone())
            .collect()
    };
    let first_paths = paths(&retained_first);
    let second_paths = paths(&r2);
    assert!(first_paths.contains("t:delete.js") && first_paths.contains("t:rename-old.js"));
    assert!(
        second_paths.contains("t:added.rs")
            && second_paths.contains("t:rename-new.js")
            && second_paths.contains("t:ignored.js")
            && !second_paths.contains("t:delete.js")
            && !second_paths.contains("t:rename-old.js")
    );
    let db = rusqlite::Connection::open(&reconciled_path).unwrap();
    let headers: i64 = db
        .query_row("SELECT count(*) FROM native_revisions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(headers, 2, "r1 and r2 headers survive");
    for pin in [first, second] {
        let id = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
        for table in ["revision_documents", "revision_capture_inputs"] {
            let count: i64 = db
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE revision_id=?1"),
                    [&id],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(count > 0, "{table}: complete {id} evidence must remain");
        }
    }
    assert_eq!(
        store
            .source_at("delete.js", Some(first))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function deleted() {}\n"
    );
    assert!(
        store
            .source_at("delete.js", Some(second))
            .unwrap()
            .is_none()
    );
    drop(db);

    let fresh_state = tempfile::tempdir().unwrap();
    fs::set_permissions(fresh_state.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let fresh = Store::open_for_tests(fresh_state.path(), workspace.path()).unwrap();
    let cold_pin = IndexJobCoordinator::prepare(&fresh, None)
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    let fresh_path = index_dir(fresh_state.path()).join("index.db");
    assert_eq!(
        r2,
        sqlite_snapshot(&fresh_path, cold_pin, true),
        "current r2 must equal a full independent cold build, including every selected v14 evidence table"
    );
    let metadata = |path: &Path| {
        let db = rusqlite::Connection::open(path).unwrap();
        db.query_row(
            "SELECT reconcile_options,stats,diagnostics FROM index_metadata",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap()
    };
    assert_eq!(metadata(&reconciled_path), metadata(&fresh_path));
}

#[cfg(unix)]
#[test]
fn full_scan_detects_same_size_preserved_mtime_edit_through_persisted_ctime() {
    use std::{
        os::unix::{ffi::OsStrExt, fs::MetadataExt},
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };
    let (state, workspace) = fixture();
    let source = workspace.path().join("same.js");
    fs::write(&source, "function same() { aa(); }\n").unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(workspace.path().to_owned());
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = job.session();
    let first = job.run(&options, &cancel, |_| {}).unwrap();
    let path = index_dir(state.path()).join("index.db");
    let read_stat = || {
        let db = rusqlite::Connection::open(&path).unwrap();
        let payload: String = db
            .query_row(
                "SELECT capture_stat FROM revision_documents WHERE revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1) AND path='same.js'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str::<serde_json::Value>(&payload).unwrap()
    };
    let before = read_stat();
    let metadata = fs::metadata(&source).unwrap();
    fs::write(&source, "function same() { bb(); }\n").unwrap();
    let path_bytes = source.as_os_str().as_bytes();
    let c_path = std::ffi::CString::new(path_bytes).unwrap();
    let times = [
        libc::timespec {
            tv_sec: metadata.atime(),
            tv_nsec: metadata.atime_nsec(),
        },
        libc::timespec {
            tv_sec: metadata.mtime(),
            tv_nsec: metadata.mtime_nsec(),
        },
    ];
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) },
        0
    );
    let second = IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone())
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    let after = read_stat();
    assert_eq!(before["size"], after["size"]);
    assert_eq!(before["mtimeSeconds"], after["mtimeSeconds"]);
    assert_eq!(before["mtimeNanoseconds"], after["mtimeNanoseconds"]);
    assert_ne!(
        (
            before["ctimeSeconds"].clone(),
            before["ctimeNanoseconds"].clone()
        ),
        (
            after["ctimeSeconds"].clone(),
            after["ctimeNanoseconds"].clone()
        )
    );
    assert_eq!(second.index_generation, first.index_generation);
    assert_eq!(second.index_revision, first.index_revision + 1);
    assert!(
        store
            .source_at("same.js", Some(second))
            .unwrap()
            .unwrap()
            .1
            .text
            .contains("bb()")
    );
}

#[cfg(feature = "test-support")]
#[test]
fn extractor_and_typed_mismatch_rebuild_in_place_and_failed_rebuild_stays_closed() {
    use std::{
        os::unix::fs::MetadataExt,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };
    let (state, workspace) = fixture();
    fs::write(workspace.path().join("one.js"), "function one() {}\n").unwrap();
    fs::write(workspace.path().join("good.js"), "function good() {}\n").unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = job.session();
    let first = job.run(&options, &cancel, |_| {}).unwrap();
    let path = index_dir(state.path()).join("index.db");
    let inode = fs::metadata(&path).unwrap().ino();
    drop(store);

    let db = rusqlite::Connection::open(&path).unwrap();
    assert!(
        db.execute(
            "UPDATE index_metadata SET extractor_version='obsolete-extractor'",
            []
        )
        .is_err(),
        "v9 metadata must physically forbid unsupported extractor markers"
    );
    db.execute("UPDATE index_metadata SET stats='not-json'", [])
        .unwrap();
    drop(db);
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    let rebuilt = IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone())
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    assert_eq!(rebuilt.index_revision, 1);
    assert_ne!(rebuilt.index_generation, first.index_generation);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    drop(store);

    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE document_versions SET source_bytes=x'FF',byte_length=1 WHERE vid=(SELECT vid FROM revision_documents WHERE path='one.js')",
        [],
    )
    .unwrap();
    drop(db);
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let clone = store.clone();
    let separate = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    assert_eq!(store.status().unwrap().revision, rebuilt);
    assert!(
        store
            .source_at("missing.js", Some(rebuilt))
            .unwrap()
            .is_none()
    );
    let wrong = trellis::model::IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        index_revision: rebuilt.index_revision,
    };
    assert!(store.source_at("good.js", Some(wrong)).is_err());
    assert_eq!(store.status().unwrap().revision, rebuilt);
    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let busy = store.source_at("good.js", Some(rebuilt)).unwrap_err();
    assert!(busy.to_string().contains("storage_busy"), "{busy:#}");
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(store.status().unwrap().revision, rebuilt);
    assert!(store.source_at("good.js", Some(rebuilt)).unwrap().is_some());
    assert_eq!(separate.status().unwrap().revision, rebuilt);
    assert!(
        separate
            .source_at("good.js", Some(rebuilt))
            .unwrap()
            .is_some()
    );
    let selected = store.source_at("one.js", Some(rebuilt)).unwrap_err();
    assert!(
        selected.to_string().contains("incompatible_index"),
        "{selected:#}"
    );
    assert!(store.status().is_err());
    assert!(clone.status().is_err());
    assert_eq!(separate.status().unwrap().revision, rebuilt);
    assert!(separate.source_at("one.js", Some(rebuilt)).is_err());
    assert!(separate.status().is_err());
    drop(separate);

    let failed_cancel: CancelFlag = Arc::new(AtomicBool::new(true));
    let failure = IndexJobCoordinator::prepare_with_session(&store, Some(rebuilt), session.clone())
        .unwrap()
        .run(&options, &failed_cancel, |_| {})
        .unwrap_err();
    assert!(failure.to_string().contains("cancelled"));
    let db = rusqlite::Connection::open(&path).unwrap();
    let malformed: String = db
        .query_row("SELECT hex(source_bytes) FROM document_versions WHERE vid=(SELECT vid FROM revision_documents WHERE path='one.js')", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(malformed, "FF");
    drop(db);
    assert!(store.status().is_err());
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    failed_cancel.store(false, Ordering::Release);
    let mut recovered =
        IndexJobCoordinator::prepare_with_session(&store, Some(rebuilt), session.clone())
            .unwrap()
            .run(&options, &failed_cancel, |_| {})
            .unwrap();
    assert_eq!(recovered.index_revision, 1);
    assert_ne!(recovered.index_generation, rebuilt.index_generation);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(store.status().unwrap().revision, recovered);

    // A SQLite type mismatch in any normalized derived row is replaceable
    // derived corruption, not a hard Store-open failure.
    drop(store);
    let db = rusqlite::Connection::open(&path).unwrap();
    let (syntax_id, lookup_key): (String, String) = db
        .query_row(
            "SELECT 'sid:v1:'||lower(hex(d.syntax_id)),d.lookup_key FROM native_version_declarations d JOIN document_versions v ON v.vid=d.vid WHERE d.lookup_key IS NOT NULL AND v.language='javascript' ORDER BY d.syntax_id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        db.execute(
            "UPDATE native_version_declarations SET key_ordinal=CAST(key_ordinal + 0.5 AS REAL) WHERE syntax_id=unhex(substr(?1,8))",
            [&syntax_id],
        )
        .unwrap(),
        1
    );
    drop(db);
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    assert_eq!(store.status().unwrap().revision, recovered);
    let type_error = store
        .native_declarations_at(recovered, "javascript", &lookup_key)
        .unwrap_err();
    assert!(
        matches!(
            type_error.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::InvalidColumnType(..))
        ),
        "{type_error:#}"
    );
    #[cfg(feature = "test-support")]
    assert!(store.recovery_pending_for_tests());
    assert!(store.status().is_err());
    failed_cancel.store(true, Ordering::Release);
    let failure =
        IndexJobCoordinator::prepare_with_session(&store, Some(recovered), session.clone())
            .unwrap()
            .run(&options, &failed_cancel, |_| {})
            .unwrap_err();
    assert!(failure.to_string().contains("cancelled"), "{failure:#}");
    let db = rusqlite::Connection::open(&path).unwrap();
    let stored_type: String = db
        .query_row(
            "SELECT typeof(key_ordinal) FROM native_version_declarations WHERE syntax_id=unhex(substr(?1,8))",
            [&syntax_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_type, "real");
    drop(db);
    assert!(store.status().is_err());
    failed_cancel.store(false, Ordering::Release);
    let typed_recovered =
        IndexJobCoordinator::prepare_with_session(&store, Some(recovered), session.clone())
            .unwrap()
            .run(&options, &failed_cancel, |_| {})
            .unwrap();
    assert_eq!(typed_recovered.index_revision, 1);
    assert_ne!(typed_recovered.index_generation, recovered.index_generation);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    recovered = typed_recovered;
    assert_eq!(store.status().unwrap().revision, recovered);

    // A newly acquired writer must complete startup reconciliation before it can serve reads.
    // The latch is shared by Store clones and remains closed after the guard drops.
    let clone = store.clone();
    drop(session);
    drop(store.writer_session_for_tests().unwrap());
    assert!(store.status().is_err());
    assert!(clone.status().is_err());
    // A separately opened Store performs its own full recovery admission.
    let separate = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    assert_eq!(separate.index_baseline().unwrap(), recovered);
    drop(separate);
    let job = IndexJobCoordinator::prepare(&clone, Some(recovered)).unwrap();
    let _next_session = job.session();
    let next = job.run(&options, &failed_cancel, |_| {}).unwrap();
    assert_eq!(clone.status().unwrap().revision, next);
    assert_eq!(store.status().unwrap().revision, next);
}

#[test]
fn bounded_control_decode_rebuilds_and_failed_attempt_stays_closed() {
    use std::{
        os::unix::fs::MetadataExt,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };
    let (state, workspace) = fixture();
    fs::write(workspace.path().join("one.js"), "function one() {}\n").unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = job.session();
    let pin = job.run(&options, &cancel, |_| {}).unwrap();
    let path = index_dir(state.path()).join("index.db");
    let inode = fs::metadata(&path).unwrap().ino();
    drop(store);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE index_metadata SET stats='not-json'", [])
        .unwrap();
    drop(db);

    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    cancel.store(true, Ordering::Release);
    let failure = IndexJobCoordinator::prepare_with_session(&store, Some(pin), session.clone())
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap_err();
    assert!(failure.to_string().contains("cancelled"), "{failure:#}");
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    let db = rusqlite::Connection::open(&path).unwrap();
    let stats: String = db
        .query_row("SELECT stats FROM index_metadata", [], |row| row.get(0))
        .unwrap();
    assert_eq!(stats, "not-json");
    drop(db);
    assert!(store.status().is_err());

    cancel.store(false, Ordering::Release);
    let repaired = IndexJobCoordinator::prepare_with_session(&store, Some(pin), session.clone())
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    assert_eq!(repaired.index_revision, 1);
    assert_ne!(repaired.index_generation, pin.index_generation);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(store.status().unwrap().revision, repaired);
}

#[test]
fn exceptional_format_is_typed_deferred_refusal_without_file_mutation() {
    use std::{
        io::{Seek, SeekFrom, Write},
        os::unix::fs::MetadataExt,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };
    let (state, workspace) = fixture();
    fs::write(workspace.path().join("one.js"), "function one() {}\n").unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    IndexJobCoordinator::prepare(&store, None)
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    let path = index_dir(state.path()).join("index.db");
    drop(store);
    let inode = fs::metadata(&path).unwrap().ino();
    let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(b"not sqlite format").unwrap();
    file.sync_all().unwrap();
    drop(file);
    let corrupt = fs::read(&path).unwrap();

    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    #[allow(unused_mut)] // Test-only writer refusal is appended under test-support.
    let mut refusals = vec![
        store.status().unwrap_err(),
        store.index_baseline().unwrap_err(),
    ];
    #[cfg(feature = "test-support")]
    refusals.push(
        store
            .publication_for_tests()
            .expect_err("exceptional format writer refusal"),
    );
    for refusal in refusals {
        let text = refusal.to_string();
        assert!(
            text.contains("exceptional index recovery deferred"),
            "{refusal:#}"
        );
    }
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(fs::read(&path).unwrap(), corrupt);
}

#[test]
fn current_v8_inventory_validation_rejects_missing_unknown_and_unsupported_state() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };
    for mutation in [
        "DELETE FROM revision_capture_inputs WHERE input_key='config:package.json'",
        "INSERT INTO revision_capture_inputs(revision_id,input_key,payload) SELECT id,'unknown:slot','{\"state\":\"absent\"}' FROM native_revisions WHERE published_index_revision=(SELECT index_revision FROM index_metadata)",
        "UPDATE index_metadata SET reconcile_options=json_set(reconcile_options,'$.version',3)",
        "UPDATE revision_capture_inputs SET payload=json_set(payload,'$.hash','invalid-digest') WHERE input_key='config:package.json'",
    ] {
        let (state, workspace) = fixture();
        fs::write(workspace.path().join("one.js"), "function one() {}\n").unwrap();
        fs::write(
            workspace.path().join("package.json"),
            b"{\"name\":\"capture-fixture\"}",
        )
        .unwrap();
        let scip = workspace.path().join("labels.scip");
        let manifest = workspace.path().join("labels.json");
        fs::write(&scip, b"captured presentation bytes").unwrap();
        fs::write(&manifest, b"{}").unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let job = IndexJobCoordinator::prepare(&store, None).unwrap();
        let session = job.session();
        let pin = job.run(&options, &cancel, |_| {}).unwrap();
        let path = index_dir(state.path()).join("index.db");
        drop(store);
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(mutation).unwrap();
        assert!(
            db.changes() == 1,
            "inventory mutation must touch exactly one row: {mutation}"
        );
        drop(db);
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        assert!(
            store.status().is_err(),
            "invalid inventory was publicly readable: {mutation}"
        );
        let repaired =
            IndexJobCoordinator::prepare_with_session(&store, Some(pin), session.clone())
                .unwrap()
                .run(&options, &cancel, |_| {})
                .unwrap();
        assert_eq!(repaired.index_revision, 1);
        assert_ne!(repaired.index_generation, pin.index_generation);
        assert_eq!(store.status().unwrap().revision, repaired);
        let db = rusqlite::Connection::open(&path).unwrap();
        let keys = db
            .prepare("SELECT input_key FROM revision_capture_inputs ORDER BY input_key")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(!keys.iter().any(|key| key.starts_with("presentation-")));
        assert!(keys.contains(&"config:package.json".to_owned()));
        assert!(!keys.iter().any(|key| key.starts_with("unknown:")));
    }
}

#[test]
fn selected_projection_corruption_latches_only_selected_reads() {
    // A selected source that will not decode fails typed and latches a rebuild.
    let (state, _workspace, store, pin, _session) = projection_fixture();
    let clone = store.clone();
    let db = rusqlite::Connection::open(index_dir(state.path()).join("index.db")).unwrap();
    db.execute(
        "UPDATE document_versions SET source_bytes=x'FF',byte_length=1 WHERE vid=(SELECT vid FROM revision_documents WHERE path='flow.js')",
        [],
    )
    .unwrap();
    let error = store.methods_at("flow.js", Some(pin)).unwrap_err();
    assert!(
        error.to_string().contains("incompatible_index"),
        "{error:#}"
    );
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );

    // The selected source's bytes remain the same, but storing them as TEXT
    // instead of BLOB must produce a typed conversion refusal. No FK or CHECK
    // is disabled: the source hash and declared byte length still match.
    let (state, _workspace, store, pin, _session) = projection_fixture();
    let clone = store.clone();
    let db = rusqlite::Connection::open(index_dir(state.path()).join("index.db")).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE document_versions SET source_bytes=CAST(source_bytes AS TEXT) WHERE vid=(SELECT vid FROM revision_documents WHERE path='flow.js')",
            [],
        )
        .unwrap(),
        1
    );
    let storage_class: String = db.query_row(
        "SELECT typeof(source_bytes) FROM document_versions WHERE vid=(SELECT vid FROM revision_documents WHERE path='flow.js')",
        [],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(storage_class, "text");
    let error = store.methods_at("flow.js", Some(pin)).unwrap_err();
    assert!(error.is::<rusqlite::types::FromSqlError>(), "{error:#}");
    #[cfg(feature = "test-support")]
    assert!(store.recovery_pending_for_tests());
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );

    // Tree enrichment counts narrow node kinds for its visible file.
    let (state, workspace, store, pin, _session) = projection_fixture();
    let db = rusqlite::Connection::open(index_dir(state.path()).join("index.db")).unwrap();
    let expected: i64 = db
        .query_row(
            "SELECT count(*) FROM graph_nodes WHERE gid=(SELECT gid FROM revision_documents WHERE path='flow.js') AND kind IN ('function','method')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(expected > 0);
    let mut items = vec![trellis::file_tree::Entry {
        name: "flow.js".into(),
        path: "flow.js".into(),
        kind: "file",
        indexed_path: None,
        method_count: None,
        unindexed_reason: None,
    }];
    let tree_root = workspace.path().canonicalize().unwrap();
    assert_eq!(store.tree_metadata(&tree_root, &mut items).unwrap().0, pin);
    assert_eq!(items[0].method_count, Some(expected as usize));
    assert_eq!(items[0].indexed_path.as_deref(), Some("flow.js"));

    // Benign absence and a bad pin remain non-latching request outcomes.
    let (_state, _workspace, store, pin, _session) = projection_fixture();
    let clone = store.clone();
    assert!(store.methods_at("missing.js", Some(pin)).unwrap().is_none());
    let wrong = trellis::model::IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        index_revision: pin.index_revision,
    };
    assert!(
        store
            .files_at(Some(wrong), 0, 10)
            .unwrap_err()
            .to_string()
            .contains("revision conflict")
    );
    assert_eq!(store.status().unwrap().revision, pin);
    assert_eq!(clone.status().unwrap().revision, pin);
}

#[test]
fn selected_projection_rebuild_is_same_inode_and_clears_clones_only_after_commit() {
    use std::os::unix::fs::MetadataExt;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };

    let (state, workspace, store, pin, session) = projection_fixture();
    let clone = store.clone();
    let path = index_dir(state.path()).join("index.db");
    let inode = fs::metadata(&path).unwrap().ino();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE document_versions SET source_bytes=x'FF',byte_length=1 WHERE vid=(SELECT vid FROM revision_documents WHERE path='flow.js')",
        [],
    )
    .unwrap();
    drop(db);
    let error = store.methods_at("flow.js", Some(pin)).unwrap_err();
    assert!(
        error.to_string().contains("incompatible_index"),
        "{error:#}"
    );
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );

    let cancel: CancelFlag = Arc::new(AtomicBool::new(true));
    let failed = IndexJobCoordinator::prepare_with_session(&store, Some(pin), session.clone())
        .unwrap()
        .run(
            &IndexOptions::new(workspace.path().to_owned()),
            &cancel,
            |_| {},
        )
        .unwrap_err();
    assert!(failed.to_string().contains("cancelled"), "{failed:#}");
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);

    cancel.store(false, Ordering::Release);
    let recovered = IndexJobCoordinator::prepare_with_session(&store, Some(pin), session.clone())
        .unwrap()
        .run(
            &IndexOptions::new(workspace.path().to_owned()),
            &cancel,
            |_| {},
        )
        .unwrap();
    assert_eq!(recovered.index_revision, 1);
    assert_ne!(recovered.index_generation, pin.index_generation);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(store.status().unwrap().revision, recovered);
    assert_eq!(clone.status().unwrap().revision, recovered);
    assert_eq!(
        store.files_at(Some(recovered), 0, 10).unwrap()["items"][0]["path"],
        "flow.js"
    );
}

#[test]
fn metadata_real_revision_uses_private_witness_and_recovers_same_inode() {
    use rusqlite::types::ValueRef;
    use std::{
        os::unix::fs::MetadataExt,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };

    let (state, workspace, initial, old, session) = projection_fixture();
    let path = index_dir(state.path()).join("index.db");
    let inode = fs::metadata(&path).unwrap().ino();
    drop(initial);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE index_metadata SET index_revision=CAST(1.5 AS REAL)",
        [],
    )
    .unwrap();
    drop(db);

    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let clone = store.clone();
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert!(
        store
            .index_baseline()
            .unwrap_err()
            .to_string()
            .contains("not decodable")
    );
    assert!(
        IndexJobCoordinator::prepare_with_session(&store, Some(old), session.clone())
            .err()
            .expect("undecodable pin must conflict")
            .to_string()
            .contains("revision conflict")
    );

    let cancel: CancelFlag = Arc::new(AtomicBool::new(true));
    let error = IndexJobCoordinator::prepare_with_session(&store, None, session.clone())
        .unwrap()
        .run(
            &IndexOptions::new(workspace.path().to_owned()),
            &cancel,
            |_| {},
        )
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    let db = rusqlite::Connection::open(&path).unwrap();
    let bits = db
        .query_row(
            "SELECT index_revision FROM index_metadata",
            [],
            |row| match row.get_ref(0)? {
                ValueRef::Real(value) => Ok(value.to_bits()),
                _ => Err(rusqlite::Error::InvalidQuery),
            },
        )
        .unwrap();
    assert_eq!(bits, 1.5f64.to_bits());
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    drop(db);
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );

    cancel.store(false, Ordering::Release);
    let recovered = IndexJobCoordinator::prepare_with_session(&store, None, session.clone())
        .unwrap()
        .run(
            &IndexOptions::new(workspace.path().to_owned()),
            &cancel,
            |_| {},
        )
        .unwrap();
    assert_eq!(recovered.index_revision, 1);
    assert_ne!(recovered.index_generation, old.index_generation);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(store.status().unwrap().revision, recovered);
    assert_eq!(clone.status().unwrap().revision, recovered);
}

#[test]
fn private_metadata_witness_distinguishes_nul_real_and_multichunk_blob() {
    use rusqlite::MAIN_DB;
    use std::{
        os::unix::fs::MetadataExt,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    use trellis::{index_coordinator::IndexJobCoordinator, indexer::IndexOptions};

    let conflict = |first: &str, mutate: &dyn Fn(&rusqlite::Connection)| {
        let (state, workspace, initial, _old, session) = projection_fixture();
        let path = index_dir(state.path()).join("index.db");
        drop(initial);
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute(first, []).unwrap();
        drop(db);
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let coordinator =
            IndexJobCoordinator::prepare_with_session(&store, None, session.clone()).unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        mutate(&db);
        drop(db);
        let error = coordinator
            .run(
                &IndexOptions::new(workspace.path().to_owned()),
                &Arc::new(AtomicBool::new(false)),
                |_| {},
            )
            .unwrap_err();
        assert!(error.to_string().contains("revision conflict"), "{error:#}");
        assert!(
            store
                .status()
                .unwrap_err()
                .to_string()
                .contains("incompatible_index")
        );
    };

    conflict(
        "UPDATE index_metadata SET index_generation='bad' || char(0) || 'one'",
        &|db| {
            db.execute(
                "UPDATE index_metadata SET index_generation='bad' || char(0) || 'two'",
                [],
            )
            .unwrap();
        },
    );
    conflict(
        "UPDATE index_metadata SET index_revision=CAST(1.5 AS REAL)",
        &|db| {
            db.execute(
                "UPDATE index_metadata SET index_revision=?1",
                [f64::from_bits(1.5f64.to_bits() + 1)],
            )
            .unwrap();
        },
    );

    let (state, workspace, initial, _old, session) = projection_fixture();
    let path = index_dir(state.path()).join("index.db");
    drop(initial);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE index_metadata SET index_generation=zeroblob(131073)",
        [],
    )
    .unwrap();
    drop(db);
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let coordinator =
        IndexJobCoordinator::prepare_with_session(&store, None, session.clone()).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    let mut blob = db
        .blob_open(MAIN_DB, "index_metadata", "index_generation", 1, false)
        .unwrap();
    blob.write_at(&[1], 70_000).unwrap();
    blob.close().unwrap();
    drop(db);
    let error = coordinator
        .run(
            &IndexOptions::new(workspace.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap_err();
    assert!(error.to_string().contains("revision conflict"), "{error:#}");
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    let inode = fs::metadata(&path).unwrap().ino();
    let cancel = Arc::new(AtomicBool::new(true));
    let error = IndexJobCoordinator::prepare_with_session(&store, None, session.clone())
        .unwrap()
        .run(
            &IndexOptions::new(workspace.path().to_owned()),
            &cancel,
            |_| {},
        )
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    cancel.store(false, Ordering::Release);
    let recovered = IndexJobCoordinator::prepare_with_session(&store, None, session.clone())
        .unwrap()
        .run(
            &IndexOptions::new(workspace.path().to_owned()),
            &cancel,
            |_| {},
        )
        .unwrap();
    assert_eq!(recovered.index_revision, 1);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(store.status().unwrap().revision, recovered);

    let (state, workspace, initial, _old, _session) = projection_fixture();
    let path = index_dir(state.path()).join("index.db");
    drop(initial);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE index_metadata SET root_spelling='wrong' || char(0) || 'root',index_revision=CAST(1.5 AS REAL)", []).unwrap();
    drop(db);
    let error = Store::open_for_tests(state.path(), workspace.path()).unwrap_err();
    assert!(
        error.to_string().contains("root_key_collision"),
        "{error:#}"
    );

    let (state, workspace, initial, _old, _session) = projection_fixture();
    let path = index_dir(state.path()).join("index.db");
    drop(initial);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE index_metadata SET root_spelling=CAST(zeroblob(131073) AS TEXT),index_revision=CAST(1.5 AS REAL)", []).unwrap();
    drop(db);
    let error = Store::open_for_tests(state.path(), workspace.path()).unwrap_err();
    assert!(
        error.to_string().contains("root_key_collision"),
        "{error:#}"
    );

    let (state, workspace, initial, _old, _session) = projection_fixture();
    let path = index_dir(state.path()).join("index.db");
    drop(initial);
    let locker = rusqlite::Connection::open(&path).unwrap();
    locker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let error = Store::open_for_tests(state.path(), workspace.path()).unwrap_err();
    assert!(error.to_string().contains("storage_busy"), "{error:#}");
    locker.execute_batch("ROLLBACK").unwrap();
    assert!(Store::open_for_tests(state.path(), workspace.path()).is_ok());
}

#[test]
fn live_control_decode_corruption_latches_direct_reads_and_control_clones() {
    // Malformed stats must be reported by a direct pinned read, not only status.
    let (state, _workspace, store, pin, _session) = projection_fixture();
    let clone = store.clone();
    let path = index_dir(state.path()).join("index.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE index_metadata SET stats='not-json'", [])
        .unwrap();
    drop(db);
    let error = store.source_at("flow.js", Some(pin)).unwrap_err();
    assert!(
        error.to_string().starts_with("incompatible_index:"),
        "{error:#}"
    );
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert!(
        store
            .files_at(None, 0, 10)
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert!(
        store
            .source_at("flow.js", None)
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert!(
        store
            .symbols_at("go", 10)
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );

    // REAL revision must fail before pin comparison and close every clone.
    let (state, _workspace, store, pin, _session) = projection_fixture();
    let clone = store.clone();
    let symbol = store.symbols_at("go", 10).unwrap().1.remove(0).id;
    let path = index_dir(state.path()).join("index.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE index_metadata SET index_revision=CAST(1.5 AS REAL)",
        [],
    )
    .unwrap();
    drop(db);
    let error = store.symbol_at(&symbol, Some(pin)).unwrap_err();
    assert!(
        matches!(
            error.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::InvalidColumnType(..))
        ),
        "{error:#}"
    );
    #[cfg(feature = "test-support")]
    assert!(store.recovery_pending_for_tests());
    assert!(
        clone
            .files_at(None, 0, 10)
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );

    // Malformed persisted input is reached by the existing full-read inventory pass.
    let (state, _workspace, store, pin, _session) = projection_fixture();
    let clone = store.clone();
    let path = index_dir(state.path()).join("index.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE revision_capture_inputs SET payload='not-json' WHERE input_key='root:.'",
        [],
    )
    .unwrap();
    drop(db);
    let error = store.classes_at(None, "", Some(pin), 0, 10).unwrap_err();
    assert!(error.is::<serde_json::Error>(), "{error:#}");
    #[cfg(feature = "test-support")]
    assert!(store.recovery_pending_for_tests());
    assert!(
        clone
            .symbols_at("go", 10)
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
}

#[test]
fn live_structural_inventory_and_control_first_reads_fail_closed() {
    // This is an already executed structural check, not a new derived invariant.
    let (state, _workspace, store, pin, _session) = projection_fixture();
    let clone = store.clone();
    let path = index_dir(state.path()).join("index.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE revision_capture_inputs SET input_key='unexpected:role' WHERE input_key='root:.'",
        [],
    )
    .unwrap();
    drop(db);
    let error = store.source_at("flow.js", Some(pin)).unwrap_err();
    assert!(
        error.to_string().starts_with("incompatible_index:"),
        "{error:#}"
    );
    assert!(
        clone
            .files_at(None, 0, 10)
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert!(
        clone
            .source_at("flow.js", None)
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    assert!(
        clone
            .symbols_at("go", 10)
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );

    // A control-only read can also be the first observer of typed control corruption.
    let (state, _workspace, store, _pin, _session) = projection_fixture();
    let clone = store.clone();
    let path = index_dir(state.path()).join("index.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE index_metadata SET stats='not-json'", [])
        .unwrap();
    drop(db);
    let error = store.status().unwrap_err();
    assert!(
        error.to_string().starts_with("incompatible_index:"),
        "{error:#}"
    );
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
}

#[test]
fn live_root_mismatch_precedes_typed_control_corruption_without_latching() {
    let (state, _workspace, store, pin, _session) = projection_fixture();
    let clone = store.clone();
    let path = index_dir(state.path()).join("index.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    let original_root: String = db
        .query_row("SELECT root_spelling FROM index_metadata", [], |row| {
            row.get(0)
        })
        .unwrap();
    db.execute(
        "UPDATE index_metadata SET root_spelling='wrong' || char(0) || 'root',index_revision=CAST(1.5 AS REAL)",
        [],
    )
    .unwrap();
    drop(db);
    let error = store.source_at("flow.js", Some(pin)).unwrap_err();
    assert!(
        error.to_string().starts_with("root_key_collision:"),
        "{error:#}"
    );

    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE index_metadata SET root_spelling=?1,index_revision=?2",
        rusqlite::params![original_root, pin.index_revision as i64],
    )
    .unwrap();
    drop(db);
    assert_eq!(
        clone.status().unwrap().revision,
        pin,
        "root failure must not latch"
    );
}

#[test]
fn live_stats_recovery_cancel_then_success_preserves_inode_and_rotates_pin() {
    use std::{
        os::unix::fs::MetadataExt,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };

    let (state, workspace, store, old, session) = projection_fixture();
    let clone = store.clone();
    let path = index_dir(state.path()).join("index.db");
    let inode = fs::metadata(&path).unwrap().ino();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE index_metadata SET stats='not-json'", [])
        .unwrap();
    drop(db);
    assert!(
        store
            .source_at("flow.js", Some(old))
            .unwrap_err()
            .to_string()
            .starts_with("incompatible_index:")
    );

    let cancel: CancelFlag = Arc::new(AtomicBool::new(true));
    let error = IndexJobCoordinator::prepare_with_session(&store, Some(old), session.clone())
        .unwrap()
        .run(
            &IndexOptions::new(workspace.path().to_owned()),
            &cancel,
            |_| {},
        )
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT stats FROM index_metadata", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "not-json"
    );
    drop(db);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );

    cancel.store(false, Ordering::Release);
    let recovered = IndexJobCoordinator::prepare_with_session(&store, Some(old), session.clone())
        .unwrap()
        .run(
            &IndexOptions::new(workspace.path().to_owned()),
            &cancel,
            |_| {},
        )
        .unwrap();
    assert_eq!(recovered.index_revision, 1);
    assert_ne!(recovered.index_generation, old.index_generation);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(store.status().unwrap().revision, recovered);
    assert_eq!(clone.status().unwrap().revision, recovered);
    assert!(
        IndexJobCoordinator::prepare_with_session(&store, Some(old), session.clone())
            .err()
            .expect("old pin must conflict")
            .to_string()
            .contains("revision conflict")
    );
}

#[test]
fn live_control_corruption_status_first_is_typed_and_clone_shared() {
    for sql in [
        "UPDATE index_metadata SET stats='not-json'",
        "UPDATE index_metadata SET index_revision=CAST(1.5 AS REAL)",
        "UPDATE revision_capture_inputs SET payload='not-json' WHERE input_key='root:.'",
    ] {
        let (state, _workspace, store, pin, _session) = projection_fixture();
        let clone = store.clone();
        let path = index_dir(state.path()).join("index.db");
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute(sql, []).unwrap();
        drop(db);
        let error = store.status().unwrap_err();
        if sql.contains("index_revision") {
            assert!(
                matches!(
                    error.downcast_ref::<rusqlite::Error>(),
                    Some(rusqlite::Error::InvalidColumnType(..))
                ),
                "{sql}: {error:#}"
            );
        } else if sql.contains("payload") {
            assert!(error.is::<serde_json::Error>(), "{sql}: {error:#}");
        } else {
            assert!(
                format!("{error:#}").contains("native header/control graph metadata mismatch"),
                "{sql}: {error:#}"
            );
        }
        #[cfg(feature = "test-support")]
        assert!(store.recovery_pending_for_tests(), "{sql}");
        assert!(
            clone
                .source_at("flow.js", Some(pin))
                .unwrap_err()
                .to_string()
                .contains("incompatible_index"),
            "{sql}"
        );
    }
}

#[test]
fn native_index_does_not_project_optional_scip_labels() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.js"), "function f() {}\n").unwrap();
    let options = trellis::indexer::IndexOptions::new(root.path().to_owned());
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (before, _) =
        trellis::indexer::index_workspace_with_capture(&options, &cancel, |_| {}).unwrap();
    std::fs::write(
        root.path().join("artifact.scip"),
        b"old optional artifact ignored",
    )
    .unwrap();
    let (after, _) =
        trellis::indexer::index_workspace_with_capture(&options, &cancel, |_| {}).unwrap();
    assert_eq!(before.nodes, after.nodes);
    assert!(after.nodes.iter().all(|node| node.display_label.is_none()));
}

#[cfg(feature = "test-support")]
#[test]
#[ignore = "pinned medium Cmedium0000 Java prefix insertion requires two cold indices; run explicitly"]
fn canonical_medium_java_prefix_literal_local_matches_independent_cold_and_retains_old_pin() {
    use sha2::{Digest, Sha256};
    use std::{
        process::Command,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };

    let root = tempfile::tempdir().unwrap();
    let corpus = root.path().join("frozen-canonical-v1");
    let generated = Command::new("node")
        .arg("tools/synthetic-cohorts/generate.mjs")
        .arg("--out")
        .arg(&corpus)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        generated.status.success(),
        "pinned generator: {}",
        String::from_utf8_lossy(&generated.stderr)
    );
    assert_eq!(
        hex::encode(Sha256::digest(
            fs::read(corpus.join("manifest.json")).unwrap()
        )),
        "ab4a058fac22dd9f324a476990755a3af07ab32f977f32c977d6eeea79627259"
    );
    let workspace = corpus.join("medium");
    let relative = "java/Cmedium0000.java";
    let leaf = workspace.join(relative);
    let old_bytes = fs::read(&leaf).unwrap();
    let original = b"public static int f0(){return 70000;}";
    let replacement = b"public static int f0(){return 170000;}";
    assert_eq!(
        old_bytes
            .windows(original.len())
            .filter(|window| *window == original)
            .count(),
        1
    );
    let state = tempfile::tempdir().unwrap();
    fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open_for_tests(state.path(), &workspace).unwrap();
    let options = IndexOptions::new(workspace.clone());
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let first_job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = first_job.session();
    let old_pin = first_job.run(&options, &cancel, |_| {}).unwrap();
    let old_source = store.source_at(relative, Some(old_pin)).unwrap().unwrap().1;
    let old_graph = serde_json::to_value(store.graph_at(Some(old_pin)).unwrap()).unwrap();
    let old_class = serde_json::to_value(
        store
            .classes_at(Some(relative), "", Some(old_pin), 0, 100)
            .unwrap(),
    )
    .unwrap();

    let mut edited = Vec::with_capacity(old_bytes.len() + 1);
    let start = old_bytes
        .windows(original.len())
        .position(|window| window == original)
        .unwrap();
    edited.extend_from_slice(&old_bytes[..start]);
    edited.extend_from_slice(replacement);
    edited.extend_from_slice(&old_bytes[start + original.len()..]);
    fs::write(&leaf, edited).unwrap();
    let phases = std::sync::Mutex::new(Vec::new());
    let local_pin =
        IndexJobCoordinator::prepare_with_session(&store, Some(old_pin), session.clone())
            .unwrap()
            .run(&options, &cancel, |progress| {
                phases.lock().unwrap().push(progress.phase)
            })
            .unwrap();
    assert!(
        phases
            .into_inner()
            .unwrap()
            .iter()
            .any(|phase| phase == "mode:local"),
        "canonical one-byte numeric-prefix insertion must not silently choose FULL"
    );
    assert_eq!(
        store.source_at(relative, Some(old_pin)).unwrap().unwrap().1,
        old_source
    );
    assert_eq!(
        serde_json::to_value(store.graph_at(Some(old_pin)).unwrap()).unwrap(),
        old_graph
    );
    assert_eq!(
        serde_json::to_value(
            store
                .classes_at(Some(relative), "", Some(old_pin), 0, 100)
                .unwrap()
        )
        .unwrap(),
        old_class
    );

    let cold_state = tempfile::tempdir().unwrap();
    fs::set_permissions(cold_state.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let cold = Store::open_for_tests(cold_state.path(), &workspace).unwrap();
    let cold_job = IndexJobCoordinator::prepare(&cold, None).unwrap();
    let _cold_session = cold_job.session();
    let cold_pin = cold_job.run(&options, &cancel, |_| {}).unwrap();
    assert_eq!(
        sqlite_snapshot(&index_dir(state.path()).join("index.db"), local_pin, true),
        sqlite_snapshot(
            &index_dir(cold_state.path()).join("index.db"),
            cold_pin,
            true
        ),
        "pinned medium selected native/graph/class/reference SQL must match independent same-root cold"
    );
    assert_eq!(
        serde_json::to_value(store.graph_at(Some(local_pin)).unwrap()).unwrap(),
        serde_json::to_value(cold.graph_at(Some(cold_pin)).unwrap()).unwrap()
    );
    let local_page = store
        .classes_at(Some(relative), "", Some(local_pin), 0, 100)
        .unwrap();
    let cold_page = cold
        .classes_at(Some(relative), "", Some(cold_pin), 0, 100)
        .unwrap();
    assert_eq!(local_page.revision, local_pin);
    assert_eq!(cold_page.revision, cold_pin);
    let mut local_page = serde_json::to_value(local_page).unwrap();
    let mut cold_page = serde_json::to_value(cold_page).unwrap();
    local_page.as_object_mut().unwrap().remove("revision");
    cold_page.as_object_mut().unwrap().remove("revision");
    assert_eq!(
        local_page, cold_page,
        "class page content must match the independent cold pin"
    );
    assert_eq!(
        store.source_at(relative, Some(old_pin)).unwrap().unwrap().1,
        old_source
    );
}

#[test]
fn exported_javascript_and_bare_rust_tail_local_match_independent_cold() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        index_coordinator::IndexJobCoordinator, indexer::IndexOptions, model::CancelFlag,
    };
    for (path, before, after) in [
        (
            "f0.js",
            "export function f0(){return 7000;}\n",
            "export function f0(){return 17000;}\n",
        ),
        (
            "f0.rs",
            "pub fn f0()->i32{100}\n",
            "pub fn f0()->i32{1100}\n",
        ),
    ] {
        let (state, workspace) = fixture();
        let leaf = workspace.path().join(path);
        fs::write(&leaf, before).unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let job = IndexJobCoordinator::prepare(&store, None).unwrap();
        let session = job.session();
        let old_pin = job.run(&options, &cancel, |_| {}).unwrap();
        let old_source = store.source_at(path, Some(old_pin)).unwrap().unwrap().1;
        let old_graph = serde_json::to_value(store.graph_at(Some(old_pin)).unwrap()).unwrap();
        fs::write(&leaf, after).unwrap();
        let phases = std::sync::Mutex::new(Vec::new());
        let local_pin =
            IndexJobCoordinator::prepare_with_session(&store, Some(old_pin), session.clone())
                .unwrap()
                .run(&options, &cancel, |progress| {
                    phases.lock().unwrap().push(progress.phase)
                })
                .unwrap();
        assert!(
            phases
                .into_inner()
                .unwrap()
                .iter()
                .any(|phase| phase == "mode:local"),
            "{path}: body-only exported/tail literal must be locally proved"
        );
        assert_eq!(
            store.source_at(path, Some(old_pin)).unwrap().unwrap().1,
            old_source
        );
        assert_eq!(
            serde_json::to_value(store.graph_at(Some(old_pin)).unwrap()).unwrap(),
            old_graph
        );
        let cold_state = tempfile::tempdir().unwrap();
        fs::set_permissions(cold_state.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let cold = Store::open_for_tests(cold_state.path(), workspace.path()).unwrap();
        let cold_job = IndexJobCoordinator::prepare(&cold, None).unwrap();
        let _cold_session = cold_job.session();
        let cold_pin = cold_job.run(&options, &cancel, |_| {}).unwrap();
        assert_eq!(
            sqlite_snapshot(&index_dir(state.path()).join("index.db"), local_pin, true),
            sqlite_snapshot(
                &index_dir(cold_state.path()).join("index.db"),
                cold_pin,
                true
            ),
            "{path}: native/graph/class/reference selected facts must equal same-root cold"
        );
    }
}

#[cfg(feature = "test-support")]
#[test]
fn full_reuse_small_edit_plain_and_stream_match_independent_cold_publication() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        index_coordinator::IndexJobCoordinator,
        indexer::{IndexOptions, index_workspace_bundle},
        model::{CancelFlag, IndexPin},
    };
    for stream in [false, true] {
        let (state, workspace) = fixture();
        fs::write(
            workspace.path().join("A.java"),
            "class A { int run(){ return helper(); } int helper(){ return 1; } }\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("a.py"),
            "class Base: pass\nclass A(Base):\n    def run(self):\n        return 1\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("edit.js"),
            "function edit(){ return 1; }\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("gap.rs"),
            "fn gap(value: i32) -> i32 { opaque!(hidden); value }\n",
        )
        .unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let first = IndexJobCoordinator::prepare(&store, None)
            .unwrap()
            .run(&options, &cancel, |_| {})
            .unwrap();
        let ids = |pin: IndexPin| {
            let db = rusqlite::Connection::open(index_dir(state.path()).join("index.db")).unwrap();
            let mut statement = db.prepare("SELECT path,vid,gid,cid FROM revision_documents WHERE revision_id=?1 AND path!='edit.js' ORDER BY path").unwrap();
            statement
                .query_map(
                    [format!(
                        "pin:v1:{}:{}",
                        pin.index_generation, pin.index_revision
                    )],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let prior_ids = ids(first);
        assert_eq!(prior_ids.len(), 3);
        let first_families = measure_native_for_parity(workspace.path(), &store);
        assert_native_reference_and_gap_fixture(&first_families);
        assert_selected_native_matches(&store, first, &first_families);
        fs::write(
            workspace.path().join("edit.js"),
            "function renamed(){ return 2; }\nrenamed();\n",
        )
        .unwrap();
        // Force the two FULL entrypoints so the later local-path widening
        // cannot turn this reuse regression into a local-publication test.
        let session = store.writer_session_for_tests().unwrap();
        let leader = session.writer_guard().unwrap();
        let next = if stream {
            let capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
            store
                .publish_stream_native_only_for_tests(&options, &capture, leader, &cancel)
                .unwrap()
        } else {
            let (graph, native, capture) =
                index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
            store
                .publish_native(&graph, &capture, &native, leader, first, &cancel)
                .unwrap()
        };
        drop(session);
        assert_eq!(next.index_generation, first.index_generation);
        assert_eq!(next.index_revision, first.index_revision + 1);
        assert_eq!(
            ids(next),
            prior_ids,
            "stream={stream}: reuse all three stored IDs"
        );
        assert_selected_matches_cold(state.path(), workspace.path(), &store, next);
        assert_selected_native_matches(&store, first, &first_families);
    }
}
