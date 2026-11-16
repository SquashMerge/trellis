use super::unit_read::UnitFailure;
use super::*;
use std::sync::atomic::AtomicBool;

fn pin(revision: u64) -> IndexPin {
    IndexPin {
        index_generation: uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
        index_revision: revision,
    }
}

fn selection(producer: &str) -> PendingSemanticSelection {
    PendingSemanticSelection {
        producer_id: producer.into(),
        batch_id: 1,
        captured_pin: pin(1),
        basis_pin: pin(1),
    }
}

/// The captured pin a snapshot handle reads is one of the pending target's own selections:
/// any other selection is refused before a snapshot is opened.
#[test]
fn a_selection_that_is_not_the_targets_is_refused() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store =
        crate::testing::retry_admission(|| Store::open_for_tests(state.path(), workspace.path()));
    let target = PendingSemanticTarget {
        pin: pin(2),
        native_revision_id: "native-2".into(),
        root_id: "root".into(),
        selections: vec![selection("producer")],
    };
    let held = store.open_semantic_read();
    let mut foreign = selection("producer");
    foreign.captured_pin = pin(3);
    let mut opened = false;
    let error = store
        .with_pending_semantic_read(&target, &foreign, &held, |_| {
            opened = true;
            Ok(())
        })
        .unwrap_err();
    assert!(!opened);
    assert_eq!(
        error.to_string(),
        "semantic selection is not one of the pending target's"
    );
}

fn terminal(error: anyhow::Error, cancel: &CancelFlag) -> Option<anyhow::Error> {
    match UnitFailure::of(error, cancel) {
        UnitFailure::Terminal(error) => Some(error),
        UnitFailure::Content(_) => None,
    }
}

#[test]
fn a_failed_step_is_terminal_for_storage_and_integrity_and_content_otherwise() {
    let running: CancelFlag = Arc::new(AtomicBool::new(false));
    let busy = terminal(SqliteContention(()).into(), &running).unwrap();
    assert!(busy.is::<SqliteContention>());
    let corrupt = terminal(
        SelectedIntegrity("incompatible_index: test".into()).into(),
        &running,
    )
    .unwrap();
    assert!(corrupt.is::<SelectedIntegrity>());
    // A raw SQLite error leaves typed, never as the raw error.
    let sqlite = terminal(
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), None)
            .into(),
        &running,
    )
    .unwrap();
    assert!(!sqlite.is::<rusqlite::Error>());
    assert!(terminal(std::io::Error::other("disk").into(), &running).is_some());
    #[cfg(feature = "test-support")]
    assert!(
        terminal(
            anyhow::anyhow!("rows differ").context(RejoinOracleMismatch),
            &running
        )
        .unwrap()
        .is::<RejoinOracleMismatch>()
    );
    // A producer's content failure and a witness fallback stay with the scheduler.
    assert!(terminal(anyhow::anyhow!("retained artifact digest"), &running).is_none());
    assert!(
        terminal(
            crate::store::staging::WitnessFallback("required raw document missing").into(),
            &running
        )
        .is_none()
    );
    // Cancellation wins over every other reading of the failure.
    let cancelled: CancelFlag = Arc::new(AtomicBool::new(true));
    let error = terminal(anyhow::anyhow!("retained artifact digest"), &cancelled).unwrap();
    assert_eq!(error.to_string(), "index publication cancelled");
}
