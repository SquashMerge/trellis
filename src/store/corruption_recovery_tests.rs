use super::selected_read_once_tests::published_store;
use super::*;
use crate::{index_coordinator::IndexJobCoordinator, indexer::IndexOptions};
use std::{fs, sync::atomic::AtomicBool};

#[derive(Clone, Copy, Debug)]
enum ReaderFamily {
    Source,
    Symbols,
    Navigation,
    Metrics,
    Files,
    Architecture,
}

impl ReaderFamily {
    fn read(self, store: &Store, pin: IndexPin) -> Result<()> {
        match self {
            Self::Source => {
                let (_, source) = store.source_at("f0.js", Some(pin))?.unwrap();
                assert!(source.text.contains("function f0"));
            }
            Self::Symbols => {
                let (actual, symbols) = store.symbols_at("f0", 10)?;
                assert_eq!(actual, pin);
                assert!(symbols.iter().any(|symbol| symbol.name == "f0"));
            }
            Self::Navigation => {
                let request = crate::navigation::NavigationRequest::Source(
                    crate::navigation::SourceSelector {
                        expected_revision: pin,
                        path: "f0.js".into(),
                        line: 1,
                    },
                );
                let result = store.navigation_at(&request)?;
                assert_eq!(result.revision, pin);
                assert!(!result.targets.is_empty());
            }
            Self::Metrics => {
                let response = store.evidence_response()?;
                let result = response
                    .metrics_at(&MetricsOptions::default(), &mut || Ok(()))
                    .map_err(|error| response.report_index_failure(error))?;
                let result = response.finish(result)?;
                assert_eq!(result.index_revision, pin.index_revision);
                assert_eq!(result.files.len(), 1);
            }
            Self::Files => {
                let page = store.files_at(Some(pin), 0, 10)?;
                assert_eq!(page["items"].as_array().unwrap().len(), 1);
                assert_eq!(page["items"][0]["path"], "f0.js");
            }
            Self::Architecture => {
                let response = store.evidence_response()?;
                let result = response.architecture_response(
                    "references",
                    &serde_json::json!({"expectedBasis":pin}),
                    &mut || Ok(()),
                )?;
                let result = response.finish(result)?;
                assert_eq!(result["indexRevision"], pin.index_revision);
                assert_eq!(result["modules"].as_array().unwrap().len(), 1);
            }
        }
        Ok(())
    }

    fn corrupt(self, store: &Store) {
        let db = store.cache_write().unwrap();
        db.pragma_update(None, "foreign_keys", "OFF").unwrap();
        db.pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        let sql = match self {
            // Keep source bytes unchanged: decoding the stored length must fail.
            Self::Source => "UPDATE document_versions SET byte_length='invalid'",
            // A short query uses the literal search, so the failure is the row
            // conversion, not an FTS integrity check.
            Self::Symbols => "UPDATE graph_nodes SET id='invalid' WHERE name='f0'",
            Self::Navigation => "UPDATE native_revisions SET class_truncated='invalid'",
            // The metrics manifest enumeration reads this before fetching a document.
            Self::Metrics => "UPDATE revision_documents SET language=x'ff'",
            Self::Files => "UPDATE revision_documents SET gid=-1",
            Self::Architecture => {
                "UPDATE native_version_declarations SET role='invalid' WHERE name='f0'"
            }
        };
        assert_eq!(db.execute(sql, []).unwrap(), 1, "{self:?}");
    }
}

fn reindex(store: &Store, root: &Path) -> IndexPin {
    IndexJobCoordinator::prepare(store, None)
        .unwrap()
        .run(
            &IndexOptions::new(root.to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap()
}

fn assert_rebuild(store: &Store, error: &anyhow::Error) {
    assert_eq!(recovery_class(error), RecoveryClass::Rebuild, "{error:#}");
    assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
    assert!(store.recovery_required.load(Ordering::Acquire));
}

#[test]
fn row_corruption_in_each_reader_family_reindexes_and_restores_reads() {
    for family in [
        ReaderFamily::Source,
        ReaderFamily::Symbols,
        ReaderFamily::Navigation,
        ReaderFamily::Metrics,
        ReaderFamily::Files,
        ReaderFamily::Architecture,
    ] {
        let (_state, root, store) = published_store(1);
        let pin = store.status().unwrap().revision;
        family.read(&store, pin).unwrap();
        family.corrupt(&store);
        let clone = store.clone();
        let error = family.read(&store, pin).unwrap_err();
        assert_rebuild(&store, &error);
        assert_eq!(clone.disposition(), RecoveryDisposition::Rebuild);
        assert!(clone.status().is_err());

        let repaired = reindex(&store, root.path());
        assert_eq!(store.disposition(), RecoveryDisposition::Ready);
        assert!(!store.recovery_required.load(Ordering::Acquire));
        family.read(&clone, repaired).unwrap();
    }
}

#[test]
fn corrupt_prior_document_read_schedules_a_full_reindex() {
    let (_state, root, store) = published_store(1);
    let options = IndexOptions::new(root.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let baseline = store.recovery_index_baseline().unwrap();
    {
        let db = store.cache_write().unwrap();
        db.pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        assert_eq!(
            db.execute("UPDATE document_versions SET byte_length='invalid'", [])
                .unwrap(),
            1
        );
    }
    fs::write(
        root.path().join("f0.js"),
        "function f0(x) { if (x) { return g(x); } return 2; }\n",
    )
    .unwrap();
    let capture = crate::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let error = match store.prepare_local_revision(&capture, &baseline, &cancel) {
        Err(error) => error,
        Ok(_) => panic!("the prior document must not decode"),
    };
    assert_rebuild(&store, &error);
    let repaired = reindex(&store, root.path());
    ReaderFamily::Source.read(&store, repaired).unwrap();
    assert!(
        store
            .source("f0.js")
            .unwrap()
            .unwrap()
            .text
            .contains("return 2")
    );
}

#[test]
fn corrupt_retained_pin_binding_schedules_reindex() {
    let (_state, root, store) = published_store(1);
    let retained = store.status().unwrap().revision;
    reindex(&store, root.path());
    let key = format!(
        "pin:v1:{}:{}",
        retained.index_generation, retained.index_revision
    );
    {
        let db = store.cache_write().unwrap();
        db.pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        assert_eq!(
            db.execute(
                "UPDATE revision_producer_bindings SET producer_sha=x'ff' WHERE revision_id=?1",
                [&key],
            )
            .unwrap(),
            1
        );
    }
    let response = store.evidence_response().unwrap();
    let error = response
        .validate_pin(retained)
        .map_err(|error| response.report_index_failure(error))
        .unwrap_err();
    drop(response);
    assert_rebuild(&store, &error);
    let repaired = reindex(&store, root.path());
    store
        .evidence_response()
        .unwrap()
        .validate_pin(repaired)
        .unwrap();
    ReaderFamily::Source.read(&store, repaired).unwrap();
}

#[test]
fn evidence_boundary_keeps_operational_failures_out_of_recovery() {
    let (_state, _root, store) = published_store(1);
    let pin = store.status().unwrap().revision;
    for code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_LOCKED,
        rusqlite::ffi::SQLITE_IOERR,
        rusqlite::ffi::SQLITE_FULL,
        rusqlite::ffi::SQLITE_INTERRUPT,
    ] {
        let returned = store
            .with_evidence::<()>(|_| {
                storage_result(Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(code),
                    None,
                )))
            })
            .unwrap_err();
        if matches!(
            code,
            rusqlite::ffi::SQLITE_BUSY | rusqlite::ffi::SQLITE_LOCKED
        ) {
            assert!(returned.is::<SqliteContention>());
        } else {
            assert!(returned.is::<rusqlite::Error>());
        }
        assert_eq!(store.disposition(), RecoveryDisposition::Ready);
        assert!(!store.recovery_required.load(Ordering::Acquire));
        assert_eq!(store.status().unwrap().revision, pin);
    }
    let returned = store
        .with_evidence::<()>(|_| {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected").into())
        })
        .unwrap_err();
    assert!(returned.is::<std::io::Error>());
    assert_eq!(store.disposition(), RecoveryDisposition::Ready);
    assert!(!store.recovery_required.load(Ordering::Acquire));
}

#[derive(Debug)]
struct CancelledRead;
impl std::fmt::Display for CancelledRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}
impl std::error::Error for CancelledRead {}

#[test]
fn metrics_cancellation_does_not_schedule_recovery() {
    let (_state, _root, store) = published_store(1);
    let pin = store.status().unwrap().revision;
    let response = store.evidence_response().unwrap();
    let error = response
        .metrics_at(
            &MetricsOptions::default(),
            &mut || Err(CancelledRead.into()),
        )
        .map_err(|error| response.report_index_failure(error))
        .unwrap_err();
    assert!(error.is::<CancelledRead>());
    drop(response);
    assert_eq!(store.disposition(), RecoveryDisposition::Ready);
    assert!(!store.recovery_required.load(Ordering::Acquire));
    ReaderFamily::Metrics.read(&store, pin).unwrap();
}

#[test]
fn missing_selected_source_version_rebuilds_instead_of_looking_absent() {
    let (_state, root, store) = published_store(1);
    let pin = store.status().unwrap().revision;
    {
        let db = store.cache_write().unwrap();
        db.execute_batch("PRAGMA foreign_keys=OFF; DELETE FROM document_versions;")
            .unwrap();
    }
    let error = store.source_at("f0.js", Some(pin)).unwrap_err();
    assert_rebuild(&store, &error);
    let repaired = reindex(&store, root.path());
    ReaderFamily::Source.read(&store, repaired).unwrap();
    assert!(
        store
            .source_at("absent.js", Some(repaired))
            .unwrap()
            .is_none()
    );
    assert_eq!(store.disposition(), RecoveryDisposition::Ready);
}

#[test]
fn optional_recovery_survives_response_drop_and_transferred_fence() {
    for transfer in [false, true] {
        let (_state, root, store) = published_store(1);
        let pin = store.status().unwrap().revision;
        let response = store.evidence_response().unwrap();
        response.report_optional_corruption(
            SelectedIntegrity("injected optional corruption".into()).into(),
        );
        assert_eq!(store.disposition(), RecoveryDisposition::Ready);
        if transfer {
            let fence = response.into_fence(EvidenceFencePolicy::ExactPin(pin));
            assert_eq!(store.disposition(), RecoveryDisposition::Ready);
            assert_eq!(fence.finish(7).unwrap(), 7);
        } else {
            drop(response);
        }
        assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
        let repaired = reindex(&store, root.path());
        ReaderFamily::Source.read(&store, repaired).unwrap();
    }
}

#[test]
fn optional_operational_failures_do_not_schedule_recovery() {
    let (_state, _root, store) = published_store(1);
    for code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_IOERR,
        rusqlite::ffi::SQLITE_FULL,
        rusqlite::ffi::SQLITE_INTERRUPT,
    ] {
        let response = store.evidence_response().unwrap();
        response.report_optional_corruption(
            rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None).into(),
        );
        response.finish(()).unwrap();
        drop(response);
        assert_eq!(store.disposition(), RecoveryDisposition::Ready);
        assert!(!store.recovery_required.load(Ordering::Acquire));
    }
}

#[test]
fn native_declaration_index_decode_failure_schedules_rebuild() {
    let (_state, root, store) = published_store(1);
    let pin = store.status().unwrap().revision;
    {
        let db = store.cache_write().unwrap();
        db.execute_batch(
            "PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON; UPDATE revision_documents SET language=x'ff';",
        )
        .unwrap();
    }
    let response = store.evidence_response().unwrap();
    let error = response
        .mcp_find("f0", 10, Some(pin))
        .map_err(|error| response.report_index_failure(error))
        .err()
        .expect("corrupt native manifest must reject find");
    drop(response);
    assert_rebuild(&store, &error);
    let repaired = reindex(&store, root.path());
    let response = store.evidence_response().unwrap();
    let found = response.mcp_find("f0", 10, Some(repaired)).unwrap();
    // Find also matches syntax-ID substrings, including unnamed modules.
    assert!(found.symbols.iter().any(|symbol| {
        symbol.declaration.name.as_deref() == Some("f0")
            && symbol.declaration.document.path == "f0.js"
    }));
    response.finish(()).unwrap();
}

#[test]
fn missing_metadata_marker_reopens_and_rebuilds_in_place() {
    let (state, root, store) = published_store(1);
    let path = store.roots.index_db(&store.identity);
    use std::os::unix::fs::MetadataExt;
    let inode = fs::metadata(&path).unwrap().ino();
    store
        .cache_write()
        .unwrap()
        .execute("DELETE FROM index_metadata", [])
        .unwrap();
    drop(store);
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_eq!(reopened.disposition(), RecoveryDisposition::Rebuild);
    let repaired = reindex(&reopened, root.path());
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    ReaderFamily::Source.read(&reopened, repaired).unwrap();
}

#[test]
fn undecodable_root_metadata_rebuilds_but_decoded_other_root_still_refuses() {
    for conflicting in [false, true] {
        let (state, root, store) = published_store(1);
        {
            let db = store.cache_write().unwrap();
            db.execute_batch(
                "PRAGMA ignore_check_constraints=ON; UPDATE index_metadata SET root_device=x'ff';",
            )
            .unwrap();
            if conflicting {
                db.execute(
                    "UPDATE index_metadata SET root_spelling='/different-root'",
                    [],
                )
                .unwrap();
            }
        }
        drop(store);
        let reopened = Store::open_for_tests(state.path(), root.path());
        if conflicting {
            assert!(
                reopened
                    .err()
                    .unwrap()
                    .to_string()
                    .starts_with("root_key_collision:")
            );
        } else {
            let reopened = reopened.unwrap();
            assert_eq!(reopened.disposition(), RecoveryDisposition::Rebuild);
            assert!(reopened.recorded_index_options().unwrap().is_none());
            let pin = reindex(&reopened, root.path());
            ReaderFamily::Source.read(&reopened, pin).unwrap();
        }
    }
}
