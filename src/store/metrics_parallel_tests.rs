use super::selected_read_once_tests::published_store;
use super::*;
use std::sync::mpsc;

/// Corrupt one document's stored graph rows so its attestation fails.
fn corrupt(store: &Store, table: &str, column: &str, path: &str) {
    let db = Connection::open(store.roots.index_db(&store.identity)).unwrap();
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    let changed = db
        .execute(
            &format!("UPDATE {table} SET {column}={column}||'x' WHERE projection_id=(SELECT graph_projection_id FROM revision_documents WHERE path=?1)"),
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
    // A valid role change leaves graph derivation intact but fails the worker's
    // native-vs-source attestation with a different error from f0's node check.
    let db = Connection::open(store.roots.index_db(&store.identity)).unwrap();
    let changed = db
        .execute(
            "UPDATE native_version_declarations
             SET role=CASE role WHEN 'production' THEN 'test' ELSE 'production' END
             WHERE version_id=(SELECT document_version_id FROM revision_documents WHERE path='f2.js')",
            [],
        )
        .unwrap();
    assert!(changed > 0, "f2.js has native declarations");
    drop(db);
    // f0 resumes only after f2's worker has returned its distinct failure.
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
fn read_ahead_counts_one_carried_document_at_a_chunk_boundary() {
    let (_state, _root, store) = published_store(12);
    let (_, unbounded) = metrics_with(&store, 3, u64::MAX, &mut || Ok(()));
    let largest = *unbounded.charges.iter().max().unwrap();
    // Each chunk fits one fixture document; fetching the next one crosses
    // its boundary while both pending and current chunks are still held.
    let budget = largest * 2;
    let (payload, trace) = metrics_with(&store, 3, budget, &mut || Ok(()));
    assert_eq!(payload.unwrap().files.len(), 12);
    let boundary = trace
        .boundaries
        .iter()
        .find(|&&(pending, current, carried)| pending > 0 && current > 0 && carried > 0)
        .expect("lookahead crosses a chunk boundary before pending acceptance");
    assert!(boundary.0 + boundary.1 <= budget, "{trace:?}");
    assert!(boundary.2 <= largest, "{trace:?}");
    assert!(
        boundary.0 + boundary.1 + boundary.2 > budget,
        "the carried document crosses the two-chunk budget: {trace:?}"
    );
    assert!(
        trace.max_resident_bytes >= boundary.0 + boundary.1 + boundary.2,
        "peak includes pending + current + carried: {trace:?}"
    );
    assert!(trace.max_resident_bytes <= budget + largest, "{trace:?}");
    assert!(trace.max_resident_documents >= 3, "{trace:?}");
}
