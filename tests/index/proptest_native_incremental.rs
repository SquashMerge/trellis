//! Incremental native histories against a fresh same-root cold publication.
use super::{index_dir, sqlite_snapshot};
use notify::{
    Event, EventKind,
    event::{CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode},
};
use proptest::prelude::*;
use std::{
    fs,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use trellis::{
    index_coordinator::{IndexJobCoordinator, WatcherWork},
    indexer::IndexOptions,
    store::Store,
};

static LOCAL: AtomicUsize = AtomicUsize::new(0);
static FULL: AtomicUsize = AtomicUsize::new(0);
static STEPS: AtomicUsize = AtomicUsize::new(0);
static OPS: [AtomicUsize; 9] = [const { AtomicUsize::new(0) }; 9];
static CHANGED: AtomicUsize = AtomicUsize::new(0);
static UNCHANGED: AtomicUsize = AtomicUsize::new(0);
static RECOVERIES: AtomicUsize = AtomicUsize::new(0);
static WATCH_PUBLISHED: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Debug)]
enum Op {
    Edit(u8, u8),
    Add(u8, u8),
    Remove(u8),
    Rename(u8),
    Atomic(u8, u8),
    Restore(u8),
    Touch(u8),
    Multi(u8),
    Partial(u8),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0u8..2, 0u8..3).prop_map(|(p,v)| Op::Edit(p,v)),
        2 => (0u8..2, 0u8..3).prop_map(|(p,v)| Op::Add(p,v)),
        2 => (0u8..2).prop_map(Op::Remove),
        2 => (0u8..2).prop_map(Op::Rename),
        2 => (0u8..2, 0u8..3).prop_map(|(p,v)| Op::Atomic(p,v)),
        2 => (0u8..2).prop_map(Op::Restore),
        2 => (0u8..2).prop_map(Op::Touch),
        2 => (0u8..3).prop_map(Op::Multi),
        2 => (0u8..2).prop_map(Op::Partial),
    ]
}

fn path(root: &Path, n: u8) -> std::path::PathBuf {
    root.join(if n == 0 { "a.js" } else { "b.js" })
}
fn source(n: u8, version: u8) -> String {
    let name = ["alpha", "beta", "gamma"][version as usize];
    format!(
        "export function {name}{n}(value) {{ if (value) {{ helper(value); }} return value + {version}; }}\nfunction helper(value) {{ return value; }}\n"
    )
}
fn apply(root: &Path, operation: &Op) -> Event {
    let modify = || Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)));
    match *operation {
        Op::Edit(n, _) | Op::Add(n, _) | Op::Restore(n) | Op::Partial(n) => {
            let p = path(root, n);
            let existed = p.exists();
            let text = match *operation {
                Op::Edit(_, v) | Op::Add(_, v) => source(n, v),
                Op::Restore(_) => source(n, 0),
                _ => format!("function broken{n}(value) {{ return ( ; }}\n"),
            };
            fs::write(&p, text).unwrap();
            if existed {
                modify().add_path(p)
            } else {
                Event::new(EventKind::Create(CreateKind::File)).add_path(p)
            }
        }
        Op::Remove(n) => {
            let p = path(root, n);
            if p.exists() {
                fs::remove_file(&p).unwrap();
            }
            Event::new(EventKind::Remove(RemoveKind::File)).add_path(p)
        }
        Op::Rename(n) => {
            let from = path(root, n);
            let to = path(root, 1 - n);
            if from.exists() {
                fs::rename(&from, &to).unwrap();
            }
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(from)
                .add_path(to)
        }
        Op::Atomic(n, v) => {
            let p = path(root, n);
            let tmp = root.join("save.tmp");
            fs::write(&tmp, source(n, v)).unwrap();
            fs::rename(&tmp, &p).unwrap();
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(tmp)
                .add_path(p)
        }
        Op::Touch(n) => {
            let p = path(root, n);
            if let Ok(bytes) = fs::read(&p) {
                fs::write(&p, bytes).unwrap();
            }
            modify().add_path(p)
        }
        Op::Multi(v) => {
            let a = path(root, 0);
            let b = path(root, 1);
            fs::write(&a, source(0, v)).unwrap();
            fs::write(&b, source(1, (v + 1) % 3)).unwrap();
            modify().add_path(a).add_path(b)
        }
    }
}

fn compare(
    store: &Store,
    state: &Path,
    pin: trellis::model::IndexPin,
    cold: &Store,
    cold_state: &Path,
    cold_pin: trellis::model::IndexPin,
) {
    let (artifact, sources) = store.retained_native_snapshot_at(pin).unwrap();
    let (cold_artifact, cold_sources) = cold.retained_native_snapshot_at(cold_pin).unwrap();
    assert!(
        artifact == cold_artifact,
        "retained native artifact differs"
    );
    assert_eq!(sources, cold_sources, "retained native source map");
    let actual = sqlite_snapshot(&index_dir(state).join("index.db"), pin, true);
    let expected = sqlite_snapshot(&index_dir(cold_state).join("index.db"), cold_pin, true);
    assert_eq!(actual.len(), expected.len());
    for ((name, rows), (cold_name, cold_rows)) in actual.iter().zip(&expected) {
        assert_eq!(name, cold_name);
        assert_eq!(rows, cold_rows, "complete native snapshot table {name}");
    }
    let graph = store.graph_at(Some(pin)).unwrap();
    assert_eq!(graph, cold.graph_at(Some(cold_pin)).unwrap(), "graph");
    for p in std::iter::once(None).chain(graph.files.iter().map(|f| Some(f.path.as_str()))) {
        let page = |s: &Store, pin| {
            let mut v =
                serde_json::to_value(s.classes_at(p, "", Some(pin), 0, 100).unwrap()).unwrap();
            v.as_object_mut().unwrap().remove("revision");
            v
        };
        assert_eq!(page(store, pin), page(cold, cold_pin), "classes {p:?}");
    }
    for name in ["a.js", "b.js", "Types.java", "gap.rs"] {
        assert_eq!(
            store.source_at(name, Some(pin)).unwrap().map(|(_, s)| s),
            cold.source_at(name, Some(cold_pin))
                .unwrap()
                .map(|(_, s)| s),
            "source {name}"
        );
    }
}

fn history(ops: &[Op]) {
    let root = tempfile::tempdir().unwrap();
    let watcher_state = tempfile::tempdir().unwrap();
    let local_state = tempfile::tempdir().unwrap();
    fs::write(path(root.path(), 0), source(0, 0)).unwrap();
    fs::write(path(root.path(), 1), source(1, 1)).unwrap();
    fs::write(
        root.path().join("Types.java"),
        "class Stable { Other ref; }\nclass Other {}\n",
    )
    .unwrap();
    fs::write(
        root.path().join("gap.rs"),
        "fn gap(v: i32) -> i32 { opaque!(hidden); v }\n",
    )
    .unwrap();
    let options = IndexOptions::new(root.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let watcher = Store::open_for_tests(watcher_state.path(), root.path()).unwrap();
    let owner =
        trellis::index_coordinator::establish_serving_session(&watcher, Some(&options), &cancel)
            .unwrap();
    let mut work = WatcherWork::new(&watcher, &owner, &options).unwrap();
    work.disable_native_watcher_for_tests();
    assert!(
        work.reconcile_due(&watcher, &owner, &options, &cancel, true)
            .unwrap()
    );
    work.delay_periodic_inventory_for_tests();
    let local = Store::open_for_tests(local_state.path(), root.path()).unwrap();
    let job = IndexJobCoordinator::prepare(&local, None).unwrap();
    let session = job.session();
    let mut local_pin = job.run(&options, &cancel, |_| {}).unwrap();
    let old_watch = watcher.status().unwrap().revision;
    let old_local = local_pin;
    let old_watch_snapshot = watcher.retained_native_snapshot_at(old_watch).unwrap();
    let old_local_snapshot = local.retained_native_snapshot_at(old_local).unwrap();
    let old_watch_sql = sqlite_snapshot(
        &index_dir(watcher_state.path()).join("index.db"),
        old_watch,
        false,
    );
    let old_local_sql = sqlite_snapshot(
        &index_dir(local_state.path()).join("index.db"),
        old_local,
        false,
    );
    let measured =
        trellis::indexer::index_workspace_bundle(&options, local.root_id(), &cancel, |_| {})
            .unwrap()
            .1;
    assert!(
        !measured.declarations.is_empty()
            && !measured.calls.is_empty()
            && !measured.references.is_empty()
    );
    assert!(
        measured
            .capability_reports
            .iter()
            .any(|r| r.families.iter().any(|f| !f.gaps.is_empty()))
    );
    for (step, operation) in ops.iter().enumerate() {
        let before: Vec<_> = (0..2)
            .map(|n| fs::read(path(root.path(), n)).ok())
            .collect();
        let watch_before = watcher.status().unwrap().revision;
        let event = apply(root.path(), operation);
        let after: Vec<_> = (0..2)
            .map(|n| fs::read(path(root.path(), n)).ok())
            .collect();
        let kind = match operation {
            Op::Edit(..) => 0,
            Op::Add(..) => 1,
            Op::Remove(..) => 2,
            Op::Rename(..) => 3,
            Op::Atomic(..) => 4,
            Op::Restore(..) => 5,
            Op::Touch(..) => 6,
            Op::Multi(..) => 7,
            Op::Partial(..) => 8,
        };
        OPS[kind].fetch_add(1, Ordering::Relaxed);
        if before == after {
            UNCHANGED.fetch_add(1, Ordering::Relaxed);
        } else {
            CHANGED.fetch_add(1, Ordering::Relaxed);
        }
        for (a, b) in before.iter().zip(&after) {
            if a.as_ref()
                .is_some_and(|s| s.starts_with(b"function broken"))
                && b.as_ref()
                    .is_some_and(|s| !s.starts_with(b"function broken"))
            {
                RECOVERIES.fetch_add(1, Ordering::Relaxed);
            }
        }
        work.submit_watch_event_for_tests(Ok(event));
        assert!(
            work.accepted_watch_intent(&options),
            "hint must enter bounded ingress"
        );
        assert!(
            work.reconcile_due(&watcher, &owner, &options, &cancel, true)
                .unwrap(),
            "watch accounting {step}: {operation:?}"
        );
        assert!(
            !work.accepted_watch_intent(&options),
            "hint must be acknowledged"
        );
        let expected_partial = after
            .iter()
            .filter(|s| {
                s.as_ref()
                    .is_some_and(|bytes| bytes.starts_with(b"function broken"))
            })
            .count();
        assert_eq!(
            watcher.status().unwrap().stats.parse_error_files as usize,
            expected_partial,
            "partial/recovery accounting"
        );
        if watcher.status().unwrap().revision != watch_before {
            WATCH_PUBLISHED.fetch_add(1, Ordering::Relaxed);
        }
        let phases = Mutex::new(Vec::new());
        local_pin =
            IndexJobCoordinator::prepare_with_session(&local, Some(local_pin), session.clone())
                .unwrap()
                .run(&options, &cancel, |p| phases.lock().unwrap().push(p.phase))
                .unwrap();
        let phases = phases.into_inner().unwrap();
        let local_mode = phases.iter().any(|p| p == "mode:local");
        let full_mode = phases.iter().any(|p| p == "mode:full");
        LOCAL.fetch_add(usize::from(local_mode), Ordering::Relaxed);
        FULL.fetch_add(usize::from(full_mode), Ordering::Relaxed);
        assert!(local_mode, "step {step} {operation:?}: {phases:?}");
        let cold_state = tempfile::tempdir().unwrap();
        let cold = Store::open_for_tests(cold_state.path(), root.path()).unwrap();
        let cold_job = IndexJobCoordinator::prepare(&cold, None).unwrap();
        let _cold_session = cold_job.session();
        let cold_pin = cold_job.run(&options, &cancel, |_| {}).unwrap();
        compare(
            &watcher,
            watcher_state.path(),
            watcher.status().unwrap().revision,
            &cold,
            cold_state.path(),
            cold_pin,
        );
        compare(
            &local,
            local_state.path(),
            local_pin,
            &cold,
            cold_state.path(),
            cold_pin,
        );
        assert_eq!(
            sqlite_snapshot(
                &index_dir(watcher_state.path()).join("index.db"),
                old_watch,
                false
            ),
            old_watch_sql,
            "old watcher pin"
        );
        assert_eq!(
            sqlite_snapshot(
                &index_dir(local_state.path()).join("index.db"),
                old_local,
                false
            ),
            old_local_sql,
            "old local pin"
        );
        assert_eq!(
            watcher.retained_native_snapshot_at(old_watch).unwrap(),
            old_watch_snapshot
        );
        assert_eq!(
            local.retained_native_snapshot_at(old_local).unwrap(),
            old_local_snapshot
        );
        STEPS.fetch_add(1, Ordering::Relaxed);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 4, .. ProptestConfig::default() })]
    #[test]
    fn generated_native_histories_match_cold(ops in proptest::collection::vec(op(), 1..5)) {
        history(&ops);
        eprintln!("native-incremental totals: steps={} local={} full={} changed={} noop={} watcher_published={} recoveries={} ops(edit,add,remove,rename,atomic,restore,touch,multi,partial)={:?}", STEPS.load(Ordering::Relaxed), LOCAL.load(Ordering::Relaxed), FULL.load(Ordering::Relaxed), CHANGED.load(Ordering::Relaxed), UNCHANGED.load(Ordering::Relaxed), WATCH_PUBLISHED.load(Ordering::Relaxed), RECOVERIES.load(Ordering::Relaxed), OPS.each_ref().map(|v| v.load(Ordering::Relaxed)));
    }
}

#[test]
fn partial_then_restore_matches_cold() {
    history(&[Op::Partial(0), Op::Restore(0)]);
}

#[test]
fn cutoff_rejects_capture_then_accounts_fresh_bytes() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let p = path(root.path(), 0);
    fs::write(&p, source(0, 0)).unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let options = IndexOptions::new(root.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let session =
        trellis::index_coordinator::establish_serving_session(&store, Some(&options), &cancel)
            .unwrap();
    let mut work = WatcherWork::new(&store, &session, &options).unwrap();
    work.disable_native_watcher_for_tests();
    assert!(
        work.reconcile_due(&store, &session, &options, &cancel, true)
            .unwrap()
    );
    work.delay_periodic_inventory_for_tests();
    let old = store.status().unwrap().revision;
    fs::write(&p, source(0, 1)).unwrap();
    let event =
        || Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content))).add_path(p.clone());
    work.submit_watch_event_for_tests(Ok(event()));
    let accounted = work
        .reconcile_due_observed(&store, &session, &options, &cancel, true, |_, watch| {
            fs::write(&p, source(0, 2)).unwrap();
            watch.submit_event(Ok(event()));
        })
        .unwrap();
    assert!(!accounted);
    assert_eq!(store.status().unwrap().revision, old);
    assert!(work.accepted_watch_intent(&options));
    assert!(
        work.reconcile_due(&store, &session, &options, &cancel, true)
            .unwrap()
    );
    assert!(!work.accepted_watch_intent(&options));
    let cold_state = tempfile::tempdir().unwrap();
    let cold = Store::open_for_tests(cold_state.path(), root.path()).unwrap();
    let job = IndexJobCoordinator::prepare(&cold, None).unwrap();
    let _owner = job.session();
    let cold_pin = job.run(&options, &cancel, |_| {}).unwrap();
    compare(
        &store,
        state.path(),
        store.status().unwrap().revision,
        &cold,
        cold_state.path(),
        cold_pin,
    );
}
