use super::selected_read_once_tests::published_store;
use super::*;
use crate::{index_coordinator::IndexJobCoordinator, indexer::IndexOptions};

fn reindex(store: &Store, root: &Path) -> IndexPin {
    let pin = IndexJobCoordinator::prepare(store, None)
        .unwrap()
        .run(
            &IndexOptions::new(root.to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
    assert_eq!(store.disposition(), RecoveryDisposition::Ready);
    assert!(!store.recovery_required.load(Ordering::Acquire));
    assert!(store.source("f0.js").unwrap().is_some());
    pin
}

fn assert_rebuild(store: &Store, error: &anyhow::Error) {
    assert_eq!(recovery_class(error), RecoveryClass::Rebuild, "{error:#}");
    assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
    assert!(store.recovery_required.load(Ordering::Acquire));
}

#[test]
fn missing_selected_producer_executable_inventory_rebuilds_and_repairs() {
    let (_state, root, store) = published_store(1);
    let selected = store.status().unwrap().revision;
    reindex(&store, root.path());
    // Keep the current head healthy so selected producer binding, rather than
    // current-head reconciliation inventory admission, detects this missing row.
    {
        let db = store.cache_write().unwrap();
        let key = format!(
            "pin:v1:{}:{}",
            selected.index_generation, selected.index_revision
        );
        assert_eq!(
            db.execute(
                "DELETE FROM revision_capture_inputs WHERE revision_id=?1 AND input_key LIKE 'executable:%'",
                [&key],
            )
            .unwrap(),
            1
        );
    }
    let response = store.evidence_response().unwrap();
    let error = response
        .validate_pin(selected)
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
}

#[test]
fn newer_user_version_is_hard_and_does_not_schedule_recovery() {
    let (_state, _root, store) = published_store(1);
    {
        let db = store.cache_write().unwrap();
        db.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION + 1)
            .unwrap();
    }
    let error = store.report_index_failure(store.status().unwrap_err());
    assert_eq!(recovery_class(&error), RecoveryClass::Hard, "{error:#}");
    assert_eq!(store.disposition(), RecoveryDisposition::Ready);
    assert!(!store.recovery_required.load(Ordering::Acquire));
    assert!(error.to_string().contains("unsupported newer schema"));
}

#[test]
fn native_call_with_deleted_owner_rebuilds_and_repairs() {
    let (_state, root, store) = published_store(1);
    let pin = store.status().unwrap().revision;
    let owner = store
        .native_declarations_at(pin, "javascript", "f0")
        .unwrap()[0]
        .syntax_id
        .clone();
    assert_eq!(store.native_calls_at(pin, &owner).unwrap().len(), 1);
    {
        let db = store.cache_write().unwrap();
        db.pragma_update(None, "foreign_keys", "OFF").unwrap();
        assert_eq!(
            db.execute(
                "DELETE FROM native_version_declarations WHERE syntax_id=?1",
                [id_codec::encode_sid(&owner)],
            )
            .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM native_version_calls", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            1
        );
    }
    let error = store.native_calls_at(pin, &owner).unwrap_err();
    assert_rebuild(&store, &error);
    let repaired = reindex(&store, root.path());
    let owner = store
        .native_declarations_at(repaired, "javascript", "f0")
        .unwrap()[0]
        .syntax_id
        .clone();
    assert_eq!(store.native_calls_at(repaired, &owner).unwrap().len(), 1);
}
