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
    edit_with_optional_counts(world, "src/C.js", text, counts)
}

fn edit_with_optional_counts(
    world: &World,
    path: &str,
    text: &str,
    counts: &Path,
) -> Option<Value> {
    let _ = fs::remove_file(counts);
    write_files(&world.root, &[(path, text)]);
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

const RUST_MANIFEST: &str = "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n";
const RUST_MANIFEST_MOVED: &str =
    "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[lib]\npath = \"src/alt.rs\"\n";
const RUST_LIB: &str = "mod b;\nfn run() { b::target(); }\n";
const RUST_B: &str = "pub fn target() {}\n";
const RUST_TARGET: &str = "rust-analyzer cargo fixture 0.1.0 b/target().";

fn owners_at(world: &World, pin: IndexPin) -> trellis::scip_raw::CargoTargets {
    let (_, config, _) = world.store.retained_native_components_at(pin).unwrap();
    let (_, sources) = world.store.retained_native_snapshot_at(pin).unwrap();
    let mut builder = trellis::scip_raw::CargoTargetsBuilder::from_native_config(&config);
    assert!(!builder.is_empty(), "Cargo.toml must be captured");
    for (document, bytes) in sources {
        if document.language == "rust" {
            builder.add_document(&document.path, &bytes);
        }
    }
    builder.finish()
}

fn assert_rust_selection(world: &World, captured: IndexPin, descriptor: &(PathBuf, String)) {
    let head = world.store.index_baseline().unwrap();
    let selected = world.store.selected_raw_inputs_at(head).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].captured_pin, captured);
    let (native, _) = world.store.retained_native_snapshot_at(head).unwrap();
    assert!(
        native
            .declarations
            .iter()
            .any(|d| d.document.path == "src/b.rs" && d.name.as_deref() == Some("target"))
    );
    assert!(
        native
            .references
            .iter()
            .any(|r| r.document.path == "src/lib.rs" && r.spelling == "target")
    );
    assert!(native.calls.iter().any(|c| c.document.path == "src/lib.rs"
        && c.spelling.as_deref().is_some_and(|s| s.contains("target"))));
    let snapshot = world.store.semantic_at(head).unwrap();
    let scopes: Vec<_> = snapshot
        .symbols
        .iter()
        .filter(|s| s.key.symbol == RUST_TARGET)
        .map(|s| {
            (
                format!("{:?}", s.key.scope),
                s.key.document.as_ref().map(|d| d.path.clone()),
            )
        })
        .collect();
    assert_eq!(
        scopes,
        [("Target".to_string(), Some("src/lib.rs".to_string()))]
    );
    assert!(
        snapshot
            .coverage
            .iter()
            .any(|c| c.document_path == "src/lib.rs"
                && c.selected
                && c.state == CoverageState::Complete)
    );
    assert!(
        snapshot
            .coverage
            .iter()
            .any(|c| c.document_path == "src/b.rs"
                && c.selected
                && c.state == CoverageState::Complete)
    );
    assert!(snapshot.call_bindings.iter().any(|call| call.join.anchor.document.path == "src/lib.rs" && matches!(&call.declared_target, Some(Target::Internal { revision_id, document, .. }) if revision_id == &native.revision.id && document.path == "src/b.rs")));
    assert!(snapshot.references.iter().any(|r| r.document.path == "src/lib.rs" && r.spelling == "target" && r.resolution == Resolution::Resolved && matches!(&r.declared_target, Some(Target::Internal { document, .. }) if document.path == "src/b.rs")));
    assert!(!snapshot.provenance.is_empty());
    let expected = world.memory_publication(descriptor, DestinationMutations::default());
    assert_eq!(
        canonical(&snapshot.producers),
        canonical(&expected.producers)
    );
    assert_published_matches_staged(&snapshot, &expected);
}

#[test]
fn rust_cargo_owner_change_below_head_and_two_edits() {
    isolated(
        "semantic_batch_store::witness_catchup_574::child_rust_cargo_owner_change_below_head_and_two_edits",
        None,
    );
}

#[test]
#[ignore = "isolated child; invoked by rust_cargo_owner_change_below_head_and_two_edits"]
fn child_rust_cargo_owner_change_below_head_and_two_edits() {
    let counts = counts_file();
    let world = world_with(
        |root| {
            write_files(
                root,
                &[
                    ("Cargo.toml", RUST_MANIFEST),
                    ("src/lib.rs", RUST_LIB),
                    ("src/b.rs", RUST_B),
                    ("src/alt.rs", "mod b;\n"),
                    ("src/c.rs", "fn extra() {}\n"),
                ],
            )
        },
        true,
    );
    let captured = world.store.index_baseline().unwrap();
    let captured_owners = owners_at(&world, captured);
    for path in ["src/lib.rs", "src/b.rs"] {
        let owner = captured_owners.target_of(path).expect("captured lib owner");
        assert_eq!(
            (
                owner.kind.as_str(),
                owner.root.as_str(),
                owner.package.as_str(),
                owner.version.as_deref()
            ),
            ("lib", "src/lib.rs", "fixture", Some("0.1.0"))
        );
    }
    assert!(captured_owners.target_of("src/alt.rs").is_none());
    let run = "rust-analyzer cargo fixture 0.1.0 run().";
    let lib = occ_at(
        DocSpec::new("src/lib.rs"),
        RUST_LIB,
        ("run", 0),
        None,
        run,
        1,
    );
    let lib = occ_at(lib, RUST_LIB, ("target", 0), Some(8), RUST_TARGET, 0).symbol(run, 17, &[]);
    let b = occ_at(
        DocSpec::new("src/b.rs"),
        RUST_B,
        ("target", 0),
        None,
        RUST_TARGET,
        1,
    )
    .symbol(RUST_TARGET, 17, &[]);
    let mut fixture = world.fixture().reference("rust-analyzer");
    fixture.artifact(
        "rust",
        vec![DocSpec::new("src/alt.rs"), b, DocSpec::new("src/c.rs"), lib],
    );
    let descriptor = world.write(&fixture);
    let _ = edit_with_optional_counts(&world, "src/c.rs", "fn extra() { let _ = 0; }\n", &counts);
    let advanced = world.store.index_baseline().unwrap();
    assert_ne!(advanced, captured);
    let (imported, _, joined) = world.publish(&descriptor).unwrap();
    assert_eq!(joined.captured_pin(), captured);
    assert_eq!(joined.destination_pin(), advanced);
    assert_rust_selection(&world, captured, &descriptor);

    write_files(&world.root, &[("Cargo.toml", RUST_MANIFEST_MOVED)]);
    let phases = std::sync::Mutex::new(Vec::new());
    let (full, session) = trellis::index_coordinator::reconcile_workspace(
        &world.store,
        &IndexOptions::new(world.root.clone()),
        &world.cancel,
        |p| phases.lock().unwrap().push(p.phase),
    )
    .unwrap();
    drop(session);
    assert_ne!(full, imported);
    assert!(phases.lock().unwrap().iter().any(|p| p == "mode:full"));
    let destination_owners = owners_at(&world, full);
    assert!(destination_owners.target_of("src/lib.rs").is_none());
    for path in ["src/alt.rs", "src/b.rs"] {
        let owner = destination_owners
            .target_of(path)
            .expect("destination lib owner");
        assert_eq!(
            (
                owner.kind.as_str(),
                owner.root.as_str(),
                owner.package.as_str()
            ),
            ("lib", "src/alt.rs", "fixture")
        );
    }
    assert_rust_selection(&world, captured, &descriptor);
    for text in ["fn extra() { let _ = 1; }\n", "fn extra() { let _ = 2; }\n"] {
        let old = world.store.index_baseline().unwrap();
        let previous = world.store.semantic_at(old).unwrap();
        let work = edit_with_optional_counts(&world, "src/c.rs", text, &counts);
        if let Some(work) = work {
            eprintln!(
                "Rust below-head edit: fullFallbacks={}, witnessedRejoins={}, reasons={}, rawDecodes={}",
                work["fullFallbacks"],
                work["witnessedRejoins"],
                work["fallbackReasons"],
                work["originalRawDecodes"]
            );
        } else {
            eprintln!("Rust below-head edit: publication counters unavailable");
        }
        assert_ne!(world.store.index_baseline().unwrap(), old);
        assert_eq!(world.store.semantic_at(old).unwrap(), previous);
        assert_rust_selection(&world, captured, &descriptor);
    }
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
