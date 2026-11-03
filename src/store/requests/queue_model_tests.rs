//! Bounded, in-process Decision 0008 queue histories. The model is a plain
//! ordered vector; it deliberately does not query SQLite to decide transitions.
use super::*;
use proptest::prelude::*;
use std::sync::{Arc, atomic::AtomicBool};

#[derive(Clone, Debug)]
enum Op {
    Enqueue(u8),
    Claim,
    Finish(bool),
    Busy,
    Restart,
    FailQueued,
    Read(u8),
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(
        prop_oneof![
            4 => (0u8..3).prop_map(Op::Enqueue),
            3 => Just(Op::Claim),
            3 => any::<bool>().prop_map(Op::Finish),
            2 => Just(Op::Busy),
            2 => Just(Op::Restart),
            1 => Just(Op::FailQueued),
            2 => (0u8..3).prop_map(Op::Read),
        ],
        1..25,
    )
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    Queued,
    Running(u32),
    Done(IndexPin),
    Failed(&'static str),
}

struct Row {
    id: String,
    seq: i64,
    key: u8,
    state: State,
    bytes: u64,
}

fn replay(history: &[Op]) {
    let state_dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.js"), "function a() {}\n").unwrap();
    let store = Store::open_for_tests(state_dir.path(), root.path()).unwrap();
    let options = IndexOptions::new(root.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let (mut pin, owner) =
        crate::index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    let mut owner = Some(owner);
    let mut epoch = 0u32;
    let mut rows: Vec<Row> = Vec::new();
    let mut active: Option<Request> = None;

    for (step, op) in history.iter().enumerate() {
        match *op {
            Op::Enqueue(key) => {
                let mut options = options.clone();
                options.max_file_bytes = 1024 + u64::from(key);
                let row = store.enqueue_request(&options, None).unwrap();
                assert_eq!(row.state, "queued");
                assert!(rows.last().is_none_or(|last| row.seq > last.seq));
                rows.push(Row {
                    id: row.id,
                    seq: row.seq,
                    key,
                    state: State::Queued,
                    bytes: options.max_file_bytes,
                });
            }
            Op::Claim => {
                let first = rows
                    .iter()
                    .position(|r| matches!(r.state, State::Queued | State::Running(_)));
                let expected = first.filter(|&i| rows[i].state != State::Running(epoch));
                let got = store.claim_request(owner.as_ref().unwrap()).unwrap();
                assert_eq!(
                    got.as_ref().map(|r| r.id.as_str()),
                    expected.map(|i| rows[i].id.as_str()),
                    "step {step}, {history:?}"
                );
                if let (Some(i), Some(claim)) = (expected, got) {
                    rows[i].state = State::Running(epoch);
                    active = Some(claim);
                }
            }
            Op::Finish(success) => {
                if let Some(claim) = active.take() {
                    let result = if success {
                        Ok(pin)
                    } else {
                        Err(anyhow::anyhow!("untyped test failure"))
                    };
                    store
                        .record_and_finish_request(owner.as_ref().unwrap(), &claim, result)
                        .unwrap();
                    let row = rows.iter_mut().find(|r| r.id == claim.id).unwrap();
                    row.state = if success {
                        State::Done(pin)
                    } else {
                        State::Failed("index_failed: internal error")
                    };
                }
            }
            Op::Busy => {
                if let Some(claim) = active.take() {
                    store
                        .requeue_busy_claim(owner.as_ref().unwrap(), &claim, Some(pin))
                        .unwrap();
                    rows.iter_mut().find(|r| r.id == claim.id).unwrap().state = State::Queued;
                }
            }
            Op::Restart => {
                drop(owner.take());
                epoch += 1;
                active = None;
                let (new_pin, new_owner) = crate::index_coordinator::reconcile_workspace(
                    &store,
                    &options,
                    &cancel,
                    |_| {},
                )
                .unwrap();
                pin = new_pin;
                owner = Some(new_owner);
            }
            Op::FailQueued => {
                let expected = rows.iter().filter(|r| r.state == State::Queued).count();
                assert_eq!(
                    store.fail_queued_recovery_blocked("test").unwrap(),
                    expected
                );
                for row in &mut rows {
                    if row.state == State::Queued {
                        row.state = State::Failed("recovery_blocked: test");
                    }
                }
            }
            Op::Read(key) => {
                // Each of three keys repeatedly selects the newest matching accepted ID.
                if let Some(row) = rows.iter().rev().find(|r| r.key == key) {
                    assert_eq!(store.request_by_id(&row.id).unwrap().unwrap().seq, row.seq);
                }
            }
        }
        let first = rows
            .iter()
            .find(|r| matches!(r.state, State::Queued | State::Running(_)));
        assert_eq!(
            store
                .earliest_unfinished_request()
                .unwrap()
                .as_ref()
                .map(|r| r.id.as_str()),
            first.map(|r| r.id.as_str()),
            "step {step}, {history:?}"
        );
        assert_eq!(
            store
                .current_request()
                .unwrap()
                .as_ref()
                .map(|r| r.id.as_str()),
            rows.last().map(|r| r.id.as_str())
        );
        for row in &rows {
            let actual = store.request_by_id(&row.id).unwrap().unwrap();
            assert_eq!(actual.seq, row.seq);
            assert_eq!(
                actual.options(root.path()).unwrap().max_file_bytes,
                row.bytes
            );
            let expected_state = match row.state {
                State::Queued => "queued",
                State::Running(_) => "running",
                State::Done(_) => "done",
                State::Failed(_) => "failed",
            };
            assert_eq!(actual.state, expected_state, "step {step}, {history:?}");
            assert_eq!(
                actual.revision,
                match row.state {
                    State::Done(completed) => Some(completed),
                    _ => None,
                }
            );
            assert_eq!(
                actual.error_code.as_deref(),
                match row.state {
                    State::Failed(code) => Some(code),
                    _ => None,
                }
            );
        }
    }
    drop(owner);
    store.retire_checkout_sqlite_witnesses();
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_QUEUE_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(32), .. ProptestConfig::default() })]
    #[test]
    fn queue_histories(history in ops()) {
        replay(&history);
    }
}

#[test]
fn fifo_reclaim_requeue_and_terminal_control() {
    replay(&[
        Op::Enqueue(0),
        Op::Enqueue(1),
        Op::Claim,
        Op::Claim,
        Op::Restart,
        Op::Claim,
        Op::Busy,
        Op::Claim,
        Op::Finish(true),
        Op::Claim,
        Op::Finish(false),
        Op::Claim,
        Op::FailQueued,
    ]);
}

#[test]
fn completed_pin_does_not_follow_restart() {
    replay(&[Op::Enqueue(0), Op::Claim, Op::Finish(true), Op::Restart]);
}

fn replacement_history(keys: &[u8], completed: usize, running: bool) {
    let state_dir = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let old = Store::open_for_tests(state_dir.path(), &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let (pin, owner) =
        crate::index_coordinator::reconcile_workspace(&old, &options, &cancel, |_| {}).unwrap();
    let mut accepted = Vec::new();
    for key in keys {
        let mut opts = options.clone();
        opts.max_file_bytes = 1024 + u64::from(*key);
        accepted.push(old.enqueue_request(&opts, None).unwrap());
    }
    let completed = completed.min(accepted.len());
    for expected in accepted.iter().take(completed) {
        let claim = old.claim_request(&owner).unwrap().unwrap();
        assert_eq!(claim.id, expected.id);
        old.record_and_finish_request(&owner, &claim, Ok(pin))
            .unwrap();
    }
    if running && completed < accepted.len() {
        let claim = old.claim_request(&owner).unwrap().unwrap();
        assert_eq!(claim.id, accepted[completed].id);
    }
    drop(owner);
    fs::rename(&root, parent.path().join("old-workspace")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("b.js"), "function b() {}\n").unwrap();
    old.retire_checkout_sqlite_witnesses();
    drop(old);

    let new = Store::open_for_tests(state_dir.path(), &root).unwrap();
    assert_eq!(
        new.settle_foreign_root_requests().unwrap(),
        accepted.len() - completed
    );
    assert_eq!(new.settle_foreign_root_requests().unwrap(), 0);
    {
        let (_guard, db) = new.request_connection().unwrap();
        for (i, original) in accepted.iter().enumerate() {
            let row = db
                .query_row(
                    &format!("SELECT {COLUMNS} FROM requests WHERE id=?1"),
                    [&original.id],
                    read,
                )
                .unwrap();
            assert_eq!((row.seq, &row.id), (original.seq, &original.id));
            if i < completed {
                assert_eq!(row.state, "done");
                assert_eq!(row.revision, Some(pin));
                assert_eq!(row.error_code, None);
            } else {
                assert_eq!(row.state, "failed");
                assert_eq!(row.error_code.as_deref(), Some("root_changed"));
                assert_eq!(row.revision, None);
            }
        }
    }
    let accepted_new = new
        .enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    let (_, owner) = crate::index_coordinator::reconcile_workspace(
        &new,
        &IndexOptions::new(root),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert_eq!(
        new.claim_request(&owner).unwrap().unwrap().id,
        accepted_new.id
    );
    drop(owner);
    new.retire_checkout_sqlite_witnesses();
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_QUEUE_ROOT_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(16), .. ProptestConfig::default() })]
    #[test]
    fn replacement_settles_only_unfinished(keys in prop::collection::vec(0u8..3, 1..6), completed in 0usize..6, running in any::<bool>()) {
        replacement_history(&keys, completed, running);
    }
}

// Shrunk calibration failure: the oldest request must win even for equal options.
#[test]
fn identical_requests_claim_in_acceptance_order() {
    replay(&[Op::Enqueue(0), Op::Enqueue(0), Op::Claim]);
}
