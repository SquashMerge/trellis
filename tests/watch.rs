#![cfg(feature = "test-support")]
//! Leader-owned watcher and durable FIFO share one native publication stream.
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};
use trellis::{
    index_coordinator::{self, WatcherWork},
    indexer::IndexOptions,
    store::Store,
};

#[test]
fn leader_reconciles_edit_without_explicit_request() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = workspace.path().join("a.js");
    fs::write(&source, "function before() { return 1; }\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let session =
        index_coordinator::establish_serving_session(&store, Some(&options), &cancel).unwrap();
    let before = store.status().unwrap().revision;
    let mut work = WatcherWork::new(&store, &session, &options).unwrap();
    fs::write(&source, "function after() { return 2; }\n").unwrap();
    let until = Instant::now() + Duration::from_secs(8);
    while Instant::now() < until && store.status().unwrap().revision == before {
        work.reconcile_due(&store, &session, &options, &cancel, false)
            .unwrap();
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_ne!(
        store.status().unwrap().revision,
        before,
        "watch signal must publish a fresh selected revision"
    );
    let pinned = store.evidence_response().unwrap();
    let (_, historical) = pinned.source_at("a.js", Some(before)).unwrap().unwrap();
    assert_eq!(historical.text, "function before() { return 1; }\n");
    pinned.finish(()).unwrap();
}

#[test]
fn delayed_snapshot_does_not_publish_after_pre_cutoff_edit() {
    use notify::{Event, EventKind, event::ModifyKind};
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = workspace.path().join("a.js");
    fs::write(&source, "function original() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let owner =
        index_coordinator::establish_serving_session(&store, Some(&options), &cancel).unwrap();
    let mut work = WatcherWork::new(&store, &owner, &options).unwrap();
    // Drain the initial full wake and establish a known selected baseline.
    work.reconcile_due(&store, &owner, &options, &cancel, true)
        .unwrap();
    let prior = store.status().unwrap().revision;
    fs::write(&source, "function first() {}\n").unwrap();
    let admitted = std::cell::Cell::new(false);
    let published = work
        .reconcile_due_observed(&store, &owner, &options, &cancel, true, |capture, watch| {
            assert!(capture.files.iter().any(|file| file.path == "a.js"));
            admitted.set(true);
            fs::write(&source, "function second() {}\n").unwrap();
            watch.submit_event(Ok(Event::new(EventKind::Modify(ModifyKind::Data(
                notify::event::DataChange::Content,
            )))
            .add_path(source.clone())));
        })
        .unwrap();
    assert!(admitted.get());
    assert!(
        !published,
        "a pre-cutoff hint must reject the delayed snapshot"
    );
    assert_eq!(store.status().unwrap().revision, prior);
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.status().unwrap().revision == prior && Instant::now() < deadline {
        work.reconcile_due(&store, &owner, &options, &cancel, true)
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    let final_pin = store.status().unwrap().revision;
    assert!(final_pin.index_revision > prior.index_revision);
    let (_, final_source) = store.source_at("a.js", Some(final_pin)).unwrap().unwrap();
    assert_eq!(final_source.text, "function second() {}\n");
}

#[test]
fn post_cutoff_hint_stays_pending_after_first_publication() {
    use notify::{
        Event, EventKind,
        event::{DataChange, ModifyKind},
    };
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = workspace.path().join("a.js");
    fs::write(&source, "function first() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let owner =
        index_coordinator::establish_serving_session(&store, Some(&options), &cancel).unwrap();
    let mut work = WatcherWork::new(&store, &owner, &options).unwrap();
    // This fixture tests cutoff accounting, not OS notification latency.
    // Both hints enter the same bounded channel, without a late native event
    // invalidating the first capture before its intended post-cutoff hint.
    work.disable_native_watcher_for_tests();
    let before = store.status().unwrap().revision;
    fs::write(&source, "function middle() {}\n").unwrap();
    work.submit_watch_event_for_tests(Ok(Event::new(EventKind::Modify(ModifyKind::Data(
        DataChange::Content,
    )))
    .add_path(source.clone())));
    let accounted = work
        .reconcile_due_with_cutoffs(
            &store,
            &owner,
            &options,
            &cancel,
            true,
            (
                |_, _| {},
                |watch: &trellis::watch::WatchSignals| {
                    fs::write(&source, "function last() {}\n").unwrap();
                    watch.submit_event(Ok(Event::new(EventKind::Modify(ModifyKind::Data(
                        DataChange::Content,
                    )))
                    .add_path(source.clone())));
                },
            ),
        )
        .unwrap();
    assert!(
        !accounted,
        "post-cutoff hint cannot be acknowledged with first capture"
    );
    let middle = store.status().unwrap().revision;
    assert!(middle.index_revision > before.index_revision);
    assert_eq!(
        store
            .source_at("a.js", Some(middle))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function middle() {}\n"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.status().unwrap().revision == middle && Instant::now() < deadline {
        work.reconcile_due(&store, &owner, &options, &cancel, true)
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    let latest = store.status().unwrap().revision;
    assert!(latest.index_revision > middle.index_revision);
    assert_eq!(
        store
            .source_at("a.js", Some(latest))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function last() {}\n"
    );
}

#[test]
fn failed_capture_keeps_dirty_generation_for_successful_retry() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let source = workspace.path().join("a.js");
    fs::write(&source, "function before() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let owner =
        index_coordinator::establish_serving_session(&store, Some(&options), &cancel).unwrap();
    let prior = store.status().unwrap().revision;
    // Change before watcher registration so the initial full wake is the only
    // pending signal, with no dependency on notify delivery or event timing.
    fs::write(&source, "function after() {}\n").unwrap();
    let mut work = WatcherWork::new(&store, &owner, &options).unwrap();
    // A late OS notification for the write above would restart debounce and
    // postpone this deterministic retry; the initial full wake is internal.
    work.disable_native_watcher_for_tests();
    std::thread::sleep(Duration::from_millis(350));
    let failed =
        work.reconcile_due_observed(&store, &owner, &options, &cancel, false, |captured, _| {
            assert_eq!(captured.files[0].path, "a.js");
            cancel.store(true, std::sync::atomic::Ordering::Release);
        });
    assert!(failed.unwrap_err().to_string().contains("cancelled"));
    assert_eq!(
        store.status().unwrap().revision,
        prior,
        "failed capture must not select or acknowledge an unverified snapshot"
    );
    cancel.store(false, std::sync::atomic::Ordering::Release);
    // The scheduler's failure backoff is finite. No extra event or forced
    // inventory is submitted: only the retained dirty generation can retry.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        work.reconcile_due(&store, &owner, &options, &cancel, false)
            .unwrap(),
        "failed work must leave its dirty full wake pending"
    );
    let selected = store.status().unwrap().revision;
    assert!(selected.index_revision > prior.index_revision);
    assert_eq!(
        store
            .source_at("a.js", Some(selected))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function after() {}\n"
    );
}

#[test]
fn edits_creates_renames_atomic_saves_and_deletes_match_cold_full() {
    use notify::{
        Event, EventKind,
        event::{CreateKind, ModifyKind, RemoveKind, RenameMode},
    };
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let source = root.join("a.js");
    let second = root.join("b.js");
    let renamed = root.join("c.js");
    let temporary = root.join("a.js.tmp");
    fs::write(
        &source,
        "function start() { return helper(); }\nfunction helper() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("Types.java"),
        "class Stable { Other ref; }\nclass Other {}\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), root).unwrap();
    let options = IndexOptions::new(root.to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let owner =
        index_coordinator::establish_serving_session(&store, Some(&options), &cancel).unwrap();
    let mut work = WatcherWork::new(&store, &owner, &options).unwrap();
    // Each action happens while the prior immutable snapshot is delayed. The
    // injected notify event takes the same bounded ingress as the real watcher.
    for step in 0..5 {
        let prior = store.status().unwrap().revision;
        let admitted = work
            .reconcile_due_observed(&store, &owner, &options, &cancel, true, |_, watch| {
                let event = match step {
                    0 => {
                        fs::write(
                            &source,
                            "function edited() { return helper(); }\nfunction helper() {}\n",
                        )
                        .unwrap();
                        Event::new(EventKind::Modify(ModifyKind::Data(
                            notify::event::DataChange::Content,
                        )))
                        .add_path(source.clone())
                    }
                    1 => {
                        fs::write(&second, "function added() {}\n").unwrap();
                        Event::new(EventKind::Create(CreateKind::File)).add_path(second.clone())
                    }
                    2 => {
                        fs::rename(&second, &renamed).unwrap();
                        Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                            .add_path(second.clone())
                            .add_path(renamed.clone())
                    }
                    3 => {
                        fs::write(
                            &temporary,
                            "function atomic() { return helper(); }\nfunction helper() {}\n",
                        )
                        .unwrap();
                        fs::rename(&temporary, &source).unwrap();
                        Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                            .add_path(temporary.clone())
                            .add_path(source.clone())
                    }
                    _ => {
                        fs::remove_file(&renamed).unwrap();
                        Event::new(EventKind::Remove(RemoveKind::File)).add_path(renamed.clone())
                    }
                };
                watch.submit_event(Ok(event));
            })
            .unwrap();
        assert!(
            !admitted,
            "step {step}: delayed capture cannot account for a new hint"
        );
        assert_eq!(store.status().unwrap().revision, prior);
        let deadline = Instant::now() + Duration::from_secs(5);
        while store.status().unwrap().revision == prior && Instant::now() < deadline {
            work.reconcile_due(&store, &owner, &options, &cancel, true)
                .unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            store.status().unwrap().revision.index_revision > prior.index_revision,
            "step {step}: changed workspace must publish"
        );
        let cold_state = tempfile::tempdir().unwrap();
        let cold = Store::open_for_tests(cold_state.path(), root).unwrap();
        let cold_job = index_coordinator::IndexJobCoordinator::prepare(&cold, None).unwrap();
        let _cold_session = cold_job.session();
        cold_job.run(&options, &cancel, |_| {}).unwrap();
        assert_eq!(
            store.graph().unwrap(),
            cold.graph().unwrap(),
            "step {step}: selected graph differs from independent cold full"
        );
        let selected = store.evidence_response().unwrap();
        let cold_read = cold.evidence_response().unwrap();
        for path in ["a.js", "b.js", "c.js"] {
            let selected_source = selected
                .source_at(path, None)
                .unwrap()
                .map(|(_, source)| source.text);
            let cold_source = cold_read
                .source_at(path, None)
                .unwrap()
                .map(|(_, source)| source.text);
            assert_eq!(
                selected_source, cold_source,
                "step {step}: {path} source differs"
            );
        }
        // Compare selected typed native and class projections, not only the
        // graph and source text. IDs of a revision are store-specific; compare
        // semantic facts and pin each read to its own selected revision.
        let selected_pin = store.status().unwrap().revision;
        let cold_pin = cold.status().unwrap().revision;
        let mut cases = vec![
            ("Types.java", "java", "Stable"),
            (
                "a.js",
                "javascript",
                if step < 3 { "edited" } else { "atomic" },
            ),
        ];
        if (1..4).contains(&step) {
            cases.push((
                if step == 1 { "b.js" } else { "c.js" },
                "javascript",
                "added",
            ));
        }
        for (path, language, lookup) in cases {
            let selected_declarations = store
                .native_declarations_at(selected_pin, language, lookup)
                .unwrap();
            let cold_declarations = cold
                .native_declarations_at(cold_pin, language, lookup)
                .unwrap();
            assert!(
                !selected_declarations.is_empty(),
                "step {step}: {path} declaration missing"
            );
            let summarized =
                |store: &Store, pin, declarations: Vec<trellis::native_evidence::Declaration>| {
                    declarations
                        .into_iter()
                        .map(|d| {
                            assert_eq!(d.document.path, path);
                            let coverage =
                                store.native_coverage_at(pin, &d.document).unwrap().unwrap();
                            let calls = store.native_calls_at(pin, &d.syntax_id).unwrap();
                            serde_json::json!({
                                "kind": d.kind, "name": d.name, "lookup": d.lookup_key,
                                "range": d.range, "nameRange": d.name_range, "header": d.header,
                                "coverage": {
                                    "state": coverage.state, "requested": coverage.requested,
                                    "selected": coverage.selected,
                                    "supported": coverage.supported_roles,
                                    "observed": coverage.observed_roles,
                                    "diagnostic": coverage.diagnostic,
                                },
                                "calls": calls.into_iter().map(|call| serde_json::json!({
                                    "ordinal": call.ordinal, "range": call.range,
                                    "calleeRange": call.callee_range, "spelling": call.spelling,
                                })).collect::<Vec<_>>(),
                            })
                        })
                        .collect::<Vec<_>>()
                };
            let measured = summarized(&store, selected_pin, selected_declarations);
            let expected = summarized(&cold, cold_pin, cold_declarations);
            assert_eq!(
                measured, expected,
                "step {step}: {path} native projections differ"
            );
            if path == "a.js" {
                assert!(
                    measured.iter().any(|fact| fact["calls"]
                        .as_array()
                        .is_some_and(|calls| !calls.is_empty())),
                    "step {step}: native call comparison must exercise a real call"
                );
            }
        }
        if step == 4 {
            let selected_deleted = store
                .native_declarations_at(selected_pin, "javascript", "added")
                .unwrap();
            let cold_deleted = cold
                .native_declarations_at(cold_pin, "javascript", "added")
                .unwrap();
            assert_eq!(
                selected_deleted, cold_deleted,
                "deleted c.js native declaration must match independent cold full"
            );
            assert!(
                selected_deleted.is_empty(),
                "deleted c.js must not leave a stale pinned native declaration"
            );
        }
        let seed = store
            .graph()
            .unwrap()
            .nodes
            .into_iter()
            .find(|symbol| symbol.name == "Stable" && symbol.path == "Types.java")
            .unwrap()
            .id;
        let diagram = |store: &Store, pin| {
            let mut diagram = serde_json::to_value(
                store
                    .class_diagram_at(&trellis::class_diagram::ClassDiagramRequest {
                        seed: seed.clone(),
                        expected_revision: pin,
                        expanded: vec![],
                        include_unmatched: false,
                        include_hierarchy: true,
                    })
                    .unwrap(),
            )
            .unwrap();
            diagram.as_object_mut().unwrap().remove("revision");
            diagram
        };
        let selected_diagram = diagram(&store, selected_pin);
        assert!(!selected_diagram["nodes"].as_array().unwrap().is_empty());
        assert_eq!(
            selected_diagram,
            diagram(&cold, cold_pin),
            "step {step}: class projection differs from independent cold full"
        );
        selected.finish(()).unwrap();
        cold_read.finish(()).unwrap();
    }
}

fn cli(root: &std::path::Path, home: &std::path::Path, command: &str) -> std::process::Command {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_trellis"));
    child
        .env("HOME", home)
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME")
        .arg(command)
        .arg("--workspace")
        .arg(root);
    child
}

fn request_db_under(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == "requests.db") {
            return Some(path);
        }
        if path.is_dir()
            && let Some(found) = request_db_under(&path)
        {
            return Some(found);
        }
    }
    None
}

#[test]
fn daemon_watcher_keeps_cli_file_cap_and_selected_export_after_edit() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let home = temp.path().join("home");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&home).unwrap();
    let source = root.join("source.js");
    fs::write(&source, "function before() { return 1; }\n").unwrap();
    let indexed = cli(&root, &home, "index")
        .arg("--max-file-bytes")
        .arg("128")
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let first: serde_json::Value = serde_json::from_slice(&indexed.stdout).unwrap();
    assert_eq!(first["status"]["stats"]["files"], 1);
    let previous = first["publishedRevision"]["indexRevision"]
        .as_u64()
        .unwrap();
    fs::write(&source, "function after() { return 2; }\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let current = cli(&root, &home, "status").output().unwrap();
        assert!(
            current.status.success(),
            "{}",
            String::from_utf8_lossy(&current.stderr)
        );
        let current: serde_json::Value = serde_json::from_slice(&current.stdout).unwrap();
        if current["revision"]["indexRevision"]
            .as_u64()
            .is_some_and(|r| r > previous)
        {
            assert_eq!(current["stats"]["files"], 1);
            let exported = cli(&root, &home, "export").output().unwrap();
            assert!(
                exported.status.success(),
                "{}",
                String::from_utf8_lossy(&exported.stderr)
            );
            let graph: serde_json::Value = serde_json::from_slice(&exported.stdout).unwrap();
            assert!(graph["catchingUp"].is_boolean());
            assert_eq!(
                graph["data"]["files"][0]["text"],
                "function after() { return 2; }\n"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon watcher did not publish edited source"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn empty_checkout_explicit_request_publishes_fresh_manifest_and_terminal_pin() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let options = IndexOptions::new(workspace.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let (pin, writer) =
        index_coordinator::reconcile_workspace(&store, &options, &cancel, |_| {}).unwrap();
    writer.verify().unwrap();
    let accepted = store.enqueue_request(&options, None).unwrap();
    assert_eq!(
        index_coordinator::drain_requests(&store, &writer).unwrap(),
        1
    );
    let published = store.status().unwrap().revision;
    assert_eq!(published.index_generation, pin.index_generation);
    assert_eq!(
        published.index_revision,
        pin.index_revision + 1,
        "unchanged explicit FIFO request publishes a fresh manifest"
    );
    assert_eq!(store.status().unwrap().stats.files, 0);
    let row = store.request_by_id(&accepted.id).unwrap().unwrap();
    assert_eq!(row.state, "done");
    assert_eq!(row.revision, Some(published));
}

#[test]
fn periodic_inventory_recovers_unhinted_source_and_absent_ignore_input() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let source = root.join("a.js");
    fs::write(&source, "function before() { return 1; }\n").unwrap();
    fs::write(root.join("b.js"), "function hidden() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), root).unwrap();
    let options = IndexOptions::new(root.to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let owner =
        index_coordinator::establish_serving_session(&store, Some(&options), &cancel).unwrap();
    let mut work = WatcherWork::new(&store, &owner, &options).unwrap();
    assert!(
        work.reconcile_due(&store, &owner, &options, &cancel, true)
            .unwrap()
    );
    let before = store.status().unwrap().revision;
    work.suppress_watch_signals_for_tests(root.join("never-created-watch-root"));
    // Source bytes change without a usable watcher callback. A same-length
    // preserved mtime still has to be discovered by the complete inventory.
    #[cfg(unix)]
    let previous_mtime = {
        use std::os::unix::fs::MetadataExt;
        let stat = fs::metadata(&source).unwrap();
        (stat.mtime(), stat.mtime_nsec())
    };
    fs::write(&source, "function after_() { return 2; }\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
        let times = [
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
            libc::timespec {
                tv_sec: previous_mtime.0,
                tv_nsec: previous_mtime.1,
            },
        ];
        assert_eq!(
            unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) },
            0
        );
    }
    fs::write(root.join(".ignore"), "b.js\n").unwrap();
    assert!(
        !work
            .reconcile_due(&store, &owner, &options, &cancel, false)
            .unwrap(),
        "with lost signals an ordinary pre-periodic tick cannot see the edit"
    );
    // The full inventory, not a watcher hint, discovers both changes.
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.status().unwrap().revision == before && Instant::now() < deadline {
        work.force_periodic_inventory_for_tests();
        work.reconcile_due(&store, &owner, &options, &cancel, false)
            .unwrap();
    }
    let current = store.status().unwrap().revision;
    assert!(current.index_revision > before.index_revision);
    assert_eq!(
        store
            .source_at("a.js", Some(current))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function after_() { return 2; }\n"
    );
    assert!(store.source_at("b.js", Some(current)).unwrap().is_none());
    let cold_state = tempfile::tempdir().unwrap();
    let cold = Store::open_for_tests(cold_state.path(), root).unwrap();
    let cold_job = index_coordinator::IndexJobCoordinator::prepare(&cold, None).unwrap();
    let _cold_owner = cold_job.session();
    cold_job.run(&options, &cancel, |_| {}).unwrap();
    assert_eq!(store.graph().unwrap(), cold.graph().unwrap());
    let selected = store.status().unwrap().revision;
    assert_eq!(store.status().unwrap().revision, selected);
    let read = store.evidence_response().unwrap();
    read.source_at("a.js", Some(selected)).unwrap().unwrap();
    read.finish(()).unwrap();
    assert_eq!(
        store.status().unwrap().revision,
        selected,
        "status and pinned source reads are not indexing ingress"
    );
    // No change: the next 60-second maintenance inventory cannot add a revision.
    work.force_periodic_inventory_for_tests();
    assert!(
        work.reconcile_due(&store, &owner, &options, &cancel, false)
            .unwrap()
    );
    assert_eq!(store.status().unwrap().revision, selected);
}

#[test]
fn moved_root_stops_old_work_and_new_spelling_has_distinct_writer() {
    let state = tempfile::tempdir().unwrap();
    let roots = tempfile::tempdir().unwrap();
    let old_root = roots.path().join("old");
    let moved_root = roots.path().join("moved");
    fs::create_dir(&old_root).unwrap();
    fs::write(old_root.join("a.js"), "function original() {}\n").unwrap();
    let old = Store::open_for_tests(state.path(), &old_root).unwrap();
    let options = IndexOptions::new(old_root.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let owner =
        index_coordinator::establish_serving_session(&old, Some(&options), &cancel).unwrap();
    let mut work = WatcherWork::new(&old, &owner, &options).unwrap();
    let before = old.status().unwrap().revision;
    fs::rename(&old_root, &moved_root).unwrap();
    fs::create_dir(&old_root).unwrap();
    fs::write(old_root.join("a.js"), "function replacement() {}\n").unwrap();
    assert!(
        work.reconcile_due(&old, &owner, &options, &cancel, true)
            .is_err(),
        "old owner must refuse even a forced inventory at replaced pathname"
    );
    assert!(
        old.status().is_err(),
        "old root's selected read cannot serve after the move"
    );
    let relocated = Store::open_for_tests(state.path(), &moved_root).unwrap();
    let relocated_options = IndexOptions::new(moved_root.clone());
    let successor =
        index_coordinator::establish_serving_session(&relocated, Some(&relocated_options), &cancel)
            .unwrap();
    successor.verify().unwrap();
    assert_ne!(relocated.root_id(), old.root_id());
    let selected = relocated.status().unwrap().revision;
    assert_eq!(
        relocated
            .source_at("a.js", Some(selected))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function original() {}\n"
    );
    assert_ne!(selected, before);
    // Root keys are pathname-based. The replacement at the old spelling is
    // not a new owner of the old index; #101's wrong-root residue is separate.
    assert!(old.source_at("a.js", Some(before)).is_err());
    drop(work);
    drop(owner);
}

#[test]
fn daemon_root_loss_busy_transition_retains_resources_until_terminal_commit() {
    let state = tempfile::tempdir().unwrap();
    let roots = tempfile::tempdir().unwrap();
    let root = roots.path().join("original");
    let moved = roots.path().join("moved");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function before() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let owner =
        index_coordinator::establish_serving_session(&store, Some(&options), &cancel).unwrap();
    let daemon = trellis::http::new(
        store.clone(),
        options.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    daemon.retain_serving_session(owner.clone());
    daemon.force_retention_idle_tick_for_tests().unwrap();
    assert_eq!(daemon.root_loss_retirement_for_tests(), (true, true, false));
    let row = store.enqueue_request(&options, None).unwrap();
    let request_path = request_db_under(state.path()).unwrap();
    let blocker = rusqlite::Connection::open(&request_path).unwrap();
    blocker.busy_timeout(Duration::ZERO).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    fs::rename(&root, &moved).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function replacement() {}\n").unwrap();
    assert!(
        daemon.force_root_transition_tick_for_tests().is_err(),
        "held requests.db writer must block the root-loss terminal transition"
    );
    assert_eq!(
        daemon.root_loss_retirement_for_tests(),
        (true, true, false),
        "old resources remain until the accepted queue transition COMMIT is confirmed"
    );
    let read_error = store.status().unwrap_err();
    assert!(
        read_error.to_string().contains("root_changed"),
        "{read_error:#}"
    );
    let claim_error = store.claim_request(&owner).unwrap_err();
    assert!(
        claim_error.to_string().contains("root_changed"),
        "{claim_error:#}"
    );
    let waiting_error = store.request_by_id(&row.id).unwrap_err();
    assert!(
        waiting_error.to_string().contains("root_changed"),
        "{waiting_error:#}"
    );
    let before: String = blocker
        .query_row("SELECT state FROM requests WHERE id=?1", [&row.id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        before, "queued",
        "busy transition cannot partially settle the row"
    );
    blocker.execute_batch("ROLLBACK").unwrap();
    daemon.force_root_transition_tick_for_tests().unwrap();
    assert_eq!(
        daemon.root_loss_retirement_for_tests(),
        (false, false, false)
    );
    // The old Store intentionally rejects normal reads after root loss.
    // Inspect the durable queue row directly without claiming it or serving it.
    let (request_state, error, incarnation): (String, Option<String>, Option<String>) = blocker
        .query_row(
            "SELECT state,error_code,claim_incarnation FROM requests WHERE id=?1",
            [&row.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(request_state, "failed");
    assert_eq!(error.as_deref(), Some("root_changed"));
    let transition = incarnation.expect("terminal root-loss row needs a transition witness");
    assert!(uuid::Uuid::parse_str(&transition).is_ok());
    assert_ne!(
        transition,
        owner.incarnation().to_string(),
        "root-loss terminal settlement is not an ordinary writer claim"
    );
    let relocated = Store::open_for_tests(state.path(), &moved).unwrap();
    let next_options = IndexOptions::new(moved);
    let next =
        index_coordinator::establish_serving_session(&relocated, Some(&next_options), &cancel)
            .unwrap();
    next.verify().unwrap();
    assert_ne!(relocated.root_id(), store.root_id());
    let pin = relocated.status().unwrap().revision;
    assert_eq!(
        relocated
            .source_at("a.js", Some(pin))
            .unwrap()
            .unwrap()
            .1
            .text,
        "function before() {}\n"
    );
}

#[test]
fn ingress_filters_only_certain_excluded_noise_before_debounce() {
    use notify::{
        Event, EventKind,
        event::{DataChange, ModifyKind, RenameMode},
    };
    use trellis::watch::WatchSignals;
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    fs::create_dir(root.join("target")).unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("node_modules")).unwrap();
    let noise = [
        root.join("target/build.js"),
        root.join(".git/HEAD"),
        root.join("node_modules/module.js"),
    ];
    for path in &noise {
        fs::write(path, "noise").unwrap();
    }
    let mut watch = WatchSignals::new(root.to_owned(), None, None);
    // This fixture drives the bounded synthetic ingress only. Do not let a
    // live notify callback race the exact generation being acknowledged.
    watch.disable_native_watcher_for_tests();
    let initial = watch.drain();
    assert!(watch.accepted_unacked());
    assert!(watch.acknowledge(&initial));
    assert!(!watch.accepted_unacked());
    let event = |path: &std::path::Path| {
        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
            .add_path(path.to_owned())
    };
    for _ in 0..32 {
        for path in &noise {
            watch.submit_event(Ok(event(path)));
        }
    }
    assert!(
        !watch.accepted_unacked(),
        "ignored noise must never occupy the bounded ingress"
    );
    assert_eq!(watch.drain().generation, initial.generation);
    assert!(watch.next_deadline().is_none());
    let source = root.join("a.js");
    fs::write(
        &source,
        "function changed() {}
",
    )
    .unwrap();
    watch.submit_event(Ok(event(&source)));
    assert!(
        watch.accepted_unacked(),
        "source intent exists before debounce/drain"
    );
    assert!(!watch.batch_ready_at(Instant::now()));
    let received = watch.drain();
    assert!(received.generation > initial.generation);
    assert!(watch.acknowledge(&received));
    assert!(!watch.accepted_unacked());
    // One missing rename endpoint is uncertain, even when the other endpoint
    // sits under an excluded subtree.
    watch.submit_event(Ok(Event::new(EventKind::Modify(ModifyKind::Name(
        RenameMode::Both,
    )))
    .add_path(noise[0].clone())
    .add_path(root.join("missing.js"))));
    assert!(watch.accepted_unacked());
    assert!(watch.drain().full);
}

#[test]
fn selected_input_inside_excluded_subtree_is_accepted() {
    use notify::{
        Event, EventKind,
        event::{DataChange, ModifyKind},
    };
    use trellis::watch::WatchSignals;
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    fs::create_dir(root.join("target")).unwrap();
    let selected = root.join("target/index.scip");
    fs::write(&selected, "selected input").unwrap();
    let mut watch = WatchSignals::new(root.to_owned(), Some(selected.clone()), None);
    let initial = watch.drain();
    assert!(watch.acknowledge(&initial));
    watch.submit_event(Ok(Event::new(EventKind::Modify(ModifyKind::Data(
        DataChange::Content,
    )))
    .add_path(selected)));
    assert!(watch.accepted_unacked());
    assert!(watch.drain().full);
}

#[test]
fn accepted_watcher_intent_before_debounce_preempts_writer_unit() {
    use notify::{
        Event, EventKind,
        event::{DataChange, ModifyKind},
    };
    use std::sync::{Mutex, mpsc};
    use trellis::index_coordinator::{self, IndexJobCoordinator};
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let source = workspace.join("a.js");
    fs::write(&source, "function source() {}\n").unwrap();
    let store = Store::open_for_tests(&temp.path().join("state"), &workspace).unwrap();
    store.set_retention_clock_for_tests(1000, 0);
    let options = IndexOptions::new(workspace);
    let cancel = Arc::new(AtomicBool::new(false));
    let session =
        index_coordinator::establish_serving_session(&store, Some(&options), &cancel).unwrap();
    let old = store.status().unwrap().revision;
    IndexJobCoordinator::prepare_with_session(&store, None, session.clone())
        .unwrap()
        .run_serving(&options, &cancel, |_| {})
        .unwrap();
    let mut work = WatcherWork::new(&store, &session, &options).unwrap();
    work.disable_native_watcher_for_tests();
    work.reconcile_due(&store, &session, &options, &cancel, true)
        .unwrap();
    assert!(!work.accepted_watch_intent(&options));
    let work = Arc::new(Mutex::new(work));
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    store.set_maintenance_before_writer_hook_for_tests(move || {
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    store.set_retention_clock_for_tests(1900, 900);
    let worker_store = store.clone();
    let worker_session = session.clone();
    let worker_options = options.clone();
    let worker_watch = work.clone();
    let maintenance = std::thread::spawn(move || {
        index_coordinator::cooperative_maintenance_unit(&worker_store, &worker_session, || {
            !worker_watch
                .lock()
                .unwrap()
                .accepted_watch_intent(&worker_options)
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    fs::write(&source, "function changed() {}\n").unwrap();
    let event = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
        .add_path(source.clone());
    work.lock().unwrap().submit_watch_event_for_tests(Ok(event));
    assert!(
        work.lock().unwrap().accepted_watch_intent(&options),
        "accepted before debounce/drain"
    );
    release_tx.send(()).unwrap();
    assert_eq!(
        maintenance.join().unwrap().unwrap(),
        trellis::store::MaintenanceOutcome::Deferred
    );
    assert!(
        store.source_at("a.js", Some(old)).unwrap().is_some(),
        "no retention before watch ACK"
    );
    assert!(
        work.lock()
            .unwrap()
            .reconcile_due(&store, &session, &options, &cancel, true)
            .unwrap()
    );
    assert!(!work.lock().unwrap().accepted_watch_intent(&options));
    let selected = store.status().unwrap().revision;
    assert!(selected.index_revision > old.index_revision);
    let (_, captured) = store.source_at("a.js", Some(selected)).unwrap().unwrap();
    assert_eq!(captured.text, "function changed() {}\n");
    // After the verified fresh watcher publication and ACK, debt advances.
    assert_eq!(
        index_coordinator::cooperative_maintenance_unit(&store, &session, || !work
            .lock()
            .unwrap()
            .accepted_watch_intent(&options))
        .unwrap(),
        trellis::store::MaintenanceOutcome::Progress
    );
}
