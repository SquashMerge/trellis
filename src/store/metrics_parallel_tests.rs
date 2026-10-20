use super::selected_read_once_tests::published_store;
use super::*;
use std::sync::mpsc;

/// Corrupt one document's stored graph rows so its attestation fails.
fn corrupt(store: &Store, table: &str, column: &str, path: &str) {
    let db = Connection::open(store.roots.index_db(&store.identity)).unwrap();
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    let changed = db
        .execute(
            &format!("UPDATE {table} SET {column}={column}||'x' WHERE path=?1"),
            [path],
        )
        .unwrap();
    assert!(changed > 0, "{table} has rows for {path}");
}

/// Run one metrics revision with these overrides; return it and what it held.
fn metrics_with(
    store: &Store,
    workers: usize,
    fetch_bytes: u64,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> (Result<MetricsPayload>, MetricsFetchTrace) {
    METRICS_TEST_PLAN.set(Some(MetricsTestPlan {
        workers,
        fetch_bytes,
    }));
    METRICS_FETCH_TRACE.take();
    let result = store
        .evidence_response()
        .unwrap()
        .metrics_at(&MetricsOptions::default(), check);
    METRICS_TEST_PLAN.set(None);
    (result, METRICS_FETCH_TRACE.take())
}

/// Hold `held`'s work at its start until `other` reaches `stage`.
fn hold_until(store: &Store, held: &'static str, other: &'static str, stage: MetricsDocumentStage) {
    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let reached_rx = Mutex::new(reached_rx);
    store.set_metrics_document_hook(move |path, at| {
        if path == held && at == MetricsDocumentStage::Start {
            reached_rx
                .lock()
                .unwrap()
                .recv_timeout(crate::testing::HANG_BACKSTOP)
                .unwrap_or_else(|_| panic!("{other} reached {stage:?} while {held} was held"));
        } else if path == other && at == stage {
            let _ = reached_tx.send(());
        }
    });
}

#[test]
fn metrics_documents_are_attested_and_computed_concurrently() {
    let (_state, _root, store) = published_store(3);
    hold_until(&store, "f0.js", "f1.js", MetricsDocumentStage::Start);
    let (payload, _) = metrics_with(&store, 3, u64::MAX, &mut || Ok(()));
    let payload = payload.unwrap();
    let files: Vec<&str> = payload.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(files, ["f0.js", "f1.js", "f2.js"]);
}

#[test]
fn the_first_failure_in_manifest_order_is_reported_when_a_later_one_fails_first() {
    let (_state, _root, store) = published_store(3);
    corrupt(&store, "graph_nodes", "name", "f0.js");
    corrupt(&store, "graph_calls", "caller", "f2.js");
    // f0 resumes only after f2's work has returned its failure.
    hold_until(&store, "f0.js", "f2.js", MetricsDocumentStage::Done);
    let (result, _) = metrics_with(&store, 3, u64::MAX, &mut || Ok(()));
    let error = result.unwrap_err();
    assert!(
        format!("{error:#}").contains("selected graph node materialization differs"),
        "{error:#}"
    );
}

#[test]
fn a_check_failure_on_a_worker_is_the_typed_result() {
    let (_state, _root, store) = published_store(3);
    let expired = Arc::new(AtomicBool::new(false));
    let set = expired.clone();
    store.set_metrics_document_hook(move |path, at| {
        if path == "f1.js" && at == MetricsDocumentStage::Start {
            set.store(true, Ordering::Release);
        }
    });
    let (result, _) = metrics_with(&store, 3, u64::MAX, &mut || {
        if expired.load(Ordering::Acquire) {
            anyhow::bail!("deadline_exceeded");
        }
        Ok(())
    });
    assert_eq!(result.unwrap_err().to_string(), "deadline_exceeded");
}

#[test]
fn a_document_over_the_read_ahead_budget_is_held_alone() {
    let (_state, _root, store) = published_store(6);
    // Every document is larger than the whole budget.
    let (payload, trace) = metrics_with(&store, 3, 1, &mut || Ok(()));
    assert_eq!(payload.unwrap().files.len(), 6);
    assert_eq!(trace.max_resident_documents, 1, "{trace:?}");
}

#[test]
fn read_ahead_never_holds_more_than_its_budget() {
    let (_state, _root, store) = published_store(12);
    let (_, unbounded) = metrics_with(&store, 3, u64::MAX, &mut || Ok(()));
    let largest = *unbounded.charges.iter().max().unwrap();
    // Room for about three documents across both read-ahead chunks.
    let budget = largest * 3;
    let (payload, trace) = metrics_with(&store, 3, budget, &mut || Ok(()));
    assert_eq!(payload.unwrap().files.len(), 12);
    assert!(trace.max_resident_bytes <= budget, "{trace:?}");
    assert!(
        trace.max_resident_documents >= 2,
        "documents still overlap: {trace:?}"
    );
}
