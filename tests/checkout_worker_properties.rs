#![cfg(feature = "test-support")]

use proptest::{prelude::*, test_runner::TestRunner};
use std::{fs, sync::Arc, time::Duration};
use trellis::{
    daemon::registry::{CheckoutEvidence, CheckoutRegistry},
    http::RuntimePhase,
    indexer::IndexOptions,
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

#[derive(Clone, Debug)]
enum Op {
    Read(usize),
    DropRead(usize),
    Attach(usize),
    Detach(usize),
    Release,
    Enqueue(usize),
}

fn operation() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0usize..3).prop_map(Op::Read),
        (0usize..3).prop_map(Op::DropRead),
        (0usize..3).prop_map(Op::Attach),
        (0usize..3).prop_map(Op::Detach),
        Just(Op::Release),
        (0usize..3).prop_map(Op::Enqueue),
    ]
}

// This is the contract at the held retirement boundary, independent of the
// scheduler's state and counters. A committed direct-Store enqueue is checked
// when the worker resumes, rather than assumed to cancel synchronously.
#[derive(Default)]
struct Model {
    requested: bool,
    reads: [bool; 3],
    clients: [bool; 3],
    requests: usize,
}

#[derive(Default, Debug)]
struct Counts {
    operations: [usize; 6],
    read_cancel: usize,
    attach_cancel: usize,
    enqueue_requested: usize,
    blocked_by_read: usize,
    blocked_by_client: usize,
    renew_retirement: usize,
    multiple_requests: usize,
}

impl Counts {
    fn add(&mut self, other: Self) {
        for (a, b) in self.operations.iter_mut().zip(other.operations) {
            *a += b;
        }
        self.read_cancel += other.read_cancel;
        self.attach_cancel += other.attach_cancel;
        self.enqueue_requested += other.enqueue_requested;
        self.blocked_by_read += other.blocked_by_read;
        self.blocked_by_client += other.blocked_by_client;
        self.renew_retirement += other.renew_retirement;
        self.multiple_requests += other.multiple_requests;
    }
}

impl Model {
    fn apply(&mut self, op: &Op) {
        match *op {
            Op::Read(k) => {
                self.reads[k] = true;
                self.requested = false;
            }
            Op::DropRead(k) => self.reads[k] = false,
            Op::Attach(k) => {
                self.clients[k] = true;
                self.requested = false;
            }
            Op::Detach(k) => self.clients[k] = false,
            Op::Release
                if !self.reads.contains(&true)
                    && !self.clients.contains(&true)
                    && self.requests == 0 =>
            {
                self.requested = true
            }
            Op::Release => {}
            Op::Enqueue(_) => self.requests += 1,
        }
    }
}

async fn ready(runtime: &trellis::daemon::registry::CheckoutRuntime) {
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((read, false)) = runtime.evidence_response() {
                read.finish(()).unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("startup did not finish");
}

async fn history(ops: &[Op]) -> Counts {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("work");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let id = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(roots.clone());
    registry.attach_launch(1, &id).unwrap();
    let runtime = registry.activate(&id.root_key).unwrap();
    ready(&runtime).await;
    registry.disconnect(1);
    let scheduler = runtime.browser_scheduler().unwrap();
    let store = Store::open(
        roots,
        WorkspaceIdentity::discover(Some(&root), &root)
            .unwrap()
            .attach_marker()
            .unwrap(),
    )
    .unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel::<()>(1);
    scheduler.set_retire_hook_for_tests(move || {
        let _ = entered_tx.send(());
        let _ = resume_rx.recv();
    });
    assert!(!registry.release(&id.root_key).unwrap());
    entered_rx
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("retirement hook not reached");
    let mut model = Model {
        requested: true,
        ..Default::default()
    };
    let mut reads: [Option<CheckoutEvidence>; 3] = [None, None, None];
    let mut rows = Vec::new();
    let mut counts = Counts::default();
    for op in ops {
        match op {
            Op::Read(_) => {
                counts.operations[0] += 1;
                counts.read_cancel += usize::from(model.requested);
            }
            Op::DropRead(_) => counts.operations[1] += 1,
            Op::Attach(_) => {
                counts.operations[2] += 1;
                counts.attach_cancel += usize::from(model.requested);
            }
            Op::Detach(_) => counts.operations[3] += 1,
            Op::Release if model.requests == 0 => {
                counts.operations[4] += 1;
                counts.blocked_by_read += usize::from(model.reads.contains(&true));
                counts.blocked_by_client += usize::from(model.clients.contains(&true));
                counts.renew_retirement += usize::from(
                    !model.requested
                        && !model.reads.contains(&true)
                        && !model.clients.contains(&true),
                );
            }
            Op::Release => {}
            Op::Enqueue(_) => {
                counts.operations[5] += 1;
                counts.enqueue_requested += usize::from(model.requested);
            }
        }
        match *op {
            Op::Read(k) => {
                let (read, _) = runtime.evidence_response().unwrap();
                assert!(read.source_at("a.js", None).unwrap().is_some());
                read.finish(()).unwrap();
                reads[k] = Some(read);
            }
            Op::DropRead(k) => reads[k] = None,
            Op::Attach(k) => {
                registry.attach_launch(10 + k as u64, &id).unwrap();
                let attached = registry.activate(&id.root_key).unwrap();
                assert!(Arc::ptr_eq(&runtime, &attached));
            }
            Op::Detach(k) => registry.disconnect(10 + k as u64),
            Op::Release if model.requests == 0 => assert!(!registry.release(&id.root_key).unwrap()),
            Op::Release => continue,
            Op::Enqueue(k) => {
                let mut options = IndexOptions::new(root.clone());
                // Three repeated keys still produce distinct durable requests.
                options.max_file_bytes += k as u64;
                rows.push(store.enqueue_request(&options, None).unwrap().id);
            }
        }
        model.apply(op);
        let expected = if model.requested {
            RuntimePhase::RetireRequested
        } else {
            RuntimePhase::Ready
        };
        assert_eq!(
            scheduler.runtime_phase(),
            expected,
            "after {op:?} in {ops:?}"
        );
        assert!(runtime.has_active_resources());
    }
    counts.multiple_requests = usize::from(rows.len() > 1);
    drop(reads);
    for k in 0..3 {
        registry.disconnect(10 + k);
    }
    resume_tx.send(()).unwrap();
    if !rows.is_empty() {
        tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            loop {
                if rows
                    .iter()
                    .all(|id| store.request_by_id(id).unwrap().unwrap().state == "done")
                {
                    break;
                }
                assert_ne!(
                    scheduler.runtime_phase(),
                    RuntimePhase::Retired,
                    "worker retired with unfinished requests: {ops:?}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("accepted requests did not all complete");
        assert_eq!(
            scheduler.runtime_phase(),
            RuntimePhase::Ready,
            "enqueue failed to cancel retirement: {ops:?}"
        );
    } else if model.requested {
        let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            let version = scheduler.state_version();
            if scheduler.runtime_phase() == RuntimePhase::Retired {
                break;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "worker did not retire: {ops:?}");
            scheduler.wait_state_change(version, remaining);
        }
    } else {
        assert_eq!(scheduler.runtime_phase(), RuntimePhase::Ready);
    }
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while !registry.release(&id.root_key).unwrap() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("final release failed");
    assert!(!runtime.has_active_resources());
    counts
}

#[test]
fn retirement_histories() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cases = std::env::var("TRELLIS_WORKER_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24);
    let mut runner = TestRunner::new(proptest::test_runner::Config {
        cases,
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/checkout_worker_properties.txt",
            ),
        )),
        source_file: Some(file!()),
        test_name: Some("retirement_histories"),
        ..Default::default()
    });
    let counts = std::cell::RefCell::new(Counts::default());
    let general = proptest::collection::vec(operation(), 1..12);
    let renewal =
        (0usize..3, proptest::collection::vec(operation(), 0..8)).prop_map(|(key, suffix)| {
            let mut ops = vec![Op::Read(key), Op::DropRead(key), Op::Release];
            ops.extend(suffix);
            ops
        });
    runner
        .run(&prop_oneof![general, renewal], |ops| {
            counts.borrow_mut().add(runtime.block_on(history(&ops)));
            Ok(())
        })
        .unwrap();
    eprintln!("worker history coverage: {:?}", counts.into_inner());
}

#[test]
fn retirement_boundary_controls() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for ops in [
        vec![
            Op::Read(0),
            Op::Read(1),
            Op::DropRead(0),
            Op::Release,
            Op::DropRead(1),
            Op::Release,
        ],
        vec![
            Op::Attach(0),
            Op::Attach(1),
            Op::Detach(0),
            Op::Release,
            Op::Detach(1),
            Op::Release,
        ],
        vec![Op::Enqueue(0), Op::Enqueue(1)],
        vec![Op::Read(0), Op::DropRead(0), Op::Release, Op::Enqueue(2)],
    ] {
        let _ = runtime.block_on(history(&ops));
    }
}

#[test]
fn enqueue_after_closing_is_handled_by_a_fresh_runtime() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let base = tempfile::tempdir().unwrap();
            let root = base.path().join("work");
            fs::create_dir(&root).unwrap();
            fs::write(root.join("a.js"), "function a() {}\n").unwrap();
            let id = WorkspaceIdentity::discover(Some(&root), &root)
                .unwrap()
                .attach_marker()
                .unwrap();
            let roots = TopologyRoots::isolated_for_tests(
                base.path().join("cache"),
                base.path().join("data"),
            );
            let mut registry = CheckoutRegistry::with_roots(roots.clone());
            registry.attach_launch(1, &id).unwrap();
            let old = registry.activate(&id.root_key).unwrap();
            ready(&old).await;
            registry.disconnect(1);
            let store = Store::open(
                roots,
                WorkspaceIdentity::discover(Some(&root), &root)
                    .unwrap()
                    .attach_marker()
                    .unwrap(),
            )
            .unwrap();
            let scheduler = old.browser_scheduler().unwrap();
            let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
            let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel::<()>(1);
            scheduler.set_closing_hook_for_tests(move || {
                let _ = entered_tx.send(());
                let _ = resume_rx.recv();
            });
            assert!(!registry.release(&id.root_key).unwrap());
            entered_rx
                .recv_timeout(trellis::testing::HANG_BACKSTOP)
                .expect("closing hook not reached");
            assert_eq!(scheduler.runtime_phase(), RuntimePhase::Closing);
            assert!(old.evidence_response().is_err());
            let row = store
                .enqueue_request(&IndexOptions::new(root), None)
                .unwrap();
            assert_eq!(
                store.request_by_id(&row.id).unwrap().unwrap().state,
                "queued"
            );
            assert_eq!(scheduler.runtime_phase(), RuntimePhase::Closing);
            resume_tx.send(()).unwrap();
            registry.attach_launch(2, &id).unwrap();
            let fresh = registry.activate(&id.root_key).unwrap();
            assert!(!Arc::ptr_eq(&old, &fresh));
            ready(&fresh).await;
            tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
                loop {
                    if store.request_by_id(&row.id).unwrap().unwrap().state == "done" {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("fresh runtime did not finish the accepted request");
            assert!(!old.has_active_resources());
            registry.disconnect(2);
            tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
                while !registry.release(&id.root_key).unwrap() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("fresh runtime did not retire");
        });
}
