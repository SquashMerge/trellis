//! Generated watcher cutoff histories through the real bounded event ingress.
use super::{compare, index_dir, sqlite_snapshot};
use notify::{
    Event, EventKind,
    event::{DataChange, ModifyKind},
};
use proptest::prelude::*;
use std::{
    cell::Cell,
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use trellis::{
    index_coordinator::{self, IndexJobCoordinator, WatcherWork},
    indexer::IndexOptions,
    model::IndexPin,
    store::Store,
    watch::WatchSignals,
};

#[derive(Clone, Copy, Debug)]
enum Edit {
    Advance,
    Rewrite,
    Same,
}

#[derive(Clone, Copy, Debug)]
enum Window {
    BeforeCutoff,
    AfterCutoff,
}

#[derive(Clone, Debug)]
struct Round {
    before: Vec<Edit>,
    window: Window,
    during: Vec<Edit>,
}

fn round() -> impl Strategy<Value = Round> {
    let edit =
        prop_oneof![3 => Just(Edit::Advance), 1 => Just(Edit::Rewrite), 1 => Just(Edit::Same)];
    (
        proptest::collection::vec(edit.clone(), 1..4),
        any::<bool>(),
        proptest::collection::vec(edit, 1..4),
    )
        .prop_map(|(before, post, during)| Round {
            before,
            window: if post {
                Window::AfterCutoff
            } else {
                Window::BeforeCutoff
            },
            during,
        })
}

fn text(version: u8) -> String {
    format!("export function value{version}() {{ return {version}; }}\n")
}

// This oracle describes ingress and the selected source independently of WatcherWork.
struct Model {
    version: u8,
    selected: u8,
    wrote_since_selection: bool,
    admitted: u64,
    accounted: u64,
    pin: IndexPin,
}

fn edit(path: &Path, model: &mut Model, op: Edit, submit: impl FnOnce(Event)) {
    if matches!(op, Edit::Advance) {
        model.version = (model.version + 1) % 4;
    }
    if !matches!(op, Edit::Same) {
        fs::write(path, text(model.version)).unwrap();
        model.wrote_since_selection = true;
    }
    submit(
        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
            .add_path(path.to_owned()),
    );
    model.admitted += 1;
}

fn selected(store: &Store, pin: IndexPin, version: u8, trace: &[Round]) {
    assert_eq!(
        store.source_at("a.js", Some(pin)).unwrap().unwrap().1.text,
        text(version),
        "selected source; trace={trace:?}"
    );
}

fn generations(work: &WatcherWork, model: &Model, options: &IndexOptions, trace: &[Round]) {
    assert_eq!(
        work.acknowledged_generation(),
        model.accounted,
        "acknowledged generation; trace={trace:?}"
    );
    assert_eq!(
        work.generation_handle().load(Ordering::SeqCst),
        model.admitted,
        "admitted generation; trace={trace:?}"
    );
    assert_eq!(
        work.accounted_watch_generation(),
        Some(model.accounted),
        "accounted generation; trace={trace:?}"
    );
    assert_eq!(
        work.accepted_watch_intent(options),
        model.admitted != model.accounted,
        "pending intent; trace={trace:?}"
    );
}

fn history(trace: &[Round]) {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let path = root.path().join("a.js");
    fs::write(&path, text(0)).unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let options = IndexOptions::new(root.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let owner =
        index_coordinator::establish_serving_session(&store, Some(&options), &cancel).unwrap();
    let mut work = WatcherWork::new(&store, &owner, &options).unwrap();
    work.disable_native_watcher_for_tests();
    assert!(
        work.reconcile_due(&store, &owner, &options, &cancel, true)
            .unwrap()
    );
    work.delay_periodic_inventory_for_tests();
    let old = store.status().unwrap().revision;
    let old_snapshot = store.retained_native_snapshot_at(old).unwrap();
    let old_sql = sqlite_snapshot(&index_dir(state.path()).join("index.db"), old, false);
    let initial_generation = work.generation_handle().load(Ordering::SeqCst);
    let mut model = Model {
        version: 0,
        selected: 0,
        wrote_since_selection: false,
        admitted: initial_generation,
        accounted: initial_generation,
        pin: old,
    };
    generations(&work, &model, &options, trace);

    for (step, round) in trace.iter().enumerate() {
        for &op in &round.before {
            edit(&path, &mut model, op, |event| {
                work.submit_watch_event_for_tests(Ok(event))
            });
            generations(&work, &model, &options, trace);
        }
        let capture_version = model.version;
        let capture_wrote = model.wrote_since_selection;
        let captured = Cell::new(false);
        let hook_ran = Cell::new(false);
        let model = std::cell::RefCell::new(&mut model);
        let perform = |watch: &WatchSignals| {
            hook_ran.set(true);
            for &op in &round.during {
                edit(&path, &mut model.borrow_mut(), op, |event| {
                    watch.submit_event(Ok(event))
                });
            }
        };
        let result = work
            .reconcile_due_with_cutoffs(
                &store,
                &owner,
                &options,
                &cancel,
                true,
                (
                    |capture, watch| {
                        assert_eq!(
                            capture
                                .files
                                .iter()
                                .find(|f| f.path == "a.js")
                                .unwrap()
                                .text,
                            text(capture_version),
                            "capture step={step} trace={trace:?}"
                        );
                        captured.set(true);
                        if matches!(round.window, Window::BeforeCutoff) {
                            perform(watch);
                        }
                    },
                    |watch| {
                        if matches!(round.window, Window::AfterCutoff) {
                            perform(watch);
                        }
                    },
                ),
            )
            .unwrap();
        let model = model.into_inner();
        assert!(
            captured.get() && hook_ran.get(),
            "hooks step={step} trace={trace:?}"
        );
        let actual_pin = store.status().unwrap().revision;
        match round.window {
            Window::BeforeCutoff => {
                // Check the observable stale publication before checking the return flag.
                selected(&store, actual_pin, model.selected, trace);
                assert_eq!(
                    actual_pin, model.pin,
                    "pre-cutoff publication step={step} trace={trace:?}"
                );
            }
            Window::AfterCutoff => {
                selected(&store, actual_pin, capture_version, trace);
                if !capture_wrote {
                    assert_eq!(actual_pin, model.pin, "unchanged capture trace={trace:?}");
                } else if capture_version != model.selected {
                    assert_ne!(actual_pin, model.pin, "changed capture trace={trace:?}");
                }
                model.selected = capture_version;
                model.pin = actual_pin;
                model.wrote_since_selection =
                    round.during.iter().any(|op| !matches!(op, Edit::Same));
            }
        }
        // Failed acknowledgement/rejected capture requests one extra full wake.
        model.admitted += 1;
        generations(&work, model, &options, trace);
        assert!(
            !result,
            "newer ingress cannot be accounted step={step} trace={trace:?}"
        );

        assert!(
            work.reconcile_due(&store, &owner, &options, &cancel, true)
                .unwrap(),
            "retry step={step} trace={trace:?}"
        );
        let final_pin = store.status().unwrap().revision;
        selected(&store, final_pin, model.version, trace);
        if !model.wrote_since_selection {
            assert_eq!(final_pin, model.pin, "unchanged retry trace={trace:?}");
        } else if model.version != model.selected {
            assert_ne!(final_pin, model.pin, "changed retry trace={trace:?}");
        }
        model.selected = model.version;
        model.pin = final_pin;
        model.wrote_since_selection = false;
        model.accounted = model.admitted;
        generations(&work, model, &options, trace);
        work.delay_periodic_inventory_for_tests();

        let cold_state = tempfile::tempdir().unwrap();
        let cold = Store::open_for_tests(cold_state.path(), root.path()).unwrap();
        let job = IndexJobCoordinator::prepare(&cold, None).unwrap();
        let _cold_owner = job.session();
        let cold_pin = job.run(&options, &cancel, |_| {}).unwrap();
        compare(
            &store,
            state.path(),
            final_pin,
            &cold,
            cold_state.path(),
            cold_pin,
        );
        assert_eq!(
            store.retained_native_snapshot_at(old).unwrap(),
            old_snapshot,
            "old pin trace={trace:?}"
        );
        assert_eq!(
            sqlite_snapshot(&index_dir(state.path()).join("index.db"), old, false),
            old_sql,
            "old SQL pin trace={trace:?}"
        );
        selected(&store, old, 0, trace);
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4),
        ..ProptestConfig::default()
    })]
    #[test]
    fn generated_cutoff_schedules(trace in proptest::collection::vec(round(), 1..4)) {
        history(&trace);
    }
}

#[test]
fn pre_cutoff_changed_witness() {
    history(&[Round {
        before: vec![Edit::Advance],
        window: Window::BeforeCutoff,
        during: vec![Edit::Advance],
    }]);
}

#[test]
fn post_cutoff_changed_witness() {
    history(&[Round {
        before: vec![Edit::Advance],
        window: Window::AfterCutoff,
        during: vec![Edit::Advance],
    }]);
}

#[test]
fn unchanged_capture_and_retry_witness() {
    history(&[Round {
        before: vec![Edit::Same],
        window: Window::AfterCutoff,
        during: vec![Edit::Same],
    }]);
}
