use super::selected_read_once_tests::published_store;
use super::*;
use std::sync::mpsc;

/// Corrupt one document's stored graph rows so its attestation fails with `expected`.
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

#[test]
fn metrics_documents_are_attested_and_computed_concurrently() {
    let (_state, _root, store) = published_store(3);
    let (started_tx, started_rx) = mpsc::channel::<String>();
    let started_rx = Mutex::new(started_rx);
    store.set_metrics_document_hook(move |path| {
        if path == "f0.js" {
            // f0's work does not finish until f1's has started on another thread.
            let started = started_rx.lock().unwrap();
            loop {
                let other = started
                    .recv_timeout(crate::testing::HANG_BACKSTOP)
                    .expect("f1 started while f0 was held");
                if other == "f1.js" {
                    break;
                }
            }
        } else {
            let _ = started_tx.send(path.to_owned());
        }
    });
    let payload = store
        .evidence_response()
        .unwrap()
        .metrics_at(&MetricsOptions::default(), &mut || Ok(()))
        .unwrap();
    let files: Vec<&str> = payload.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(files, ["f0.js", "f1.js", "f2.js"]);
}

#[test]
fn the_first_failure_in_manifest_order_is_reported() {
    let (_state, _root, store) = published_store(3);
    corrupt(&store, "graph_nodes", "name", "f0.js");
    corrupt(&store, "graph_calls", "caller", "f2.js");
    let (started_tx, started_rx) = mpsc::channel::<String>();
    let started_rx = Mutex::new(started_rx);
    store.set_metrics_document_hook(move |path| {
        if path == "f0.js" {
            // f2 starts, and fails, while f0 is still held.
            let started = started_rx.lock().unwrap();
            loop {
                let other = started
                    .recv_timeout(crate::testing::HANG_BACKSTOP)
                    .expect("f2 started while f0 was held");
                if other == "f2.js" {
                    break;
                }
            }
        } else {
            let _ = started_tx.send(path.to_owned());
        }
    });
    let error = store
        .evidence_response()
        .unwrap()
        .metrics_at(&MetricsOptions::default(), &mut || Ok(()))
        .unwrap_err();
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
    store.set_metrics_document_hook(move |path| {
        if path == "f1.js" {
            set.store(true, Ordering::Release);
        }
    });
    let error = store
        .evidence_response()
        .unwrap()
        .metrics_at(&MetricsOptions::default(), &mut || {
            if expired.load(Ordering::Acquire) {
                anyhow::bail!("deadline_exceeded");
            }
            Ok(())
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "deadline_exceeded");
}
