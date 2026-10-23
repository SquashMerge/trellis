use super::*;
use crate::indexer::{IndexOptions, index_workspace_bundle};
use std::{cell::RefCell, fs, os::unix::fs::MetadataExt, sync::atomic::AtomicBool};

fn ready() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Store,
    Graph,
    crate::capture::Capture,
    crate::native_evidence::Artifact,
    IndexPin,
    CancelFlag,
    Arc<topology::WriterSession>,
) {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("flow.js"),
        "function go() { measured(); }\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(work.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let session = store.writer_session().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.writer_guard().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    (
        state, work, store, graph, capture, native, pin, cancel, session,
    )
}

#[test]
fn retained_two_revision_tree_survives_partial_third_publication_rollback() {
    let (_state, work, store, graph, capture, native, first, cancel, session) = ready();
    let leader = session.writer_guard().unwrap();
    let second = store
        .publish_native(&graph, &capture, &native, leader, first, &cancel)
        .unwrap();
    assert_eq!(second.index_generation, first.index_generation);
    assert_eq!(second.index_revision, first.index_revision + 1);
    fs::write(
        work.path().join("flow.js"),
        "function go() { changed(); }\n",
    )
    .unwrap();
    let (edited_graph, edited_native, edited_capture) = index_workspace_bundle(
        &IndexOptions::new(work.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let path = store.roots.index_db(&store.identity);
    let before = fs::read(&path).unwrap();
    let named = fs::symlink_metadata(&path).unwrap();
    let before_inode = (named.dev(), named.ino());
    let sidecars = |p: &Path| {
        ["-wal", "-shm", "-journal"]
            .into_iter()
            .map(|suffix| {
                let sidecar = p.with_file_name(format!("index.db{suffix}"));
                fs::symlink_metadata(&sidecar)
                    .ok()
                    .map(|metadata| (metadata.dev(), metadata.ino(), fs::read(sidecar).unwrap()))
            })
            .collect::<Vec<_>>()
    };
    let before_sidecars = sidecars(&path);
    let error = store
            .publish_inner_checked(
                (&edited_graph, &edited_capture, &edited_native),
                leader,
                second,
                &cancel,
                |stage, tx| {
                    if stage == PublishStage::AfterFile {
                        // The new r3 header/manifest has already been inserted;
                        // force an actual SQLite error inside its transaction.
                        tx.execute(
                            "INSERT INTO native_revisions SELECT * FROM native_revisions WHERE published_index_revision=?1",
                            [second.index_revision as i64],
                        )?;
                    }
                    Ok(())
                },
            )
            .unwrap_err();
    assert!(
        error.to_string().contains("UNIQUE constraint failed"),
        "{error:#}"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "partial r3 write changed r1 or r2 bytes"
    );
    let named = fs::symlink_metadata(&path).unwrap();
    assert_eq!(
        (named.dev(), named.ino()),
        before_inode,
        "r3 rollback must preserve original SQLite inode"
    );
    assert_eq!(
        sidecars(&path),
        before_sidecars,
        "r3 rollback must preserve SQLite sidecar bytes and inodes"
    );
    assert_eq!(store.status().unwrap().revision, second);
    let db = Connection::open(path).unwrap();
    let headers: i64 = db
        .query_row("SELECT count(*) FROM native_revisions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(headers, 2);
    assert_eq!(
        store
            .source_at("flow.js", Some(first))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function go() { measured(); }\n"
    );
    assert_eq!(
        store
            .source_at("flow.js", Some(second))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function go() { measured(); }\n"
    );
}

#[test]
fn response_post_fence_discards_none_and_empty_results_on_root_loss() {
    let (_state, work, store, _graph, _capture, _native, _pin, _cancel, _session) = ready();
    let original = work.path().to_owned();
    let moved = original.with_extension("moved-after-none");
    let none = store.with_evidence_hook(
        |_db| Ok(None::<IndexStatus>),
        || {
            std::fs::rename(&original, &moved)?;
            std::fs::create_dir(&original)?;
            Ok(())
        },
    );
    assert!(none.is_err(), "post-fence failure must discard None");

    let (_state, work, store, _graph, _capture, _native, _pin, _cancel, _session) = ready();
    let original = work.path().to_owned();
    let moved = original.with_extension("moved-after-empty");
    let empty = store.with_evidence_hook(
        |_db| Ok(Vec::<Symbol>::new()),
        || {
            std::fs::rename(&original, &moved)?;
            std::fs::create_dir(&original)?;
            Ok(())
        },
    );
    assert!(
        empty.is_err(),
        "post-fence failure must discard an empty collection"
    );
}

#[test]
fn direct_response_pre_fence_rejects_root_change() {
    let (_state, work, store, _graph, _capture, _native, _pin, _cancel, _session) = ready();
    let original = work.path().to_owned();
    let moved = original.with_extension("moved-before-read");
    std::fs::rename(&original, &moved).unwrap();
    std::fs::create_dir(&original).unwrap();
    let root_error = store.status().unwrap_err();
    assert!(
        root_error.to_string().contains("root_changed"),
        "{root_error:#}"
    );
}

#[test]
fn response_fences_root_and_error_results_before_release() {
    let (_state, work, store, _graph, _capture, _native, _pin, _cancel, _session) = ready();
    let original = work.path().to_owned();
    let moved = original.with_extension("moved-for-fence");
    let root_error = store
        .with_evidence_hook(
            |_db| Ok(Some("owned".to_owned())),
            || {
                std::fs::rename(&original, &moved)?;
                std::fs::create_dir(&original)?;
                Ok(())
            },
        )
        .unwrap_err();
    assert!(
        root_error.to_string().contains("root_changed"),
        "{root_error:#}"
    );

    let (_state, work, store, _graph, _capture, _native, _pin, _cancel, _session) = ready();
    let original = work.path().to_owned();
    let moved = original.with_extension("moved-for-error");
    let finish_observed = std::cell::Cell::new(false);
    let error = store
        .with_evidence_observed::<()>(
            |_db| anyhow::bail!("materialization sentinel"),
            || {
                std::fs::rename(&original, &moved)?;
                std::fs::create_dir(&original)?;
                Ok(())
            },
            |fence| {
                finish_observed.set(true);
                assert!(fence.is_err(), "root change must fail the final fence");
            },
        )
        .unwrap_err();
    assert!(finish_observed.get(), "final fence was not attempted");
    assert!(
        error.to_string().contains("materialization sentinel"),
        "{error:#}"
    );
}

#[test]
fn tree_metadata_applies_no_overlay_before_post_fence() {
    let (_state, work, store, _graph, _capture, _native, _pin, _cancel, _session) = ready();
    let mut entries = vec![crate::file_tree::Entry {
        name: "flow.js".into(),
        path: "flow.js".into(),
        kind: "file",
        indexed_path: None,
        method_count: None,
        unindexed_reason: None,
    }];
    let original = work.path().to_owned();
    let moved = original.with_extension("moved-after-tree-read");
    let error = store
        .tree_metadata_with_finish_hook(work.path(), &mut entries, || {
            std::fs::rename(&original, &moved)?;
            std::fs::create_dir(&original)?;
            Ok(())
        })
        .unwrap_err();
    assert!(error.to_string().contains("root_changed"), "{error:#}");
    assert!(entries[0].indexed_path.is_none());
    assert!(entries[0].method_count.is_none());
}

#[test]
fn durable_fallback_classifier_excludes_operational_and_current_corruption_errors() {
    let operational: anyhow::Error =
        std::io::Error::from(std::io::ErrorKind::PermissionDenied).into();
    assert!(!native_index_unavailable(&operational));
    let sqlite: anyhow::Error = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
        None,
    )
    .into();
    assert!(!native_index_unavailable(&sqlite));
    let corruption: anyhow::Error =
        ControlIntegrity("incompatible_index: current marker".into()).into();
    assert!(!native_index_unavailable(&corruption));
    let safe: anyhow::Error = topology::IndexNotReady::new("reconciliation required").into();
    assert!(native_index_unavailable(&safe));
}

#[test]
fn recovery_classifier_separates_corruption_from_operational_storage_errors() {
    let classify = |extended_code| {
        let error: anyhow::Error =
            rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(extended_code), None).into();
        recovery_class(&error)
    };
    assert_eq!(
        classify(rusqlite::ffi::SQLITE_CORRUPT),
        RecoveryClass::RecreatePending
    );
    assert_eq!(
        classify(rusqlite::ffi::SQLITE_NOTADB),
        RecoveryClass::RecreatePending
    );
    for code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_LOCKED,
        rusqlite::ffi::SQLITE_FULL,
        rusqlite::ffi::SQLITE_READONLY,
        rusqlite::ffi::SQLITE_CANTOPEN,
        rusqlite::ffi::SQLITE_IOERR,
        rusqlite::ffi::SQLITE_NOMEM,
        rusqlite::ffi::SQLITE_ERROR,
    ] {
        assert_eq!(classify(code), RecoveryClass::Hard);
    }
}

#[test]
fn live_read_reporter_separates_root_storage_corruption_from_operational_errors() {
    let sqlite = |code| -> anyhow::Error {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None).into()
    };
    for code in [rusqlite::ffi::SQLITE_CORRUPT, rusqlite::ffi::SQLITE_NOTADB] {
        let (_state, _work, store, _graph, _capture, _native, _pin, _cancel, _session) = ready();
        let clone = store.clone();
        let error = store.report_live_read_failure(sqlite(code));
        assert!(
            error.to_string().starts_with("recovery_required:"),
            "{error:#}"
        );
        assert_eq!(store.disposition(), RecoveryDisposition::RecreatePending);
        assert!(
            clone
                .status()
                .unwrap_err()
                .to_string()
                .starts_with("recovery_required:")
        );
    }
    for code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_LOCKED,
        rusqlite::ffi::SQLITE_IOERR,
        rusqlite::ffi::SQLITE_ERROR,
    ] {
        let (_state, _work, store, _graph, _capture, _native, pin, _cancel, _session) = ready();
        let error = store.report_live_read_failure(sqlite(code));
        assert!(error.downcast_ref::<rusqlite::Error>().is_some());
        assert_eq!(store.status().unwrap().revision, pin);
    }
    let (_state, _work, store, _graph, _capture, _native, _pin, _cancel, _session) = ready();
    let clone = store.clone();
    let error = store.report_live_read_failure(
        rusqlite::Error::InvalidColumnType(0, "root_spelling".into(), rusqlite::types::Type::Real)
            .into(),
    );
    assert!(error.to_string().starts_with("incompatible_index:"));
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );

    let (_state, _work, store, _graph, _capture, _native, _pin, _cancel, _session) = ready();
    let clone = store.clone();
    let error = store.report_live_read_failure(
        ControlIntegrity("incompatible_index: existing structural check".into()).into(),
    );
    assert!(error.to_string().starts_with("incompatible_index:"));
    assert!(
        clone
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );

    let (_state, _work, store, _graph, _capture, _native, pin, _cancel, _session) = ready();
    let error = store.report_live_read_failure(anyhow::anyhow!(
        "root_key_collision: index belongs to a different spelling"
    ));
    assert!(error.to_string().starts_with("root_key_collision:"));
    assert_eq!(store.status().unwrap().revision, pin);
}

#[test]
fn selected_operational_errors_do_not_latch_but_integrity_failure_closes_clones() {
    for code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_LOCKED,
        rusqlite::ffi::SQLITE_FULL,
        rusqlite::ffi::SQLITE_READONLY,
        rusqlite::ffi::SQLITE_CANTOPEN,
        rusqlite::ffi::SQLITE_IOERR,
        rusqlite::ffi::SQLITE_NOMEM,
        rusqlite::ffi::SQLITE_ERROR,
    ] {
        let (_state, _work, store, _graph, _capture, _native, pin, _cancel, _session) = ready();
        let clone = store.clone();
        let operational: anyhow::Error =
            rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None).into();
        let returned = store.report_selected_failure(operational);
        assert!(returned.downcast_ref::<rusqlite::Error>().is_some());
        assert_eq!(store.status().unwrap().revision, pin);
        assert_eq!(clone.status().unwrap().revision, pin);
    }

    let (_state, _work, store, _graph, _capture, _native, pin, _cancel, _session) = ready();
    let clone = store.clone();
    let io: anyhow::Error =
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected").into();
    let returned = store.report_selected_failure(io);
    assert!(returned.downcast_ref::<std::io::Error>().is_some());
    assert_eq!(store.status().unwrap().revision, pin);
    assert_eq!(clone.status().unwrap().revision, pin);

    let integrity: anyhow::Error = SelectedIntegrity("selected mismatch".into()).into();
    let returned = store.report_selected_failure(integrity);
    assert!(returned.to_string().contains("incompatible_index"));
    assert!(store.status().is_err());
    assert!(clone.status().is_err());
}

#[cfg(unix)]
#[test]
fn persisted_scan_classifies_same_size_preserved_mtime_edit_by_ctime() {
    use std::ffi::CString;
    use std::os::unix::fs::MetadataExt;

    let (_state, work, store, _graph, _capture, _native, _pin, cancel, _session) = ready();
    let path = work.path().join("flow.js");
    let before_meta = fs::metadata(&path).unwrap();
    let before_stat: crate::capture::CaptureStat = {
        let db = store.cache().unwrap();
        let payload: String = db
            .query_row(
                "SELECT capture_stat FROM revision_documents WHERE path='flow.js'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&payload).unwrap()
    };
    fs::write(&path, "function go() { observed(); }\n").unwrap();
    let c_path = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let times = [
        libc::timespec {
            tv_sec: before_meta.atime(),
            tv_nsec: before_meta.atime_nsec(),
        },
        libc::timespec {
            tv_sec: before_meta.mtime(),
            tv_nsec: before_meta.mtime_nsec(),
        },
    ];
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) },
        0
    );
    let options = IndexOptions::new(work.path().to_owned());
    let (_next, _native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let after_stat = capture.source_stat("flow.js").unwrap();
    assert_eq!(before_stat.size, after_stat.size);
    assert_eq!(before_stat.device, after_stat.device);
    assert_eq!(before_stat.inode, after_stat.inode);
    assert_eq!(before_stat.mtime_seconds, after_stat.mtime_seconds);
    assert_eq!(before_stat.mtime_nanoseconds, after_stat.mtime_nanoseconds);
    assert_ne!(
        (before_stat.ctime_seconds, before_stat.ctime_nanoseconds),
        (after_stat.ctime_seconds, after_stat.ctime_nanoseconds)
    );
    let db = store.cache().unwrap();
    let comparison = compare_capture_snapshot(&db, &capture).unwrap();
    assert_eq!(comparison.ctime_changed, 1);
    assert_eq!(comparison.hash_changed, 1);
    assert!(comparison.changed());
    assert_eq!(capture.files[0].text, "function go() { observed(); }\n");
    assert_eq!(
        capture.source_operations["flow.js"],
        crate::capture::SourceOperations {
            opens: 1,
            complete_reads: 1,
            hashes: 1,
        }
    );
}

#[test]
fn real_revision_private_recovery_before_commit_error_rolls_back_same_inode() {
    use rusqlite::types::ValueRef;
    use std::os::unix::fs::MetadataExt;

    let (_state, _work, store, graph, capture, native, _old, cancel, session) = ready();
    let clone = store.clone();
    let path = store.roots.index_db(&store.identity);
    let inode = fs::metadata(&path).unwrap().ino();
    let db = Connection::open(&path).unwrap();
    db.execute(
        "UPDATE index_metadata SET index_revision=CAST(1.5 AS REAL)",
        [],
    )
    .unwrap();
    drop(db);

    let logical = || {
        let db = Connection::open(&path).unwrap();
        let (schema, extractor, generation, files, nodes, documents, revisions, classes):
                (i64, String, String, i64, i64, i64, i64, i64) = db
                .query_row(
                    "SELECT schema_version,extractor_version,index_generation,(SELECT count(*) FROM revision_documents),(SELECT count(*) FROM graph_nodes),(SELECT count(*) FROM document_versions),(SELECT count(*) FROM native_revisions),(SELECT count(*) FROM class_projections) FROM index_metadata",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)),
                )
                .unwrap();
        let revision_bits = db
            .query_row(
                "SELECT index_revision FROM index_metadata",
                [],
                |row| match row.get_ref(0)? {
                    ValueRef::Real(value) => Ok(value.to_bits()),
                    _ => Err(rusqlite::Error::InvalidQuery),
                },
            )
            .unwrap();
        assert!(files > 0, "rollback fixture must contain a file row");
        assert!(
            documents > 0,
            "rollback fixture must contain a native document row"
        );
        let selected_file: (String, String) = db
            .query_row(
                "SELECT content_hash,path FROM document_versions ORDER BY path LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let selected_native: (String, Vec<u8>) = db
                .query_row(
                    "SELECT content_hash,source_bytes FROM document_versions ORDER BY language,path LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
        (
            schema,
            extractor,
            generation,
            revision_bits,
            files,
            nodes,
            documents,
            revisions,
            classes,
            selected_file,
            selected_native,
        )
    };
    let before = logical();
    let baseline = {
        let mut db = store.cache().unwrap();
        let tx = db.transaction().unwrap();
        store.recovery_baseline(&tx).unwrap()
    };
    assert!(baseline.pin().is_none());
    let error = store
        .publish_inner_checked_expected(
            (&graph, &capture, &native),
            session.writer_guard().unwrap(),
            ExpectedPublication::Recovery(Box::new(baseline)),
            &cancel,
            256 * 1024 * 1024 + 16 * 1024,
            |stage, _tx| {
                if stage == PublishStage::BeforeCommit {
                    anyhow::bail!("injected private metadata recovery failure");
                }
                Ok(())
            },
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected private metadata recovery failure"),
        "{error:#}"
    );
    assert_eq!(logical(), before, "failed recovery changed logical pair");
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
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
}

#[test]
fn second_connection_adds_legacy_trigger_after_admission_before_publish_lock() {
    let (_state, _work, store, graph, capture, native, old, cancel, session) = ready();
    let clone = store.clone();
    let path = store.roots.index_db(&store.identity);
    let after_external = RefCell::new(None);
    let error = store.publish_inner_checked((&graph, &capture, &native), session.writer_guard().unwrap(), old, &cancel,
            |stage, _checked_connection| {
                if stage == PublishStage::BeforeTransaction {
                    // The Store connection has passed open_index's exact object check,
                    // but has NOT acquired the SQLite writer lock yet.
                    let attacker = Connection::open(&path)?;
                    attacker.execute_batch("CREATE TRIGGER forged_after_admission AFTER INSERT ON graph_nodes BEGIN
                        UPDATE graph_nodes SET name='FORGED-NOT-MEASURED' WHERE gid=NEW.gid AND id=NEW.id; END;")?;
                    drop(attacker);
                    *after_external.borrow_mut() = Some(fs::read(&path)?);
                }
                Ok(())
            }).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("incompatible_index: cache changed after admission"),
        "{error:#}"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        after_external.into_inner().unwrap(),
        "Store must not change DB bytes after the external CREATE TRIGGER"
    );
    let db = Connection::open(&path).unwrap();
    let (schema, marker, generation, revision): (i64, String, String, i64) = db.query_row(
            "SELECT schema_version,extractor_version,index_generation,index_revision FROM index_metadata", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(
        (schema, marker.as_str(), generation, revision),
        (
            i64::from(DATABASE_SCHEMA_VERSION),
            "native-v6-delta-v1",
            old.index_generation.to_string(),
            old.index_revision as i64
        )
    );
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        i64::from(DATABASE_SCHEMA_VERSION)
    );
    let forged: i64 = db
        .query_row(
            "SELECT count(*) FROM graph_nodes WHERE name='FORGED-NOT-MEASURED'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(forged, 0, "post-admission trigger must never execute");
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    db.execute_batch("DROP TRIGGER forged_after_admission")
        .unwrap();
    drop(db);
    for closed in [store.status().unwrap_err(), clone.status().unwrap_err()] {
        assert_eq!(
            closed.to_string(),
            "incompatible_index: reconciliation required after invalid current index"
        );
    }
}

#[test]
fn second_connection_adds_view_after_admission_before_status_and_leader_snapshot() {
    let (_state, _work, store, _graph, _capture, _native, _pin, _cancel, _session) = ready();
    let status_clone = store.clone();
    let path = store.roots.index_db(&store.identity);
    let after_status_ddl = RefCell::new(None);
    let status_error = store
        .status_with_open_hook(|_checked| {
            let attacker = Connection::open(&path)?;
            attacker.execute_batch("CREATE VIEW status_after_admission AS SELECT 1")?;
            drop(attacker);
            *after_status_ddl.borrow_mut() = Some(fs::read(&path)?);
            Ok(())
        })
        .unwrap_err();
    assert!(
        status_error
            .to_string()
            .contains("incompatible_index: unknown cache object")
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        after_status_ddl.into_inner().unwrap()
    );
    let attacker = Connection::open(&path).unwrap();
    attacker
        .execute_batch("DROP VIEW status_after_admission")
        .unwrap();
    drop(attacker);
    for closed in [
        store.status().unwrap_err(),
        status_clone.status().unwrap_err(),
    ] {
        assert_eq!(
            closed.to_string(),
            "incompatible_index: reconciliation required after invalid current index"
        );
    }
}

#[test]
fn publication_connection_runs_with_the_publication_page_cache() {
    let (_state, _work, store, graph, capture, native, old, cancel, session) = ready();
    let seen = RefCell::new(Vec::new());
    store
        .publish_inner_checked(
            (&graph, &capture, &native),
            session.writer_guard().unwrap(),
            old,
            &cancel,
            |stage, connection| {
                let kib: i64 = connection.pragma_query_value(None, "cache_size", |r| r.get(0))?;
                seen.borrow_mut()
                    .push((stage == PublishStage::BeforeCommit, kib));
                Ok(())
            },
        )
        .unwrap();
    let seen = seen.into_inner();
    assert!(seen.iter().any(|(commit, _)| *commit), "{seen:?}");
    assert!(
        seen.iter().all(|(_, kib)| *kib == -PUBLICATION_CACHE_KIB),
        "every publication stage must run with the publication cache: {seen:?}"
    );
}
