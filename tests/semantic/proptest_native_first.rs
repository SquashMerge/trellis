//! Property tests of native-first publication (#620 step 3) against the real store,
//! publisher, bounded S owner units and public readers.
//!
//! Each generated history edits, restores, renames, removes, touches and re-imports a
//! tiny JavaScript workspace whose declarations call one another, so edits interact.
//! Every native publication N is checked against an independent reference model
//! before semantic catch-up S runs; S runs through the ordinary cooperative owner
//! units with the in-S LOCAL-vs-FULL oracle enabled. Properties (see the campaign log):
//!
//! 1. N is readable before S, and pinned reads never mix revisions.
//! 2. Changed documents and their actual dependents are `catchingUp`/`needsReimport`,
//!    never a negative semantic conclusion.
//! 3. Unchanged documents keep P evidence as `olderBasis` (never effective `fresh`),
//!    and no traversal follows an edge into a pending document as current.
//! 4. After S the published result equals FULL (the in-S oracle) and every retained
//!    pin is unchanged.
//! 5. A newer N supersedes obsolete staging: no stale completion, no leaked target or
//!    stage, no regression to an older result.
//! 6. A failed S is typed and leaves N and its target intact (no partial result).
//!
//! The model is deliberately independent of the projection SQL: the changed set comes
//! from the harness's own byte history, and dependents from P's decoded, published rows.
//!
//! Rerun on a given head (default 8 cases; deeper runs raise `TRELLIS_PROPTEST_CASES`):
//!
//! ```sh
//! /usr/bin/lockf -k /tmp/trellis-factory-verify.lock env -u TRELLIS_SEARCH_MODEL \
//!   -u TRELLIS_SEARCH_BACKEND -u TRELLIS_INDEX_DIAGNOSTICS CARGO_INCREMENTAL=0 \
//!   TRELLIS_PROPTEST_CASES=8 PROPTEST_RNG_SEED=62020261104 TRELLIS_PROPTEST_TRACE=1 \
//!   cargo test --locked --features test-support --test semantic -- proptest_native_first \
//!   --nocapture --test-threads=1
//! ```
#![cfg(feature = "test-support")]

use crate::semantic_batch_support::{BatchFixture, DocSpec, OccSpec};
use proptest::prelude::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    graph_query::{BoundaryReason, GraphQuery, Visit},
    indexer::{IndexOptions, index_workspace_bundle},
    model::{IndexPin, SemanticProjection, SemanticStatus},
    semantic_batch::{BatchLimits, testing},
    semantic_evidence::{Freshness, SemanticSnapshot, Target},
    semantic_import::DestinationMutations,
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity, WriterSession},
    },
};

const LANGUAGE: &str = "javascript";

// ---------------------------------------------------------------------------------------------
// In-process world.
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

impl World {
    fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.root.join(path)).ok()
    }

    fn write(&self, path: &str, content: &str) {
        let file = self.root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, content).unwrap();
    }

    fn files(&self) -> BTreeMap<String, String> {
        PATHS
            .iter()
            .filter_map(|path| self.read(path).map(|text| ((*path).to_owned(), text)))
            .collect()
    }

    /// Stage directories S may leave behind; none may survive a finished or failed S.
    fn stage_dirs(&self) -> Vec<String> {
        let Some(dir) = self.index_db.parent() else {
            return vec![];
        };
        fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.starts_with(trellis::semantic_batch::staging::STAGE_PREFIX))
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// Small interacting source domain (shared with the witness-rejoin harness's design).
// ---------------------------------------------------------------------------------------------

const PATHS: [&str; 4] = ["src/A.js", "src/B.js", "src/C.js", "src/D.js"];

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

const FAMILIES: [&[usize]; 4] = [
    &[0, 1, 2, 9, 14],
    &[3, 4, 5, 8, 15],
    &[6, 7, 16],
    &[10, 11, 12, 13, 17],
];

fn char_span(content: &str, start: usize, len: usize) -> (i32, i32, i32) {
    let line = content[..start].matches('\n').count() as i32;
    let column = content[..start]
        .rfind('\n')
        .map_or(start, |newline| start - newline - 1) as i32;
    (line, column, column + len as i32)
}

fn first_function(content: &str) -> (&str, usize) {
    let start = content.find("function ").unwrap() + "function ".len();
    let end = start + content[start..].find('(').unwrap();
    (&content[start..end], start)
}

fn call_site(content: &str, name: &str) -> Option<usize> {
    let needle = format!("{name}(");
    let mut from = 0;
    while let Some(relative) = content[from..].find(&needle) {
        let index = from + relative;
        let definition = index >= 9 && &content[index - 9..index] == "function ";
        if !definition {
            return Some(index);
        }
        from = index + needle.len();
    }
    None
}

fn occ(span: (i32, i32, i32), enclosing: Option<(i32, i32)>, symbol: &str, roles: i32) -> OccSpec {
    let (line, start, end) = span;
    OccSpec {
        range: vec![line, start, end],
        enclosing: enclosing.map_or(vec![], |(s, e)| vec![line, s, e]),
        symbol: symbol.into(),
        roles,
        typed: false,
    }
}

fn doc_for(path: &str, content: &str, kind: i32, flavor: u8) -> DocSpec {
    let (name, start) = first_function(content);
    let symbol = format!("pkg {name}.");
    let definition = char_span(content, start, name.len());
    let mut occurrences = vec![occ(definition, None, &symbol, 1)];
    let related = if flavor & 4 != 0 {
        vec!["pkg target.".to_string()]
    } else {
        vec![]
    };
    for callee in ["target", "run", "helper", "other"] {
        let Some(call) = call_site(content, callee) else {
            continue;
        };
        let span = char_span(content, call, callee.len());
        let enclosing = (flavor & 8 == 0).then_some((span.1, span.2 + 2));
        occurrences.push(occ(span, enclosing, &format!("pkg {callee}."), 0));
    }
    DocSpec {
        path: path.into(),
        occurrences,
        symbols: vec![(symbol, kind, related)],
    }
}

fn names_elsewhere(world: &World, path: usize) -> BTreeSet<String> {
    PATHS
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != path)
        .filter_map(|(_, p)| world.read(p))
        .map(|content| first_function(&content).0.to_owned())
        .collect()
}

/// A template of `path`'s family whose declaration name no other document declares.
fn choose(world: &World, path: usize, choice: usize) -> Option<&'static str> {
    let used = names_elsewhere(world, path);
    let family = FAMILIES[path];
    (0..family.len())
        .map(|offset| CONTENTS[family[(choice + offset) % family.len()]])
        .find(|content| !used.contains(first_function(content).0))
}

// ---------------------------------------------------------------------------------------------
// Reference model.
// ---------------------------------------------------------------------------------------------

/// What the harness itself knows, independent of projection SQL.
#[derive(Debug, Clone)]
struct Model {
    /// The validated semantic basis P every pending N is projected from.
    basis: IndexPin,
    /// Source bytes at P.
    basis_files: BTreeMap<String, String>,
    /// `owner path -> internal endpoint paths` of P's published rows.
    basis_endpoints: BTreeMap<String, BTreeSet<String>>,
    basis_snapshot: SemanticSnapshot,
    basis_native: String,
    producer: String,
    /// An N published since P whose S has not run (superseded or still outstanding).
    outstanding: Option<IndexPin>,
}

fn path_of(target: &Target) -> Option<&str> {
    match target {
        Target::Internal { document, .. } => Some(document.path.as_str()),
        Target::External { .. } => None,
    }
}

/// Independently derive per-document dependency endpoints from P's decoded publication.
fn endpoints(snapshot: &SemanticSnapshot) -> BTreeMap<String, BTreeSet<String>> {
    let owner: BTreeMap<&str, &str> = snapshot
        .provenance
        .iter()
        .map(|p| (p.id.as_str(), p.document.path.as_str()))
        .collect();
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut add = |proof: &str, targets: Vec<&str>| {
        if let Some(doc) = owner.get(proof) {
            let entry = out.entry((*doc).to_owned()).or_default();
            for target in targets {
                entry.insert(target.to_owned());
            }
        }
    };
    for s in &snapshot.symbols {
        add(
            &s.provenance_id,
            s.declarations.iter().filter_map(path_of).collect(),
        );
    }
    for b in &snapshot.declaration_bindings {
        add(&b.provenance_id, vec![b.join.anchor.document.path.as_str()]);
    }
    for r in &snapshot.references {
        let mut t: Vec<&str> = r.candidates.iter().filter_map(path_of).collect();
        t.extend(r.declared_target.as_ref().and_then(path_of));
        add(&r.provenance_id, t);
    }
    for c in &snapshot.call_bindings {
        let mut t: Vec<&str> = c.candidates.iter().filter_map(path_of).collect();
        t.extend(c.possible_dispatch.iter().filter_map(path_of));
        t.extend(c.declared_target.as_ref().and_then(path_of));
        t.push(c.join.anchor.document.path.as_str());
        add(&c.provenance_id, t);
    }
    for r in &snapshot.type_relationships {
        add(
            &r.provenance_id,
            [path_of(&r.source), path_of(&r.target)]
                .into_iter()
                .flatten()
                .collect(),
        );
    }
    out
}

impl Model {
    fn at(world: &World, basis: IndexPin) -> Result<Self, String> {
        let snapshot = world
            .store
            .semantic_at(basis)
            .map_err(|e| format!("basis semantic read failed: {e:#}"))?;
        let producer = snapshot
            .producers
            .first()
            .map(|p| p.id.clone())
            .ok_or("basis has no producer")?;
        Ok(Self {
            basis,
            basis_files: world.files(),
            basis_endpoints: endpoints(&snapshot),
            basis_native: native_id(world, basis)?,
            basis_snapshot: snapshot,
            producer,
            outstanding: None,
        })
    }

    fn changed(&self, files: &BTreeMap<String, String>) -> BTreeSet<String> {
        let mut changed = BTreeSet::new();
        for path in PATHS {
            if self.basis_files.get(path) != files.get(path) {
                changed.insert(path.to_owned());
            }
        }
        changed
    }

    /// Changed documents plus documents whose P evidence has an endpoint in one.
    fn pending(&self, files: &BTreeMap<String, String>) -> BTreeSet<String> {
        let changed = self.changed(files);
        files
            .keys()
            .filter(|path| {
                changed.contains(*path)
                    || self
                        .basis_endpoints
                        .get(*path)
                        .is_some_and(|ends| ends.iter().any(|end| changed.contains(end)))
            })
            .cloned()
            .collect()
    }

    fn rows_of(&self, path: &str) -> BTreeSet<String> {
        snapshot_ids(&self.basis_snapshot, Some(path))
    }
}

/// Canonical identity of a snapshot's provenance rows, optionally for one document.
fn snapshot_ids(snapshot: &SemanticSnapshot, path: Option<&str>) -> BTreeSet<String> {
    snapshot
        .provenance
        .iter()
        .filter(|p| path.is_none_or(|path| p.document.path == path))
        .map(|p| serde_json::to_string(p).unwrap())
        .collect()
}

/// A later pin of the same generation.
fn advances(next: IndexPin, prior: IndexPin) -> bool {
    next.index_generation == prior.index_generation && next.index_revision > prior.index_revision
}

fn native_id(world: &World, pin: IndexPin) -> Result<String, String> {
    world
        .store
        .with_native_reader(pin, |reader| Ok(reader.header()?.revision.id))
        .map_err(|e| format!("native identity at {pin:?} failed: {e:#}"))
}

/// Every declaration at `pin`: syntax id -> (path, name).
fn declarations(
    world: &World,
    pin: IndexPin,
) -> Result<BTreeMap<String, (String, String)>, String> {
    world
        .store
        .with_native_reader(pin, |reader| {
            let mut out = BTreeMap::new();
            let mut inventory = reader.inventory()?;
            while let Some(document) = inventory.next_document()? {
                let rows = reader.document(&document)?;
                for d in rows.declarations {
                    out.insert(
                        d.syntax_id.clone(),
                        (d.document.path.clone(), d.name.clone().unwrap_or_default()),
                    );
                }
            }
            Ok(out)
        })
        .map_err(|e| format!("native declarations at {pin:?} failed: {e:#}"))
}

type Projections = BTreeMap<String, Option<SemanticProjection>>;

fn projections(
    world: &World,
    pin: IndexPin,
    producer: &str,
    paths: &[String],
) -> Result<Projections, String> {
    paths
        .iter()
        .map(|path| {
            world
                .store
                .semantic_projection_at(pin, producer, LANGUAGE, path)
                .map(|p| (path.clone(), p))
                .map_err(|e| format!("projection {path} at {pin:?} failed: {e:#}"))
        })
        .collect()
}

/// Everything a pinned reader can observe at `pin`; it must never change once published.
/// A projected document read: its label and, when eligible, its row identities.
type ProjectedRows = (SemanticProjection, Option<BTreeSet<String>>);

#[derive(Debug, PartialEq)]
struct PinnedView {
    sources: BTreeMap<String, Option<String>>,
    projections: Projections,
    documents: BTreeMap<String, Option<ProjectedRows>>,
    native: String,
}

fn pinned_view(world: &World, pin: IndexPin, producer: &str) -> Result<PinnedView, String> {
    let paths: Vec<String> = PATHS.iter().map(|p| (*p).to_owned()).collect();
    let mut sources = BTreeMap::new();
    let mut documents = BTreeMap::new();
    for path in &paths {
        let source = world
            .store
            .source_at(path, Some(pin))
            .map_err(|e| format!("source {path} at {pin:?} failed: {e:#}"))?
            .map(|(read, file)| {
                assert_eq!(read, pin, "source read answered another pin");
                file.text
            });
        if source.is_some() {
            let key = trellis::native_evidence::DocumentKey {
                source_set_id: source_set(world, pin)?,
                language: LANGUAGE.into(),
                path: path.clone(),
            };
            let document = world
                .store
                .projected_semantic_document_at(pin, producer, &key)
                .map_err(|e| format!("projected document {path} at {pin:?} failed: {e:#}"))?
                .map(|(projection, rows)| (projection, rows.map(|r| snapshot_ids(&r, None))));
            documents.insert(path.clone(), document);
        }
        sources.insert(path.clone(), source);
    }
    let present: Vec<String> = sources
        .iter()
        .filter(|(_, s)| s.is_some())
        .map(|(p, _)| p.clone())
        .collect();
    Ok(PinnedView {
        projections: projections(world, pin, producer, &present)?,
        sources,
        documents,
        native: native_id(world, pin)?,
    })
}

fn source_set(world: &World, pin: IndexPin) -> Result<String, String> {
    world
        .store
        .with_native_reader(pin, |reader| {
            let mut inventory = reader.inventory()?;
            Ok(inventory
                .next_document()?
                .map(|d| d.key.source_set_id)
                .unwrap_or_default())
        })
        .map_err(|e| format!("source set at {pin:?} failed: {e:#}"))
}

// ---------------------------------------------------------------------------------------------
// Operations.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Edit {
        path: usize,
        choice: usize,
        defer: bool,
    },
    MultiEdit {
        mask: u8,
        choices: [usize; 4],
        defer: bool,
    },
    /// Write back P's bytes for this path.
    Restore {
        path: usize,
        defer: bool,
    },
    Remove {
        path: usize,
        defer: bool,
    },
    Add {
        path: usize,
        defer: bool,
    },
    Rename {
        from: usize,
        to: usize,
        defer: bool,
    },
    Touch {
        path: usize,
        defer: bool,
    },
    /// Explicit re-import over the current sources (may land on a pending N).
    Reimport {
        covered: [bool; 4],
        flavor: u8,
    },
    /// Run S for the outstanding target with a forced oracle mismatch first: typed failure.
    FailS,
    /// Drive S for whatever is outstanding now.
    CatchUp,
}

fn arb_op() -> impl Strategy<Value = Op> {
    let defer = prop::bool::weighted(0.35);
    prop_oneof![
        5 => (0..4usize, 0..5usize, defer).prop_map(|(path, choice, defer)| Op::Edit { path, choice, defer }),
        2 => (1u8..16, [0..5usize, 0..5usize, 0..5usize, 0..5usize], defer)
            .prop_map(|(mask, choices, defer)| Op::MultiEdit { mask, choices, defer }),
        3 => (0..4usize, defer).prop_map(|(path, defer)| Op::Restore { path, defer }),
        1 => (0..4usize, defer).prop_map(|(path, defer)| Op::Remove { path, defer }),
        2 => (0..4usize, defer).prop_map(|(path, defer)| Op::Add { path, defer }),
        1 => (0..4usize, 0..4usize, defer).prop_map(|(from, to, defer)| Op::Rename { from, to, defer }),
        1 => (0..4usize, defer).prop_map(|(path, defer)| Op::Touch { path, defer }),
        2 => (any::<[bool; 4]>(), 0u8..16).prop_map(|(covered, flavor)| Op::Reimport { covered, flavor }),
        2 => Just(Op::FailS),
        1 => Just(Op::CatchUp),
    ]
}

fn arb_history() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(arb_op(), 1..=12)
}

#[derive(Debug, Default)]
struct Stats {
    natives: usize,
    pending_docs: usize,
    dependent_only: usize,
    older_basis_docs: usize,
    traversal_boundaries: usize,
    older_basis_edges: usize,
    catch_ups: usize,
    supersessions: usize,
    stale_completions_refused: usize,
    failed_s: usize,
    reimports: usize,
    reimports_refused: usize,
    same_native: usize,
}

// ---------------------------------------------------------------------------------------------
// Replay.
// ---------------------------------------------------------------------------------------------

struct Harness<'w> {
    world: &'w World,
    session: Arc<WriterSession>,
    model: Model,
    /// Retained pins and what they showed when published; rechecked after every step.
    retained: Vec<(IndexPin, PinnedView)>,
    stats: Stats,
    nonce: u8,
}

fn err<E: std::fmt::Display>(label: &str) -> impl Fn(E) -> String + '_ {
    move |e| format!("{label}: {e}")
}

impl<'w> Harness<'w> {
    fn new(world: &'w World) -> Result<Self, String> {
        let session = Arc::new(WriterSession::writer(
            world.store.publication_for_tests().map_err(err("writer"))?,
        ));
        let mut harness = Self {
            world,
            session,
            model: Model {
                basis: world.store.index_baseline().unwrap(),
                basis_files: BTreeMap::new(),
                basis_endpoints: BTreeMap::new(),
                basis_snapshot: world
                    .store
                    .semantic_at(world.store.index_baseline().unwrap())
                    .map_err(err("initial"))?,
                basis_native: String::new(),
                producer: String::new(),
                outstanding: None,
            },
            retained: vec![],
            stats: Stats::default(),
            nonce: 0,
        };
        harness.reimport("setup", &[true, true, true, true], 0)?;
        Ok(harness)
    }

    fn reimport(&mut self, label: &str, covered: &[bool; 4], flavor: u8) -> Result<(), String> {
        self.nonce = self.nonce.wrapping_add(1);
        let world = self.world;
        let mut fixture = BatchFixture::from_published(&world.index_db, &world.root)
            .synthetic()
            .reference("scip-typescript");
        let available: Vec<String> = fixture
            .paths(LANGUAGE)
            .into_iter()
            .map(|d| d.path)
            .collect();
        let mut docs = vec![];
        for (index, path) in available.iter().enumerate() {
            if covered[index % 4]
                && let Some(content) = world.read(path)
            {
                docs.push(doc_for(
                    path,
                    &content,
                    17 + i32::from(self.nonce % 30),
                    flavor,
                ));
            }
        }
        if docs.is_empty() {
            return Ok(());
        }
        fixture.artifact(LANGUAGE, docs);
        let number = world.batches.get();
        world.batches.set(number + 1);
        let descriptor =
            fixture.write(&world.root.join(format!(".trellis-semantic-batch-{number}")));
        let joined = testing::stage_and_join_admitted(
            &world.store,
            &world.root,
            &descriptor.0,
            &descriptor.1,
            &BatchLimits::PRODUCTION,
            DestinationMutations::default(),
            &world.cancel,
        );
        let head = world.store.index_baseline().unwrap();
        let published = joined.and_then(|(mut admitted, joined, _)| {
            world.store.publish_semantic_batch(
                &joined,
                admitted.stage_mut(),
                joined.destination_pin(),
                self.session.writer_guard()?,
                &world.cancel,
            )
        });
        match published {
            Ok(pin) => {
                if !advances(pin, head) {
                    return Err(format!(
                        "{label}: import published a non-advancing pin {pin:?} <= {head:?}"
                    ));
                }
                self.stats.reimports += 1;
                // An explicit import is a validated publication: it is the new basis.
                let outstanding = self.model.outstanding.take();
                self.model = Model::at(world, pin)?;
                if outstanding.is_some()
                    && world
                        .store
                        .semantic_catch_up_pending_for_tests()
                        .map_err(err(label))?
                {
                    // A single-producer import fulfils the producer's slot; the target must go.
                    return Err(format!("{label}: import left a pending target behind"));
                }
                self.retain(pin)?;
                Ok(())
            }
            Err(error) => {
                // Refusals must be typed and leave the head alone.
                if world.store.index_baseline().unwrap() != head {
                    return Err(format!("{label}: refused import moved the head: {error:#}"));
                }
                if label == "setup" {
                    return Err(format!("setup import failed: {error:#}"));
                }
                self.stats.reimports_refused += 1;
                Ok(())
            }
        }
    }

    fn retain(&mut self, pin: IndexPin) -> Result<(), String> {
        let view = pinned_view(self.world, pin, &self.model.producer)?;
        self.retained.push((pin, view));
        Ok(())
    }

    fn check_retained(&self, label: &str) -> Result<(), String> {
        for (pin, view) in &self.retained {
            let now = pinned_view(self.world, *pin, &self.model.producer)?;
            if &now != view {
                return Err(format!(
                    "{label}: retained pin {pin:?} changed\nbefore: {view:#?}\nafter: {now:#?}"
                ));
            }
        }
        Ok(())
    }

    /// Publish native N for the current bytes and check properties 1-3 at N.
    fn publish_native(&mut self, label: &str, defer: bool) -> Result<(), String> {
        let world = self.world;
        let before = world.store.index_baseline().unwrap();
        let before_native = native_id(world, before)?;
        let before_projections = projections(
            world,
            before,
            &self.model.producer,
            &world.files().keys().cloned().collect::<Vec<_>>(),
        )?;
        let options = IndexOptions::new(world.root.clone());
        let capture = trellis::capture::Capture::admit(&options, &world.cancel, &|_| {})
            .map_err(|e| format!("{label}: capture failed: {e:#}"))?;
        let n = world
            .store
            .publish_local_for_tests(
                &options,
                &capture,
                self.session.writer_guard().map_err(err(label))?,
                &world.cancel,
            )
            .map_err(|e| format!("{label}: native publication failed: {e:#}"))?;
        if n == before {
            return Ok(());
        }
        if !advances(n, before) || world.store.index_baseline().unwrap() != n {
            return Err(format!(
                "{label}: N {n:?} is not the new head after {before:?}"
            ));
        }
        self.stats.natives += 1;
        let files = world.files();
        let paths: Vec<String> = files.keys().cloned().collect();
        // Property 1: N's committed bytes are readable now, before any S work.
        for path in PATHS {
            let read = world
                .store
                .source_at(path, Some(n))
                .map_err(|e| format!("{label}: N source {path} unreadable before S: {e:#}"))?
                .map(|(_, f)| f.text);
            if read.as_ref() != files.get(path) {
                return Err(format!(
                    "{label}: N source {path} = {read:?}, expected {:?}",
                    files.get(path)
                ));
            }
        }
        let n_native = native_id(world, n)?;
        let actual = projections(world, n, &self.model.producer, &paths)?;
        if n_native == before_native {
            // Same native identity: a semantic-only copy keeps the prior disposition.
            self.stats.same_native += 1;
            let carried: Projections = before_projections
                .into_iter()
                .map(|(path, p)| {
                    (
                        path,
                        p.map(|mut p| {
                            p.current_pin = n;
                            p
                        }),
                    )
                })
                .collect();
            if actual != carried {
                return Err(format!(
                    "{label}: same-native N changed projections\nexpected {carried:#?}\nactual {actual:#?}"
                ));
            }
            if carried.values().all(Option::is_none) && self.model.outstanding.is_none() {
                // A same-native copy of a validated pin is itself validated: the next N
                // projects from it. Its rows must equal the prior basis rows exactly.
                let copied = Model::at(world, n)?;
                if snapshot_ids(&copied.basis_snapshot, None)
                    != snapshot_ids(&self.model.basis_snapshot, None)
                {
                    return Err(format!("{label}: same-native copy changed semantic rows"));
                }
                self.model = copied;
            }
        } else {
            if self.model.outstanding.is_some() {
                self.stats.supersessions += 1;
            }
            self.check_projection(label, n, &files, &actual)?;
            self.check_traversal(label, n, &actual)?;
        }
        // Property 1: no unlabelled aggregate semantic read of a projected N.
        if actual.values().any(Option::is_some) && world.store.semantic_at(n).is_ok() {
            return Err(format!(
                "{label}: unlabelled aggregate semantic read served at projected N"
            ));
        }
        // Property 5: an older outstanding target can no longer complete.
        if let Some(old) = self.model.outstanding.replace(n) {
            let refused =
                world
                    .store
                    .complete_pending_semantic_for_tests(old, &self.session, &world.cancel);
            if refused.is_ok() {
                return Err(format!(
                    "{label}: superseded target {old:?} completed after {n:?}: {refused:?}"
                ));
            }
            if world.store.index_baseline().unwrap() != n {
                return Err(format!("{label}: refused stale completion moved the head"));
            }
            self.stats.stale_completions_refused += 1;
        }
        let target = world
            .store
            .semantic_catch_up_pending_for_tests()
            .map_err(err(label))?;
        if !target {
            self.model.outstanding = None;
            if actual.values().any(|p| {
                p.as_ref()
                    .is_some_and(|p| p.semantic_status == SemanticStatus::CatchingUp)
            }) {
                return Err(format!(
                    "{label}: catchingUp shown with no S target to finish it"
                ));
            }
        }
        self.retain(n)?;
        self.check_retained(label)?;
        if !defer {
            self.catch_up(label)?;
        }
        Ok(())
    }

    fn check_projection(
        &mut self,
        label: &str,
        n: IndexPin,
        files: &BTreeMap<String, String>,
        actual: &Projections,
    ) -> Result<(), String> {
        let pending = self.model.pending(files);
        let changed = self.model.changed(files);
        for (path, projection) in actual {
            let projection = projection
                .as_ref()
                .ok_or_else(|| format!("{label}: {path} has no projection at moved N {n:?}"))?;
            if projection.current_pin != n
                || projection.semantic_basis_pin != self.model.basis
                || projection.basis_native_revision_id != self.model.basis_native
            {
                return Err(format!(
                    "{label}: {path} projection basis {projection:?}, model basis {:?}/{}",
                    self.model.basis, self.model.basis_native
                ));
            }
            let expected_pending = pending.contains(path);
            // Property 2 (no retained-run absence is generated here: every batch is retained).
            let expected = if expected_pending {
                SemanticStatus::CatchingUp
            } else {
                SemanticStatus::OlderBasis
            };
            if projection.semantic_status != expected {
                return Err(format!(
                    "{label}: {path} status {:?}, model {expected:?} (changed {changed:?}, pending {pending:?}, P endpoints {:?})",
                    projection.semantic_status, self.model.basis_endpoints
                ));
            }
            let key = trellis::native_evidence::DocumentKey {
                source_set_id: source_set(self.world, n)?,
                language: LANGUAGE.into(),
                path: path.clone(),
            };
            let (read, rows) = self
                .world
                .store
                .projected_semantic_document_at(n, &self.model.producer, &key)
                .map_err(|e| format!("{label}: projected read {path}: {e:#}"))?
                .ok_or_else(|| format!("{label}: projected read {path} empty"))?;
            if &read != projection {
                return Err(format!(
                    "{label}: {path} document read label differs from status read"
                ));
            }
            if expected_pending {
                self.stats.pending_docs += 1;
                if !changed.contains(path) {
                    self.stats.dependent_only += 1;
                }
                if rows.is_some() {
                    return Err(format!("{label}: pending {path} served evidence rows"));
                }
            } else {
                self.stats.older_basis_docs += 1;
                // Property 3: the unchanged document keeps exactly P's evidence, never fresh.
                let rows =
                    rows.ok_or_else(|| format!("{label}: older-basis {path} hid its evidence"))?;
                let ids = snapshot_ids(&rows, Some(path));
                if ids != self.model.rows_of(path) {
                    return Err(format!("{label}: older-basis {path} rows differ from P"));
                }
                for proof in &rows.provenance {
                    if projection.effective_freshness(proof.freshness) == Freshness::Fresh {
                        return Err(format!(
                            "{label}: older-basis {path} effective freshness fresh"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Property 3: traversals never follow a semantic edge into or out of a pending
    /// document as current, and pending documents never yield a negative conclusion.
    fn check_traversal(
        &mut self,
        label: &str,
        n: IndexPin,
        actual: &Projections,
    ) -> Result<(), String> {
        let declarations = declarations(self.world, n)?;
        let status = |path: &str| {
            actual
                .get(path)
                .cloned()
                .flatten()
                .map(|p| p.semantic_status)
        };
        for (root, (root_path, _)) in &declarations {
            let query = GraphQuery {
                root_syntax_id: root.clone(),
                semantic_producer_id: Some(self.model.producer.clone()),
                depth: 3,
                max_nodes: 50,
                max_calls: 200,
                include_external: true,
                include_tests: true,
            };
            let response = self.world.store.evidence_response().map_err(err(label))?;
            let answer = response
                .graph_query(&query, Some(n), &mut || Ok(()))
                .map_err(|e| format!("{label}: graph from {root_path} failed: {e:#}"))?;
            response.finish(()).map_err(err(label))?;
            drop(response);
            let Ok(result) = answer.outcome else {
                continue;
            };
            let mut touched_pending = false;
            for edge in &result.edges {
                let from = declarations.get(&edge.from).map(|(p, _)| p.as_str());
                let to = edge
                    .to
                    .as_ref()
                    .and_then(|t| declarations.get(t))
                    .map(|(p, _)| p.as_str());
                let from_pending = from.and_then(status).is_some_and(|s| s.is_pending());
                let to_pending = to.and_then(status).is_some_and(|s| s.is_pending());
                let pending_reason = matches!(
                    edge.boundary_reason,
                    BoundaryReason::CatchingUp | BoundaryReason::NeedsReimport
                );
                touched_pending |= pending_reason;
                if from_pending {
                    if edge.binding.is_some() {
                        return Err(format!(
                            "{label}: edge from pending {from:?} used a semantic binding: {edge:?}"
                        ));
                    }
                    if !pending_reason {
                        return Err(format!(
                            "{label}: edge from pending {from:?} concluded {:?} instead of a pending boundary: {edge:?}",
                            edge.boundary_reason
                        ));
                    }
                }
                if let Some(binding) = &edge.binding {
                    let projection = edge.projection.as_ref().ok_or_else(|| {
                        format!("{label}: older-basis edge without a basis label: {edge:?}")
                    })?;
                    if projection.semantic_basis_pin != self.model.basis
                        || projection.semantic_status != SemanticStatus::OlderBasis
                    {
                        return Err(format!(
                            "{label}: edge label {projection:?} is not P/olderBasis"
                        ));
                    }
                    if edge.effective_freshness == Some(Freshness::Fresh)
                        || edge.effective_freshness.is_none()
                    {
                        return Err(format!(
                            "{label}: older-basis edge effective freshness {:?}",
                            edge.effective_freshness
                        ));
                    }
                    let targets: Vec<&str> = binding
                        .candidates
                        .iter()
                        .chain(binding.possible_dispatch.iter())
                        .chain(binding.declared_target.iter())
                        .filter_map(path_of)
                        .collect();
                    let into_pending = to_pending
                        || targets
                            .iter()
                            .any(|t| status(t).is_none_or(|s| s.is_pending()));
                    if into_pending && edge.visit == Visit::New {
                        return Err(format!(
                            "{label}: traversal followed {from:?} -> {to:?} into a pending document as current: {edge:?}"
                        ));
                    }
                    if into_pending && !pending_reason {
                        return Err(format!(
                            "{label}: edge into pending {targets:?} has boundary {:?}",
                            edge.boundary_reason
                        ));
                    }
                    self.stats.older_basis_edges += 1;
                }
                if pending_reason {
                    self.stats.traversal_boundaries += 1;
                }
            }
            if touched_pending && !result.partial {
                return Err(format!(
                    "{label}: result touching pending evidence is not partial"
                ));
            }
        }
        Ok(())
    }

    /// Property 4: drive S through the owner units; FULL oracle runs inside S.
    fn catch_up(&mut self, label: &str) -> Result<(), String> {
        let Some(n) = self.model.outstanding else {
            return Ok(());
        };
        let world = self.world;
        let completed = world
            .store
            .complete_pending_semantic_for_tests(n, &self.session, &world.cancel)
            .map_err(|e| format!("{label}: S for {n:?} failed: {e:#}"))?;
        let Some(s) = completed else {
            return Err(format!(
                "{label}: outstanding {n:?} had no target to complete"
            ));
        };
        self.stats.catch_ups += 1;
        if native_id(world, s)? != native_id(world, n)? {
            return Err(format!("{label}: S recaptured native identity"));
        }
        if world
            .store
            .semantic_catch_up_pending_for_tests()
            .map_err(err(label))?
        {
            return Err(format!("{label}: S left a pending target"));
        }
        let stages = world.stage_dirs();
        if !stages.is_empty() {
            return Err(format!("{label}: S leaked stages {stages:?}"));
        }
        let paths: Vec<String> = world.files().keys().cloned().collect();
        let after = projections(world, s, &self.model.producer, &paths)?;
        if after.values().any(Option::is_some) {
            return Err(format!(
                "{label}: S still projects an older basis: {after:#?}"
            ));
        }
        self.model = Model::at(world, s)?;
        self.retain(s)?;
        self.check_retained(label)
    }

    /// Property 6: a failed S is typed and leaves N and its target as they were.
    fn fail_s(&mut self, label: &str) -> Result<(), String> {
        let Some(n) = self.model.outstanding else {
            return Ok(());
        };
        let world = self.world;
        let before = pinned_view(world, n, &self.model.producer)?;
        let failed = world.store.with_local_oracle_mismatch_for_tests(|| {
            world
                .store
                .complete_pending_semantic_for_tests(n, &self.session, &world.cancel)
        });
        let Err(error) = failed else {
            return Err(format!("{label}: forced oracle mismatch did not fail S"));
        };
        if !format!("{error:#}").contains("oracle") {
            return Err(format!(
                "{label}: forced S failure was not the typed mismatch: {error:#}"
            ));
        }
        self.stats.failed_s += 1;
        if world.store.index_baseline().unwrap() != n
            || !world
                .store
                .semantic_catch_up_pending_for_tests()
                .map_err(err(label))?
            || pinned_view(world, n, &self.model.producer)? != before
        {
            return Err(format!("{label}: failed S changed the head, target or N"));
        }
        let stages = world.stage_dirs();
        if !stages.is_empty() {
            return Err(format!("{label}: failed S leaked stages {stages:?}"));
        }
        Ok(())
    }

    fn apply(&mut self, index: usize, op: &Op) -> Result<(), String> {
        let label = format!("op {index} {op:?}");
        let world = self.world;
        match op {
            Op::Edit {
                path,
                choice,
                defer,
            } => {
                if let Some(content) = choose(world, *path, *choice) {
                    world.write(PATHS[*path], content);
                    self.publish_native(&label, *defer)?;
                }
            }
            Op::MultiEdit {
                mask,
                choices,
                defer,
            } => {
                for path in 0..4 {
                    if mask & (1 << path) != 0
                        && let Some(content) = choose(world, path, choices[path])
                    {
                        world.write(PATHS[path], content);
                    }
                }
                self.publish_native(&label, *defer)?;
            }
            Op::Restore { path, defer } => {
                if let Some(content) = self.model.basis_files.get(PATHS[*path]).cloned()
                    && !names_elsewhere(world, *path).contains(first_function(&content).0)
                {
                    world.write(PATHS[*path], &content);
                    self.publish_native(&label, *defer)?;
                }
            }
            Op::Remove { path, defer } => {
                if world.read(PATHS[*path]).is_some() && world.files().len() > 1 {
                    fs::remove_file(world.root.join(PATHS[*path])).map_err(err(&label))?;
                    self.publish_native(&label, *defer)?;
                }
            }
            Op::Add { path, defer } => {
                if world.read(PATHS[*path]).is_none()
                    && let Some(content) = choose(world, *path, 0)
                {
                    world.write(PATHS[*path], content);
                    self.publish_native(&label, *defer)?;
                }
            }
            Op::Rename { from, to, defer } => {
                if from != to && world.read(PATHS[*from]).is_some() {
                    fs::rename(world.root.join(PATHS[*from]), world.root.join(PATHS[*to]))
                        .map_err(err(&label))?;
                    self.publish_native(&label, *defer)?;
                }
            }
            Op::Touch { path, defer } => {
                if world.read(PATHS[*path]).is_some() {
                    let file = fs::OpenOptions::new()
                        .write(true)
                        .open(world.root.join(PATHS[*path]))
                        .map_err(err(&label))?;
                    file.set_modified(
                        std::time::SystemTime::now() + std::time::Duration::from_secs(60),
                    )
                    .map_err(err(&label))?;
                    self.publish_native(&label, *defer)?;
                }
            }
            Op::Reimport { covered, flavor } => self.reimport(&label, covered, *flavor)?,
            Op::FailS => self.fail_s(&label)?,
            Op::CatchUp => self.catch_up(&label)?,
        }
        Ok(())
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

fn run_history(history: &[Op]) -> Result<Stats, String> {
    let world = world(populate);
    let mut harness = Harness::new(&world)?;
    for (index, op) in history.iter().enumerate() {
        harness.apply(index, op)?;
    }
    // Property 4/5: every history ends caught up, whatever was superseded on the way.
    harness.catch_up("final catch-up")?;
    harness.check_retained("final")?;
    Ok(harness.stats)
}

// ---------------------------------------------------------------------------------------------
// Properties.
// ---------------------------------------------------------------------------------------------

fn cases() -> u32 {
    std::env::var("TRELLIS_PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8)
}

/// The FULL oracle is process-wide: run in a child with it enabled.
fn isolated(test: &str) -> bool {
    const CHILD: &str = "TRELLIS_PROPTEST_NATIVE_FIRST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let dir = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("TRELLIS_TEST_LOCAL_FULL_ORACLE", "1")
        .env(
            "TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE",
            dir.path().join("counts.json"),
        )
        .env_remove("TRELLIS_TEST_FORCE_FULL_REJOIN")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    if std::env::var_os("TRELLIS_PROPTEST_TRACE").is_some() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed;"),
        "isolated {test} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

/// Deterministic positive control: a dependent-only document, an older-basis
/// document, a supersession and a failed S all occur in this one history.
#[test]
fn native_first_control_history() {
    if !isolated("proptest_native_first::native_first_control_history") {
        return;
    }
    let stats = run_history(&[
        // B (target) changes: A (run calls target) is a dependent; C (helper calls run) is not.
        Op::Edit {
            path: 1,
            choice: 1,
            defer: true,
        },
        Op::FailS,
        Op::Edit {
            path: 2,
            choice: 1,
            defer: false,
        },
        Op::Restore {
            path: 1,
            defer: false,
        },
    ])
    .unwrap();
    eprintln!("control stats: {stats:?}");
    assert!(
        stats.dependent_only > 0,
        "no dependent-only pending document: {stats:?}"
    );
    assert!(
        stats.older_basis_docs > 0,
        "no older-basis document: {stats:?}"
    );
    assert!(
        stats.supersessions > 0 && stats.stale_completions_refused > 0,
        "{stats:?}"
    );
    assert!(stats.failed_s > 0 && stats.catch_ups > 0, "{stats:?}");
}

#[test]
fn native_first_generated_histories() {
    let name = "proptest_native_first::native_first_generated_histories";
    if !isolated(name) {
        return;
    }
    let config = ProptestConfig {
        cases: cases(),
        source_file: Some(file!()),
        test_name: Some(name),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::WithSource("proptest-regressions"),
        )),
        ..ProptestConfig::default()
    };
    let totals = std::cell::RefCell::new(Stats::default());
    proptest::test_runner::TestRunner::new(config)
        .run(&arb_history(), |history| {
            match run_history(&history) {
                Ok(stats) => {
                    let mut t = totals.borrow_mut();
                    t.natives += stats.natives;
                    t.pending_docs += stats.pending_docs;
                    t.dependent_only += stats.dependent_only;
                    t.older_basis_docs += stats.older_basis_docs;
                    t.traversal_boundaries += stats.traversal_boundaries;
                    t.older_basis_edges += stats.older_basis_edges;
                    t.catch_ups += stats.catch_ups;
                    t.supersessions += stats.supersessions;
                    t.stale_completions_refused += stats.stale_completions_refused;
                    t.failed_s += stats.failed_s;
                    t.reimports += stats.reimports;
                    t.reimports_refused += stats.reimports_refused;
                    t.same_native += stats.same_native;
                }
                Err(error) => prop_assert!(false, "{error}"),
            }
            Ok(())
        })
        .unwrap();
    eprintln!(
        "native-first coverage over {} cases: {:?}",
        cases(),
        totals.borrow()
    );
}
