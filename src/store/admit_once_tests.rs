use super::*;
use crate::indexer::IndexOptions;
use std::sync::{Arc, atomic::AtomicBool};

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    std::fs::write(root.join("a.js"), "function Alpha() {}\n").unwrap();
    std::fs::write(root.join("b.py"), "def beta():\n    pass\n").unwrap();
    let store = Store::open_for_tests(state.path(), root).unwrap();
    let (pin, _) = crate::index_coordinator::reconcile_workspace(
        &store,
        &IndexOptions::new(root.to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    (state, workspace, store, pin)
}

fn validations() -> usize {
    STATUS_VALIDATIONS.with(|count| count.get())
}

#[test]
fn a_response_validates_the_index_once() {
    let (_state, _workspace, store, pin) = fixture();
    let before = validations();
    let response = store.evidence_response().unwrap();
    assert_eq!(validations() - before, 1, "admission validates the head");
    response.mcp_language_counts().unwrap();
    response.mcp_find("Alpha", 5, None).unwrap();
    response.mcp_find("Alpha", 5, Some(pin)).unwrap();
    assert!(response.source_at("a.js", None).unwrap().is_some());
    assert!(response.source_at("a.js", Some(pin)).unwrap().is_some());
    response.finish(()).unwrap();
    assert_eq!(
        validations() - before,
        1,
        "in-body reads and finish reuse it"
    );
}

#[test]
fn explicit_pins_keep_their_conflict_semantics() {
    let (_state, _workspace, store, pin) = fixture();
    let response = store.evidence_response().unwrap();
    let pin_at = |generation: &str, revision: u64| -> IndexPin {
        serde_json::from_value(
            serde_json::json!({"indexGeneration": generation, "indexRevision": revision}),
        )
        .unwrap()
    };
    let newer = pin_at(&pin.index_generation.to_string(), pin.index_revision + 1);
    let error = response.selected(Some(newer)).err().unwrap().to_string();
    assert!(error.contains("revision conflict"), "{error}");
    let foreign = pin_at(&uuid::Uuid::new_v4().to_string(), pin.index_revision);
    let error = response.selected(Some(foreign)).err().unwrap().to_string();
    assert!(error.contains("revision conflict"), "{error}");
    assert_eq!(response.selected(Some(pin)).unwrap().pin, pin);
    assert_eq!(response.selected(None).unwrap().pin, pin);
}

#[test]
fn the_fence_detects_a_changed_pin() {
    let (_state, _workspace, store, pin) = fixture();
    let mut response = store.evidence_response().unwrap();
    let mut other = pin;
    other.index_revision += 1;
    response.fence = ReadFence(other);
    let error = response.finish(()).unwrap_err().to_string();
    assert!(
        error.contains("index_not_ready: selected snapshot changed"),
        "{error}"
    );
}
