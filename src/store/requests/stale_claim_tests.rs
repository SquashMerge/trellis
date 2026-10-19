use super::*;
#[test]
fn busy_result_can_never_become_terminal_index_failed() {
    for reason in [
        "storage_busy: SQLite lock contention",
        "storage_busy: cached claim changed",
    ] {
        assert!(
            CompletionOutcome::from_result(Err(anyhow::anyhow!(reason))).is_err(),
            "neither transient busy nor invariant busy may mint terminal failure"
        );
    }
}
#[test]
fn stale_claim_cannot_complete_a_row_after_new_leader_reclaims_it() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let (_, old_owner) = crate::index_coordinator::reconcile_workspace(
        &store,
        &options,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let accepted = store.enqueue_request(&options, None).unwrap();
    let stale_claim = store.claim_request(&old_owner).unwrap().unwrap();
    assert_eq!(stale_claim.id, accepted.id);
    drop(old_owner);
    let replacement = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let (new_pin, new_owner) = crate::index_coordinator::reconcile_workspace(
        &replacement,
        &options,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let new_claim = replacement.claim_request(&new_owner).unwrap().unwrap();
    assert_ne!(stale_claim.claim_incarnation, new_claim.claim_incarnation);
    let stale_retry = replacement
        .requeue_busy_claim(&new_owner, &stale_claim, Some(new_pin))
        .unwrap_err();
    assert!(
        stale_retry.to_string().contains("claim changed"),
        "{stale_retry:#}"
    );
    let still_new = replacement.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(still_new.state, "running");
    assert_eq!(still_new.claim_incarnation, new_claim.claim_incarnation);
    let error = replacement
        .record_and_finish_request(&new_owner, &stale_claim, Ok(new_pin))
        .unwrap_err();
    assert!(
        error.to_string().contains("request claim changed"),
        "{error:#}"
    );
    let still_running = replacement.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(still_running.state, "running");
    assert_eq!(still_running.claim_incarnation, new_claim.claim_incarnation);
    assert!(still_running.revision.is_none() && still_running.error_code.is_none());
    replacement
        .record_and_finish_request(&new_owner, &new_claim, Ok(new_pin))
        .unwrap();
    assert_eq!(
        replacement
            .request_by_id(&accepted.id)
            .unwrap()
            .unwrap()
            .state,
        "done"
    );
}
