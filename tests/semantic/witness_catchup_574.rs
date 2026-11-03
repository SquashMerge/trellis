//! Small, process-isolated #574 catch-up cases. No daemon or producer is started.
use super::*;
use trellis::semantic_evidence::{CoverageState, Target};

fn cancel_after_semantic_write(world: &World) -> Arc<AtomicBool> {
    let observed = Arc::new(AtomicBool::new(false));
    let signal = observed.clone();
    let cancel = world.cancel.clone();
    world
        .store
        .set_semantic_publication_observer_for_tests(move |stage, _| {
            if stage == trellis::store::SemanticPublicationStage::AfterDocument {
                signal.store(true, std::sync::atomic::Ordering::SeqCst);
                cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });
    observed
}

fn assert_cancelled_without_publication(
    world: &World,
    old: IndexPin,
    before: &trellis::semantic_evidence::SemanticSnapshot,
    observed: &AtomicBool,
) {
    assert!(
        observed.load(std::sync::atomic::Ordering::SeqCst),
        "semantic write hook did not fire"
    );
    world
        .cancel
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(world.store.index_baseline().unwrap(), old);
    assert_eq!(&world.store.semantic_at(old).unwrap(), before);
}

// Parents and children have distinct test entrypoints: no inherited variable can
// turn a parent into a child or make a child recursively start another process.
fn isolated(name: &str, cases: Option<usize>) {
    let temp = tempfile::tempdir().unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TRELLIS_") {
            command.env_remove(key);
        }
    }
    command
        .args(["--exact", name, "--ignored", "--nocapture"])
        .env("TRELLIS_TEST_LOCAL_FULL_ORACLE", "1")
        .env(
            "TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE",
            temp.path().join("counts.json"),
        );
    if let Some(cases) = cases {
        command.env("TRELLIS_574_CASES", cases.to_string());
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed;"));
    print!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn counts_file() -> PathBuf {
    PathBuf::from(
        std::env::var_os("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE")
            .expect("run this child through its parent test"),
    )
}

fn cases() -> usize {
    match std::env::var("TRELLIS_574_CASES") {
        Err(std::env::VarError::NotPresent) => 4,
        value => {
            let value = value.expect("TRELLIS_574_CASES must be valid Unicode");
            let count = value
                .parse::<usize>()
                .expect("TRELLIS_574_CASES must be a positive integer");
            assert!(count > 0, "TRELLIS_574_CASES must be a positive integer");
            count
        }
    }
}

fn descriptor(world: &World) -> (PathBuf, String) {
    let (a, b) = definitions_docs();
    let mut fixture = world.fixture().reference("scip-typescript");
    fixture.artifact("javascript", vec![a, b]);
    world.write(&fixture)
}

fn assert_selected(world: &World, captured: IndexPin, descriptor: &(PathBuf, String)) {
    let head = world.store.index_baseline().unwrap();
    let selected = world.store.selected_raw_inputs_at(head).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].captured_pin, captured);
    assert_eq!(selected[0].captures.len(), 1);
    let snapshot = world.store.semantic_at(head).unwrap();
    let paths: Vec<_> = snapshot
        .coverage
        .iter()
        .filter(|c| c.selected && c.state == CoverageState::Complete)
        .map(|c| c.document_path.as_str())
        .collect();
    assert_eq!(paths, vec!["src/A.js", "src/B.js"]);
    assert_eq!(snapshot.declaration_bindings.len(), 2);
    assert_eq!(snapshot.call_bindings.len(), 1);
    let (native, _) = world.store.retained_native_snapshot_at(head).unwrap();
    assert!(snapshot.call_bindings.iter().any(|call| call.join.anchor.document.path == "src/A.js" && matches!(&call.declared_target, Some(Target::Internal { revision_id, document, .. }) if revision_id == &native.revision.id && document.path == "src/B.js")));
    assert!(!snapshot.provenance.is_empty());
    let expected = world.memory_publication(descriptor, DestinationMutations::default());
    assert_eq!(
        canonical(&snapshot.producers),
        canonical(&expected.producers)
    );
    assert_published_matches_staged(&snapshot, &expected);
}

// Work counters are diagnostics for this scenario, not a promise about which
// backend a later implementation uses or whether it exposes this test hook.
fn below_head_edit(world: &World, text: &str, counts: &Path) -> Option<Value> {
    let _ = fs::remove_file(counts);
    write_files(&world.root, &[("src/C.js", text)]);
    let options = IndexOptions::new(world.root.clone());
    let capture = trellis::capture::Capture::admit(&options, &world.cancel, &|_| {}).unwrap();
    let leader = world.store.publication_for_tests().unwrap();
    world
        .store
        .publish_local_for_tests(&options, &capture, &leader, &world.cancel)
        .unwrap();
    match fs::read(counts) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).expect("invalid publication counters")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("publication counters: {error}"),
    }
}

#[test]
fn cargo_toml_after_retained_import() {
    isolated(
        "semantic_batch_store::witness_catchup_574::child_cargo_toml_after_retained_import",
        None,
    );
}

#[test]
#[ignore = "isolated child; invoked by cargo_toml_after_retained_import"]
fn child_cargo_toml_after_retained_import() {
    let _counts = counts_file();
    let world = world_with(
        |root| {
            write_files(
                root,
                &[
                    (
                        "Cargo.toml",
                        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
                    ),
                    ("src/A.js", DEFINITIONS_A),
                    ("src/B.js", DEFINITIONS_B),
                ],
            )
        },
        true,
    );
    let captured = world.store.index_baseline().unwrap();
    let desc = descriptor(&world);
    let (imported, _, joined) = world.publish(&desc).unwrap();
    assert_eq!(joined.captured_pin(), captured);
    assert_selected(&world, captured, &desc);
    let before = world.store.semantic_at(imported).unwrap();
    write_files(
        &world.root,
        &[(
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.1\"\n",
        )],
    );
    let observed = cancel_after_semantic_write(&world);
    assert!(
        trellis::index_coordinator::reconcile_workspace(
            &world.store,
            &IndexOptions::new(world.root.clone()),
            &world.cancel,
            |_| {}
        )
        .is_err()
    );
    assert!(observed.load(std::sync::atomic::Ordering::SeqCst));
    world
        .cancel
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let observer = Store::open_for_tests(world._state.path(), &world.root).unwrap();
    assert_eq!(observer.index_baseline().unwrap(), imported);
    assert_eq!(observer.semantic_at(imported).unwrap(), before);
    let selected = observer.selected_raw_inputs_at(imported).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].captured_pin, captured);
    drop(observer);
    let phases = std::sync::Mutex::new(Vec::new());
    let (pin, session) = trellis::index_coordinator::reconcile_workspace(
        &world.store,
        &IndexOptions::new(world.root.clone()),
        &world.cancel,
        |p| phases.lock().unwrap().push(p.phase),
    )
    .unwrap();
    drop(session);
    assert_eq!(pin, world.store.index_baseline().unwrap());
    assert_eq!(pin.index_revision, imported.index_revision + 1);
    assert!(phases.lock().unwrap().iter().any(|p| p == "mode:full"));
    assert_ne!(world.store.index_baseline().unwrap(), imported);
    assert_eq!(world.store.semantic_at(imported).unwrap(), before);
    assert_selected(&world, captured, &desc);
}

#[test]
fn captured_below_head_then_two_edits() {
    isolated(
        "semantic_batch_store::witness_catchup_574::child_captured_below_head_then_two_edits",
        None,
    );
}

#[test]
#[ignore = "isolated child; invoked by captured_below_head_then_two_edits"]
fn child_captured_below_head_then_two_edits() {
    let counts = counts_file();
    let world = definitions_world();
    let captured = world.store.index_baseline().unwrap();
    let desc = descriptor(&world);
    write_files(
        &world.root,
        &[("src/C.js", "function extra(){ return 0; }\n")],
    );
    let options = IndexOptions::new(world.root.clone());
    let capture = trellis::capture::Capture::admit(&options, &world.cancel, &|_| {}).unwrap();
    let leader = world.store.publication_for_tests().unwrap();
    world
        .store
        .publish_local_for_tests(&options, &capture, &leader, &world.cancel)
        .unwrap();
    drop(leader);
    let advanced = world.store.index_baseline().unwrap();
    assert_ne!(captured, advanced);
    let (imported, _, joined) = world.publish(&desc).unwrap();
    assert_eq!(joined.captured_pin(), captured);
    assert_eq!(joined.destination_pin(), advanced);
    assert_selected(&world, captured, &desc);
    let before = world.store.semantic_at(imported).unwrap();
    write_files(
        &world.root,
        &[("src/C.js", "function extra(){ return 1; }\n")],
    );
    let options = IndexOptions::new(world.root.clone());
    let capture = trellis::capture::Capture::admit(&options, &world.cancel, &|_| {}).unwrap();
    let observed = cancel_after_semantic_write(&world);
    let leader = world.store.publication_for_tests().unwrap();
    assert!(
        world
            .store
            .publish_local_for_tests(&options, &capture, &leader, &world.cancel)
            .is_err()
    );
    drop(leader);
    assert_cancelled_without_publication(&world, imported, &before, &observed);
    assert_selected(&world, captured, &desc);
    for text in [
        "function extra(){ return 1; }\n",
        "function extra(){ return 2; }\n",
    ] {
        let before_pin = world.store.index_baseline().unwrap();
        let before = world.store.semantic_at(before_pin).unwrap();
        let work = below_head_edit(&world, text, &counts);
        let head = world.store.index_baseline().unwrap();
        assert_ne!(head, before_pin);
        assert_eq!(world.store.semantic_at(before_pin).unwrap(), before);
        assert_selected(&world, captured, &desc);
        if let Some(work) = work {
            eprintln!(
                "below-head catch-up: fullFallbacks={}, witnessedRejoins={}, reasons={}, rawDecodes={}",
                work["fullFallbacks"],
                work["witnessedRejoins"],
                work["fallbackReasons"],
                work["originalRawDecodes"]
            );
        } else {
            eprintln!("below-head catch-up: publication counters unavailable");
        }
    }
    assert_ne!(world.store.index_baseline().unwrap(), imported);
}

#[test]
fn generated_interacting_edit_restore_reimport_histories() {
    isolated(
        "semantic_batch_store::witness_catchup_574::child_generated_interacting_edit_restore_reimport_histories",
        Some(cases()),
    );
}

#[test]
#[ignore = "isolated child; invoked by generated_interacting_edit_restore_reimport_histories"]
fn child_generated_interacting_edit_restore_reimport_histories() {
    let counts = counts_file();
    let cases = cases();
    let mut seed = 20261103u64;
    let mut witnessed = 0;
    for case in 0..cases {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let world = definitions_world();
        let captured = world.store.index_baseline().unwrap();
        let mut desc = descriptor(&world);
        world.publish(&desc).unwrap();
        assert_selected(&world, captured, &desc);
        let changed_b = if seed & 1 == 0 {
            "function target(){ return 1; }\n"
        } else {
            "function target(){ return 2; }\n"
        };
        let mut edits = [
            ("src/C.js", "function extra(){ return 0; }\n"),
            ("src/B.js", changed_b),
            ("src/B.js", DEFINITIONS_B),
        ];
        if seed & 2 != 0 {
            edits.swap(0, 1);
        }
        for (path, text) in edits {
            let old = world.store.index_baseline().unwrap();
            let before = world.store.semantic_at(old).unwrap();
            let work = local_edit(&world, path, text, &counts);
            witnessed += work["witnessedRejoins"].as_u64().unwrap_or(0);
            assert_eq!(world.store.semantic_at(old).unwrap(), before, "case {case}");
            let head = world.store.index_baseline().unwrap();
            let actual = world.store.semantic_at(head).unwrap();
            let expected = world.memory_publication(&desc, DestinationMutations::default());
            assert_published_matches_staged(&actual, &expected);
            assert!(!actual.declaration_bindings.is_empty(), "case {case}");
            assert_eq!(
                world.store.selected_raw_inputs_at(head).unwrap()[0].captured_pin,
                captured
            );
        }
        let recaptured = world.store.index_baseline().unwrap();
        desc = descriptor(&world);
        let (_, _, joined) = world.publish(&desc).unwrap();
        assert_eq!(joined.captured_pin(), recaptured);
        assert_selected(&world, recaptured, &desc);
        let old = world.store.index_baseline().unwrap();
        let before = world.store.semantic_at(old).unwrap();
        let work = local_edit(
            &world,
            "src/C.js",
            "function extra(){ return 3; }\n",
            &counts,
        );
        witnessed += work["witnessedRejoins"].as_u64().unwrap_or(0);
        assert_eq!(world.store.semantic_at(old).unwrap(), before);
        assert_selected(&world, recaptured, &desc);
    }
    assert!(
        witnessed >= cases as u64,
        "no witnessed LOCAL publications: {witnessed}"
    );
    eprintln!("seed 20261103: {cases} interacting histories, {witnessed} witnessed publications");
}
