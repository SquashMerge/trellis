#![cfg(feature = "test-support")]

//! Decision 0006 safety and repeatability at individual maintenance boundaries.
//! Exact scheduling among simultaneously due revisions is deliberately not modeled.
use std::{
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    model::IndexPin,
    store::{MaintenanceOutcome, PinExpired, Store, topology::WorkspaceIdentity},
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Frame {
    outcome: String,
    states: Vec<String>,
    remaining: Vec<i64>,
}

fn next(seed: &mut u64) -> u64 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *seed
}

fn run_case(case: u64, large: bool) -> Vec<Frame> {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    store.set_retention_clock_for_tests(1000, 0);
    store.set_maintenance_unit_budget_for_tests(Duration::ZERO);
    let leader = store.publication_for_tests().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut seed = case + 1;
    let count = if large {
        65
    } else {
        2 + (next(&mut seed) % 5) as usize
    };
    let mut sources: Vec<_> = (0..count)
        .map(|i| format!("function f{i}() {{ return 0; }}\n"))
        .collect();
    let mut snapshots = Vec::new();
    let mut pins = Vec::new();
    let mut pin = store.index_baseline().unwrap();
    // First three supersessions become due separately, then together. The fourth
    // is still in grace throughout maintenance. Small repeated keys create sharing.
    for revision in 0..5 {
        if revision > 0 {
            let i = (next(&mut seed) as usize) % count;
            sources[i] = format!("function f{i}() {{ return {revision}; }}\n");
        }
        for (i, text) in sources.iter().enumerate() {
            fs::write(root.path().join(format!("f{i}.js")), text).unwrap();
        }
        let now = [1000, 1001, 1002, 1003, 1900][revision];
        store.set_retention_clock_for_tests(now, (now - 1000) as u64);
        pin = publish(&store, root.path(), &cancel, pin, &leader);
        pins.push(pin);
        snapshots.push(sources.clone());
    }
    let identity = WorkspaceIdentity::discover(Some(root.path()), root.path()).unwrap();
    let db_path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let observe = || -> (Vec<String>, Vec<i64>) {
        // No observation connection lives across a production transaction.
        let db = rusqlite::Connection::open(&db_path).unwrap();
        let rows: Vec<(String, i64)> = pins[..4]
            .iter()
            .map(|pin| {
                let key = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
                db.query_row(
                    "SELECT state,(SELECT count(*) FROM revision_documents WHERE revision_id=?1) FROM native_revision_supersessions WHERE revision_id=?1",
                    [key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
            })
            .collect();
        rows.into_iter().unzip()
    };
    let permit = store.maintenance_try_enter().unwrap();
    let probe = store.open_maintenance_queue_probe().unwrap();
    let mut frames = Vec::new();
    let mut prior = observe().0;
    for unit in 0..100 {
        // At 1900 the oldest is one second inside grace; at 1901 exactly one
        // is due; 1903 makes three due, while the fourth stays protected.
        let now = if unit == 0 {
            1900
        } else if unit == 1 {
            1901
        } else {
            1903
        };
        store.set_retention_clock_for_tests(now, (now - 1000) as u64);
        let outcome = store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap();
        let (current, remaining) = observe();
        if unit == 0 {
            assert_eq!(outcome, MaintenanceOutcome::Idle, "case {case}");
            assert!(current.iter().all(|s| s == "retained"));
        }
        if unit == 1 {
            assert_eq!(outcome, MaintenanceOutcome::Progress, "case {case}");
            assert!(current[1..].iter().all(|s| s == "retained"));
        }
        assert_eq!(
            current[3], "retained",
            "grace violated: case {case}, unit {unit}"
        );
        for i in 0..4 {
            assert!(
                !(prior[i] == "released" && current[i] != "released"),
                "case {case}, unit {unit}"
            );
        }
        for i in 0..5 {
            if i == 4 || current[i] == "retained" {
                for (j, expected) in snapshots[i].iter().enumerate() {
                    let (_, source) = store
                        .source_at(&format!("f{j}.js"), Some(pins[i]))
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        &source.text, expected,
                        "case {case}, unit {unit}, revision {i}, file {j}"
                    );
                }
            } else {
                let error = store.graph_at(Some(pins[i])).unwrap_err();
                assert!(
                    error.downcast_ref::<PinExpired>().is_some(),
                    "case {case}, unit {unit}: {error:#}"
                );
            }
        }
        frames.push(Frame {
            outcome: format!("{outcome:?}"),
            states: current.clone(),
            remaining,
        });
        prior = current;
        if unit >= 2 && outcome == MaintenanceOutcome::Idle {
            assert_eq!(
                prior,
                ["released", "released", "released", "retained"],
                "case {case}"
            );
            return frames;
        }
        assert_ne!(
            outcome,
            MaintenanceOutcome::Deferred,
            "case {case}, unit {unit}"
        );
    }
    panic!("maintenance did not drain: case {case}, frames {frames:?}");
}

fn publish(
    store: &Store,
    root: &Path,
    cancel: &Arc<AtomicBool>,
    expected: IndexPin,
    leader: &trellis::store::topology::WriterGuard,
) -> IndexPin {
    let options = IndexOptions::new(root.to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), cancel, |_| {}).unwrap();
    store
        .publish_native(&graph, &capture, &native, leader, expected, cancel)
        .unwrap()
}

#[test]
fn release_chunks_preserve_grace_and_are_repeatable() {
    let count = std::env::var("TRELLIS_RELEASE_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let mut units = 0;
    let mut pending_frames = 0;
    for case in 0..count {
        let first = run_case(case, false);
        let second = run_case(case, false);
        assert_eq!(first, second, "repeatability case {case}");
        units += first.len() + second.len();
        pending_frames += 2 * first
            .iter()
            .filter(|f| f.states.iter().any(|s| s == "pending"))
            .count();
    }
    let first = run_case(0, true);
    let second = run_case(0, true);
    assert_eq!(first, second, "multi-chunk repeatability");
    // Coverage control, not a contractual requirement on chunk size or scheduling.
    assert!(
        first
            .iter()
            .any(|f| f.remaining[0] > 0 && f.remaining[0] < 65),
        "no partial manifest observed"
    );
    eprintln!(
        "release cases: {count}; small store histories: {}; small units: {units}; pending frames: {pending_frames}; multi-chunk histories: 2; multi-chunk units per history: {}",
        count * 2,
        first.len()
    );
}
