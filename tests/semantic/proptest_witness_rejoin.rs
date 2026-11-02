//! Discovery campaign: generate filesystem histories and require every LOCAL publication to
//! agree with the independent FULL rejoin oracle, step by step.
//!
//! This is investigative machinery, not a product test. It lives on `proptest/discovery`; the
//! per-bug regression tests are extracted separately from a minimal failing history.
#![cfg(feature = "test-support")]

use crate::semantic_batch_support::{BatchFixture, DocSpec};
use proptest::prelude::*;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    semantic_batch::{AdmittedBatch, BatchCounters, BatchLimits, testing},
    semantic_import::DestinationMutations,
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

// ---------------------------------------------------------------------------------------------
// In-process world (a reduced copy of the `semantic_batch_store` fixture).
// ---------------------------------------------------------------------------------------------

struct World {
    _state: tempfile::TempDir,
    _root: tempfile::TempDir,
    root: PathBuf,
    store: Store,
    index_db: PathBuf,
    cancel: Arc<AtomicBool>,
    batches: std::cell::Cell<usize>,
}

impl World {
    fn fixture(&self) -> BatchFixture {
        BatchFixture::from_published(&self.index_db, &self.root).synthetic()
    }

    fn batch_dir(&self) -> PathBuf {
        let number = self.batches.get();
        self.batches.set(number + 1);
        self.root.join(format!(".trellis-semantic-batch-{number}"))
    }

    fn write(&self, fixture: &BatchFixture) -> (PathBuf, String) {
        fixture.write(&self.batch_dir())
    }

    fn join_with(
        &self,
        descriptor: &(PathBuf, String),
        mutations: DestinationMutations,
    ) -> anyhow::Result<(
        AdmittedBatch,
        trellis::semantic_batch::JoinedBatch,
        BatchCounters,
    )> {
        testing::stage_and_join_admitted(
            &self.store,
            &self.root,
            &descriptor.0,
            &descriptor.1,
            &BatchLimits::PRODUCTION,
            mutations,
            &self.cancel,
        )
    }

    /// Joins `descriptor` against the head and publishes it as a semantic batch (a re-import).
    fn publish_batch(&self, descriptor: &(PathBuf, String)) -> anyhow::Result<()> {
        let (mut admitted, joined, _) =
            self.join_with(descriptor, DestinationMutations::default())?;
        let leader = self.store.publication_for_tests()?;
        self.store.publish_semantic_batch(
            &joined,
            admitted.stage_mut(),
            joined.destination_pin(),
            &leader,
            &self.cancel,
        )?;
        Ok(())
    }
}

fn world(populate: impl FnOnce(&Path)) -> World {
    let state = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    populate(&root);
    let store = Store::open_for_tests(state.path(), &root).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(leader);
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(state.path().join("cache"), state.path().join("data"));
    World {
        index_db: roots.index_db(&identity),
        _state: state,
        _root: temp,
        root,
        store,
        cancel,
        batches: std::cell::Cell::new(0),
    }
}

// ---------------------------------------------------------------------------------------------
// Source and artifact generation over a tiny, interacting domain.
// ---------------------------------------------------------------------------------------------

const PATHS: [&str; 4] = ["src/A.js", "src/B.js", "src/C.js", "src/D.js"];

/// Templates over the shared symbol pool `run`, `target`, `helper`. Every one is valid
/// JavaScript and every one has a definition occurrence; a template may also call one other
/// symbol, so an edit changes cross-document resolution.
const CONTENTS: [&str; 18] = [
    "function run(){ target(); }\n",
    "function run(){ return 1; }\n",
    "function run(){ target(); return 2; }\n",
    "function target(){ }\n",
    "function target(){ return 3; }\n",
    "function target(){ return 4; }\n",
    "function helper(){ run(); }\n",
    "function helper(){ }\n",
    "// note\nfunction target(){ }\n",
    "// note\nfunction run(){ target(); }\n",
    "function other(){ helper(); }\n",
    "function other(){ }\n",
    "function other(){ target(); return 5; }\n",
    "// note\nfunction other(){ helper(); }\n",
    "function run(){ run(); target(); }\n",
    "function target(){ helper(); }\n",
    "function helper(){ helper(); run(); }\n",
    "function other(){ other(); helper(); }\n",
];

/// Each path owns one declaration name; a rename is a move, so a name never exists in two
/// documents at once. An edit that would declare a name another document already declares is
/// skipped, because a producer cannot admit two declarations of one symbol key.
const A_TEMPLATES: [usize; 5] = [0, 1, 2, 9, 14];
const B_TEMPLATES: [usize; 5] = [3, 4, 5, 8, 15];
const C_TEMPLATES: [usize; 3] = [6, 7, 16];
const D_TEMPLATES: [usize; 5] = [10, 11, 12, 13, 17];

fn family(path: usize) -> &'static [usize] {
    match path {
        0 => &A_TEMPLATES,
        1 => &B_TEMPLATES,
        2 => &C_TEMPLATES,
        _ => &D_TEMPLATES,
    }
}

/// `(line, start column, end column)` of `len` characters at byte offset `start`.
fn char_span(content: &str, start: usize, len: usize) -> (i32, i32, i32) {
    let line = content[..start].matches('\n').count() as i32;
    let column_start = content[..start]
        .rfind('\n')
        .map_or(start, |newline| start - newline - 1) as i32;
    (line, column_start, column_start + len as i32)
}

/// The name and start offset of the first `function NAME` in `content`.
fn first_function(content: &str) -> (&str, usize) {
    let keyword = content
        .find("function ")
        .expect("every generated template defines a function");
    let start = keyword + "function ".len();
    let end = content[start..]
        .find('(')
        .map(|offset| start + offset)
        .expect("every generated template has a parameter list");
    (&content[start..end], start)
}

/// The byte offset of a call to `name`, never the definition occurrence.
fn call_site(content: &str, name: &str) -> Option<usize> {
    let needle = format!("{name}(");
    let mut from = 0;
    while let Some(relative) = content[from..].find(&needle) {
        let index = from + relative;
        let definition =
            index >= "function ".len() && &content[index - "function ".len()..index] == "function ";
        if !definition {
            return Some(index);
        }
        from = index + needle.len();
    }
    None
}

/// One raw claim. `four` writes the equivalent `[line, start, line, end]` shape instead of
/// `[line, start, end]`; `typed` moves it into the typed oneof.
fn claim(
    span: (i32, i32, i32),
    enclosing: Option<(i32, i32)>,
    symbol: &str,
    roles: i32,
    four: bool,
    typed: bool,
) -> crate::semantic_batch_support::OccSpec {
    let (line, start, end) = span;
    let range = if four {
        vec![line, start, line, end]
    } else {
        vec![line, start, end]
    };
    let enclosing = match enclosing {
        Some((start, end)) if four => vec![line, start, line, end],
        Some((start, end)) => vec![line, start, end],
        None => vec![],
    };
    crate::semantic_batch_support::OccSpec {
        range,
        enclosing,
        symbol: symbol.into(),
        roles,
        typed,
    }
}

/// An artifact document whose declarations and call claims lie exactly on the text it covers.
///
/// `flavor` varies range encodings and adds optional relationships or claims fused onto the
/// declaration or call span. Each artifact is checked against FULL with its own inputs;
/// adding claims or relationships is not an equivalence transformation.
fn doc_for(path: &str, content: &str, kind: i32, flavor: u8) -> DocSpec {
    let (name, start) = first_function(content);
    let symbol = format!("pkg {name}.");
    let definition = char_span(content, start, name.len());
    let four = flavor & 1 != 0;
    let typed = flavor & 2 != 0;
    let mut occurrences = vec![claim(definition, None, &symbol, 1, four, typed)];
    let related = if flavor & 4 != 0 {
        vec!["pkg target.".to_string()]
    } else {
        vec![]
    };
    let symbols = vec![(symbol.clone(), kind, related)];
    for callee in ["target", "run", "helper", "other"] {
        let Some(call) = call_site(content, callee) else {
            continue;
        };
        let span = char_span(content, call, callee.len());
        let enclosing = (flavor & 8 == 0).then_some((span.1, span.2 + 2));
        let callee_symbol = format!("pkg {callee}.");
        occurrences.push(claim(span, enclosing, &callee_symbol, 0, four, typed));
        if flavor & 16 != 0 {
            // A claim fused onto the declaration's span: the row alone cannot say which claim
            // owns it, so FULL must decide.
            occurrences.push(claim(definition, None, &callee_symbol, 0, four, typed));
        }
        if flavor & 32 != 0 {
            occurrences.push(claim(
                span,
                enclosing,
                &format!("pkg {callee}.other."),
                0,
                four,
                typed,
            ));
        }
    }
    DocSpec {
        path: path.into(),
        occurrences,
        symbols,
    }
}

fn read_text(world: &World, path: &str) -> Option<String> {
    fs::read_to_string(world.root.join(path)).ok()
}

fn existing_documents(world: &World) -> usize {
    PATHS
        .iter()
        .filter(|path| world.root.join(path).is_file())
        .count()
}

fn write_text(world: &World, path: &str, content: &str) {
    let file = world.root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, content).unwrap();
}

/// Imports a new batch over the current sources, covering the selected paths. Returns the
/// content each covered document had when it was captured, which is the reuse model.
/// The declaration names every other existing document currently defines.
fn names_elsewhere(world: &World, path_index: usize) -> std::collections::BTreeSet<String> {
    PATHS
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != path_index)
        .filter_map(|(_, path)| read_text(world, path))
        .map(|content| first_function(&content).0.to_owned())
        .collect()
}

/// Picks a template of `path_index`'s family whose declaration name is free, preferring the
/// generated `choice` and then scanning the rest of the family.
fn choose_edit(world: &World, path_index: usize, choice: usize) -> Option<&'static str> {
    let used = names_elsewhere(world, path_index);
    let templates = family(path_index);
    for offset in 0..templates.len() {
        let content = CONTENTS[templates[(choice + offset) % templates.len()]];
        if !used.contains(first_function(content).0) {
            return Some(content);
        }
    }
    None
}

fn import(
    world: &World,
    covered: &[bool],
    nonce: u8,
    flavor: u8,
) -> Result<BTreeMap<String, String>, String> {
    let mut fixture = world.fixture().reference("scip-typescript");
    let available: Vec<String> = fixture
        .paths("javascript")
        .into_iter()
        .map(|doc| doc.path)
        .collect();
    let mut captured = BTreeMap::new();
    let mut docs = Vec::new();
    for (index, path) in available.iter().enumerate() {
        if !covered.get(index).copied().unwrap_or(false) {
            continue;
        }
        let Some(content) = read_text(world, path) else {
            continue;
        };
        docs.push(doc_for(path, &content, 17 + i32::from(nonce % 30), flavor));
        captured.insert(path.clone(), content);
    }
    if docs.is_empty() {
        return Ok(captured);
    }
    fixture.artifact("javascript", docs);
    let descriptor = world.write(&fixture);
    world
        .publish_batch(&descriptor)
        .map_err(|error| format!("import failed: {error:#}"))?;
    Ok(captured)
}

// ---------------------------------------------------------------------------------------------
// Operations and their replay.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    /// Write one template into a path and publish locally.
    Edit {
        path: usize,
        content: usize,
    },
    /// Recreate a path that is currently absent.
    Add {
        path: usize,
    },
    /// Change several documents between one publication.
    MultiEdit {
        mask: u8,
        choices: [usize; 4],
    },
    /// Write back the bytes the last import captured for this path.
    Restore {
        path: usize,
    },
    /// Publish again without changing any source.
    Republish,
    Remove {
        path: usize,
    },
    /// Delete `from` and create `to` with the same bytes.
    Rename {
        from: usize,
        to: usize,
    },
    /// Rewrite the same bytes, advancing the capture stat only.
    Touch {
        path: usize,
    },
    /// Re-import a batch covering a subset of the current sources.
    Reimport {
        covered: [bool; 4],
        flavor: u8,
    },
    /// Add or remove the extra document `src/D.js`, which declares `other`.
    Extra {
        add: bool,
    },
    /// Advance the retention clock and run due maintenance.
    Maintain {
        advance: u64,
    },
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0..PATHS.len(), 0usize..4)
            .prop_map(|(path, content)| Op::Edit { path, content }),
        2 => (0..PATHS.len()).prop_map(|path| Op::Add { path }),
        3 => (1u8..16, [0usize..4, 0usize..4, 0usize..4, 0usize..4])
            .prop_map(|(mask, choices)| Op::MultiEdit { mask, choices }),
        1 => Just(Op::Republish),
        2 => (0..PATHS.len()).prop_map(|path| Op::Restore { path }),
        1 => (0..PATHS.len()).prop_map(|path| Op::Remove { path }),
        1 => (0..PATHS.len(), 0..PATHS.len()).prop_map(|(from, to)| Op::Rename { from, to }),
        1 => (0..PATHS.len()).prop_map(|path| Op::Touch { path }),
        2 => (any::<[bool; 4]>(), 0u8..64).prop_map(|(covered, flavor)| Op::Reimport { covered, flavor }),
        1 => any::<bool>().prop_map(|add| Op::Extra { add }),
        1 => (1u64..40).prop_map(|advance| Op::Maintain { advance }),
    ]
}

fn arb_history() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(arb_op(), 1..=16)
}

#[derive(Debug, Default)]
struct Stats {
    steps: usize,
    witnessed: usize,
    fallbacks: usize,
    missed_reuse: usize,
}

fn check_publication(
    work: &Value,
    label: &str,
    expected_reusable: usize,
    stats: &mut Stats,
) -> Result<(), String> {
    let witnessed = work["witnessedRejoins"].as_u64().unwrap_or(0);
    let fallbacks = work["fullFallbacks"].as_u64().unwrap_or(0);
    let reasons = work["fallbackReasons"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if witnessed > 0 {
        if fallbacks != 0 {
            return Err(format!(
                "{label}: witnessed rejoin and FULL fallback in one publication: {work}"
            ));
        }
        stats.witnessed += 1;
    }
    if !reasons.is_empty() {
        stats.fallbacks += 1;
    }
    if expected_reusable > 0 && witnessed == 0 && reasons.is_empty() {
        // Not a correctness failure by itself, but a coverage signal: the store declined the
        // witness path without naming a reason.
        stats.missed_reuse += 1;
    }
    Ok(())
}

fn publish_local(
    world: &World,
    counts: &Path,
    label: &str,
    expected_reusable: usize,
    stats: &mut Stats,
) -> Result<(), String> {
    let _ = fs::remove_file(counts);
    let options = IndexOptions::new(world.root.clone());
    let capture = trellis::capture::Capture::admit(&options, &world.cancel, &|_| {})
        .map_err(|error| format!("{label}: capture failed: {error:#}"))?;
    let leader = world
        .store
        .publication_for_tests()
        .map_err(|error| format!("{label}: writer unavailable: {error:#}"))?;
    let before = world.store.index_baseline().unwrap();
    let before_snapshot = world.store.semantic_at(before).ok();
    world
        .store
        .publish_local_for_tests(&options, &capture, &leader, &world.cancel)
        .map_err(|error| format!("{label}: LOCAL publication failed: {error:#}"))?;
    let bytes = fs::read(counts)
        .map_err(|error| format!("{label}: work counts missing after publish: {error}"))?;
    let work: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{label}: work counts unreadable: {error}"))?;
    stats.steps += 1;
    check_publication(&work, label, expected_reusable, stats)?;
    // Publication is atomic: a revision that already existed still reads the same rows.
    if let Some(snapshot) = before_snapshot {
        if let Ok(after) = world.store.semantic_at(before) {
            if after != snapshot {
                return Err(format!(
                    "{label}: an already published revision changed after a later publication"
                ));
            }
        }
    }
    Ok(())
}

/// Contents the current selection captured, per path. Only these can be reused.
#[derive(Default)]
struct ReuseModel {
    captured: BTreeMap<String, String>,
}

impl ReuseModel {
    fn expected(&self, world: &World) -> usize {
        self.captured
            .iter()
            .filter(|(path, content)| read_text(world, path).as_deref() == Some(content.as_str()))
            .count()
    }

    fn update(&mut self, captured: BTreeMap<String, String>) {
        if !captured.is_empty() {
            self.captured = captured;
        }
    }
}

fn populate(root: &Path) {
    for (path, content) in [
        (PATHS[0], CONTENTS[0]),
        (PATHS[1], CONTENTS[3]),
        (PATHS[2], CONTENTS[6]),
    ] {
        let file = root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, content).unwrap();
    }
}

fn setup(world: &World) -> Result<ReuseModel, String> {
    let mut model = ReuseModel::default();
    model.update(import(world, &[true, true, true, false], 0, 0)?);
    Ok(model)
}

fn run_history(world: &World, counts: &Path, history: &[Op]) -> Result<Stats, String> {
    let mut model = setup(world)?;
    let mut stats = Stats::default();
    // Harness calibration: one deterministic edit that leaves `src/B.js` untouched must reach
    // the witnessed path, or the rest of the history proves nothing about LOCAL vs FULL.
    write_text(world, PATHS[0], CONTENTS[1]);
    let expected = model.expected(world);
    publish_local(world, counts, "calibration edit", expected, &mut stats)?;
    if stats.witnessed == 0 {
        return Err(format!(
            "calibration edit never reached the witnessed rejoin (model expected {} reusable document(s))",
            expected
        ));
    }
    let mut clock = 1_000u64;
    let mut nonce = 0u8;
    for (index, op) in history.iter().enumerate() {
        let label = format!("op {index} {op:?}");
        match op {
            Op::Edit { path, content } => {
                if let Some(template) = choose_edit(world, *path, *content) {
                    write_text(world, PATHS[*path], template);
                    let expected = model.expected(world);
                    publish_local(world, counts, &label, expected, &mut stats)?;
                }
            }
            Op::Add { path } => {
                if read_text(world, PATHS[*path]).is_none() {
                    if let Some(template) = choose_edit(world, *path, 0) {
                        write_text(world, PATHS[*path], template);
                        let expected = model.expected(world);
                        publish_local(world, counts, &label, expected, &mut stats)?;
                    }
                }
            }
            Op::MultiEdit { mask, choices } => {
                let mut changed = false;
                for path in 0..PATHS.len() {
                    if mask & (1 << path) == 0 {
                        continue;
                    }
                    if let Some(template) = choose_edit(world, path, choices[path]) {
                        write_text(world, PATHS[path], template);
                        changed = true;
                    }
                }
                if changed {
                    let expected = model.expected(world);
                    publish_local(world, counts, &label, expected, &mut stats)?;
                }
            }
            Op::Restore { path } => {
                let captured = model.captured.get(PATHS[*path]).cloned();
                if let Some(content) = captured {
                    let name = first_function(&content).0;
                    let name_is_free = !names_elsewhere(world, *path).contains(name);
                    if name_is_free
                        && read_text(world, PATHS[*path]).as_deref() != Some(content.as_str())
                    {
                        write_text(world, PATHS[*path], &content);
                        let expected = model.expected(world);
                        publish_local(world, counts, &label, expected, &mut stats)?;
                    }
                }
            }
            Op::Republish => {
                let expected = model.expected(world);
                publish_local(world, counts, &label, expected, &mut stats)?;
            }
            Op::Remove { path } => {
                if read_text(world, PATHS[*path]).is_some() && existing_documents(world) > 1 {
                    fs::remove_file(world.root.join(PATHS[*path]))
                        .map_err(|error| format!("{label}: remove failed: {error}"))?;
                    let expected = model.expected(world);
                    publish_local(world, counts, &label, expected, &mut stats)?;
                }
            }
            Op::Rename { from, to } => {
                if from == to {
                    continue;
                }
                if read_text(world, PATHS[*from]).is_some() {
                    fs::rename(world.root.join(PATHS[*from]), world.root.join(PATHS[*to]))
                        .map_err(|error| format!("{label}: rename failed: {error}"))?;
                    let expected = model.expected(world);
                    publish_local(world, counts, &label, expected, &mut stats)?;
                }
            }
            Op::Touch { path } => {
                if read_text(world, PATHS[*path]).is_some() {
                    let file = fs::OpenOptions::new()
                        .write(true)
                        .open(world.root.join(PATHS[*path]))
                        .map_err(|error| format!("{label}: open failed: {error}"))?;
                    file.set_modified(
                        std::time::SystemTime::now() + std::time::Duration::from_secs(60),
                    )
                    .map_err(|error| format!("{label}: touch failed: {error}"))?;
                    let expected = model.expected(world);
                    publish_local(world, counts, &label, expected, &mut stats)?;
                }
            }
            Op::Reimport { covered, flavor } => {
                nonce += 1;
                let captured = import(world, covered, nonce, *flavor)?;
                if captured.is_empty() {
                    continue;
                }
                model.update(captured);
                stats.steps += 1;
            }
            Op::Extra { add } => {
                let file = world.root.join(PATHS[3]);
                if *add {
                    if !file.exists() {
                        if let Some(template) = choose_edit(world, 3, 0) {
                            write_text(world, PATHS[3], template);
                            let expected = model.expected(world);
                            publish_local(world, counts, &label, expected, &mut stats)?;
                        }
                    }
                } else if file.exists() && existing_documents(world) > 1 {
                    fs::remove_file(&file)
                        .map_err(|error| format!("{label}: remove failed: {error}"))?;
                    let expected = model.expected(world);
                    publish_local(world, counts, &label, expected, &mut stats)?;
                }
            }
            Op::Maintain { advance } => {
                clock += advance;
                // Stay inside the grace window: retention is a separate target.
                if clock > 1_800 {
                    clock = 1_800;
                }
                world
                    .store
                    .set_retention_clock_for_tests(clock as i64, clock - 1_000);
                let leader = world
                    .store
                    .publication_for_tests()
                    .map_err(|error| format!("{label}: writer unavailable: {error:#}"))?;
                world
                    .store
                    .maintain_revisions(&leader)
                    .map_err(|error| format!("{label}: maintenance failed: {error:#}"))?;
                stats.steps += 1;
            }
        }
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------------------------
// The property.
// ---------------------------------------------------------------------------------------------

fn proptest_cases() -> u32 {
    std::env::var("TRELLIS_PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(12)
}

#[test]
fn fused_artifact_reaches_witnessed_rejoin() {
    let counts_dir = tempfile::tempdir().unwrap();
    let counts = counts_dir.path().join("work-counts.json");
    // SAFETY: run this test in its own process, as nextest does.
    unsafe {
        std::env::set_var("TRELLIS_TEST_LOCAL_FULL_ORACLE", "1");
        std::env::set_var("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE", &counts);
        std::env::remove_var("TRELLIS_TEST_FORCE_FULL_REJOIN");
    }
    let world = world(populate);
    let stats = run_history(
        &world,
        &counts,
        &[
            Op::Reimport {
                covered: [true, true, true, false],
                flavor: 16 | 32,
            },
            Op::Edit {
                path: 1,
                content: 1,
            },
        ],
    )
    .unwrap();
    assert!(
        stats.witnessed >= 2,
        "expected a witnessed rejoin after importing fused claims: {stats:?}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: proptest_cases(),
        ..ProptestConfig::default()
    })]

    /// Every generated edit history keeps each LOCAL publication row-for-row equal to the
    /// independent FULL rejoin oracle, and always reaches the witnessed path after calibration.
    #[test]
    fn local_rejoin_matches_full_on_generated_histories(history in arb_history()) {
        let counts_dir = tempfile::tempdir().unwrap();
        let counts = counts_dir.path().join("work-counts.json");
        // SAFETY: nextest runs each test in its own process, and no other thread reads these.
        unsafe {
            std::env::set_var("TRELLIS_TEST_LOCAL_FULL_ORACLE", "1");
            std::env::set_var("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE", &counts);
            std::env::remove_var("TRELLIS_TEST_FORCE_FULL_REJOIN");
        }
        let world = world(populate);
        let result = run_history(&world, &counts, &history);
        match &result {
            Ok(stats) => {
                if std::env::var_os("TRELLIS_PROPTEST_TRACE").is_some() {
                    eprintln!("proptest case ok: {stats:?}");
                }
            }
            Err(error) => prop_assert!(false, "{error}"),
        }
    }
}
