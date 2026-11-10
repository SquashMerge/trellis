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
//!    never a negative semantic conclusion. A dependent is actual only when a changed
//!    dependency's term differs (Decision 0021 D4): the dependency is gone, or a syntax ID
//!    the dependent targets is no longer declared in it. A comment or body edit that leaves
//!    those declarations alone keeps the dependent older-basis.
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(state.path(), &root));
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
    /// `owner path -> (dependency path, targeted syntax ID)` of P's published rows.
    basis_targets: BTreeMap<String, BTreeSet<(String, String)>>,
    basis_snapshot: SemanticSnapshot,
    basis_native: String,
    producer: String,
    /// Documents with a P tuple, and those whose P tuple omitted an association (#669 H1
    /// excludes the latter from copying). Read as stored facts of the validated basis.
    basis_tuples: BTreeSet<String>,
    basis_omitted: BTreeSet<String>,
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

/// Independently derive, per document, the `(dependency path, syntax ID)` pairs P's rows
/// target in other documents: the declarations a dependency term reads.
fn targeted_ids(snapshot: &SemanticSnapshot) -> BTreeMap<String, BTreeSet<(String, String)>> {
    let owner: BTreeMap<&str, &str> = snapshot
        .provenance
        .iter()
        .map(|p| (p.id.as_str(), p.document.path.as_str()))
        .collect();
    let mut out: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
    let mut add = |proof: &str, targets: Vec<&Target>| {
        let Some(doc) = owner.get(proof) else {
            return;
        };
        for target in targets {
            if let Target::Internal {
                document,
                syntax_id,
                ..
            } = target
            {
                out.entry((*doc).to_owned())
                    .or_default()
                    .insert((document.path.clone(), syntax_id.clone()));
            }
        }
    };
    for s in &snapshot.symbols {
        add(&s.provenance_id, s.declarations.iter().collect());
    }
    for r in &snapshot.references {
        let mut t: Vec<&Target> = r.candidates.iter().collect();
        t.extend(r.declared_target.as_ref());
        add(&r.provenance_id, t);
    }
    for c in &snapshot.call_bindings {
        let mut t: Vec<&Target> = c.candidates.iter().collect();
        t.extend(c.possible_dispatch.iter());
        t.extend(c.declared_target.as_ref());
        add(&c.provenance_id, t);
    }
    for r in &snapshot.type_relationships {
        add(&r.provenance_id, vec![&r.source, &r.target]);
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
        let key = pin_key(basis);
        let (basis_tuples, basis_omitted) = world
            .store
            .with_index_read_for_tests(|db| {
                let mut statement = db.prepare(
                    "SELECT c.path, COALESCE(rp.omitted,rt.omitted,0) FROM semantic_coverage c \
                     LEFT JOIN semantic_coverage_reasons rp ON rp.payload_id=c.payload_id \
                     LEFT JOIN semantic_coverage_reasons rt ON rt.tuple_id=c.tuple_id \
                     WHERE c.revision_id=?1 AND c.producer_id=?2",
                )?;
                let rows = statement
                    .query_map(rusqlite::params![key, producer], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok((
                    rows.iter().map(|(p, _)| p.clone()).collect(),
                    rows.iter()
                        .filter(|(_, o)| *o)
                        .map(|(p, _)| p.clone())
                        .collect(),
                ))
            })
            .map_err(|e| format!("basis coverage facts failed: {e:#}"))?;
        Ok(Self {
            basis,
            basis_files: world.files(),
            basis_endpoints: endpoints(&snapshot),
            basis_targets: targeted_ids(&snapshot),
            basis_native: native_id(world, basis)?,
            basis_snapshot: snapshot,
            producer,
            basis_tuples,
            basis_omitted,
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

    /// Changed documents plus documents with a changed dependency whose term differs at
    /// `n`: the dependency is gone, or a syntax ID the document's P rows target in it is not
    /// declared in it at `n`. (The fixtures declare plain named functions, so a declaration's
    /// read status is its presence; a dependency's comment or body edit keeps its IDs.)
    fn pending(
        &self,
        world: &World,
        n: IndexPin,
        files: &BTreeMap<String, String>,
    ) -> Result<BTreeSet<String>, String> {
        let changed = self.changed(files);
        let declared = declarations(world, n)?;
        let term_differs = |owner: &str, dependency: &str| {
            !files.contains_key(dependency)
                || self.basis_targets.get(owner).is_some_and(|targets| {
                    targets
                        .iter()
                        .filter(|(path, _)| path == dependency)
                        .any(|(_, sid)| {
                            declared.get(sid).is_none_or(|(path, _)| path != dependency)
                        })
                })
        };
        Ok(files
            .keys()
            .filter(|path| {
                changed.contains(*path)
                    || self.basis_endpoints.get(*path).is_some_and(|ends| {
                        ends.iter().any(|end| {
                            end != *path && changed.contains(end) && term_differs(path, end)
                        })
                    })
            })
            .cloned()
            .collect())
    }

    /// P's rows for `path` as a per-document answer projection serves them.
    fn rows_of(&self, path: &str) -> Vec<String> {
        snapshot_rows(&document_answer(&self.basis_snapshot, path))
    }
}

/// Canonical form of every semantic row in `snapshot`, all eight kinds: a sorted multiset
/// of kind-tagged canonical JSON (the store's `native_ids::canonical` encoding). A missing,
/// extra, duplicated or changed row of any kind changes it. IDs are kept as they are:
/// every comparison made with it is between reads of the same semantic basis rows, whose
/// IDs the contract keeps stable (re-keyed copies are checked by `check_rekeyed`).
fn snapshot_rows(snapshot: &SemanticSnapshot) -> Vec<String> {
    fn tagged<T: serde::Serialize>(kind: &str, rows: &[T], out: &mut Vec<String>) {
        for row in rows {
            let value = serde_json::to_value(row).unwrap();
            let bytes = trellis::native_ids::canonical(&value);
            out.push(format!("{kind}:{}", String::from_utf8(bytes).unwrap()));
        }
    }
    let SemanticSnapshot {
        producers,
        coverage,
        provenance,
        symbols,
        declaration_bindings,
        references,
        call_bindings,
        type_relationships,
    } = snapshot;
    let mut out = vec![];
    tagged("producer", producers, &mut out);
    tagged("coverage", coverage, &mut out);
    tagged("provenance", provenance, &mut out);
    tagged("symbol", symbols, &mut out);
    tagged("declaration", declaration_bindings, &mut out);
    tagged("reference", references, &mut out);
    tagged("call", call_bindings, &mut out);
    tagged("type", type_relationships, &mut out);
    out.sort();
    out
}

/// The per-document answer projection of one document of a whole-revision snapshot:
/// its coverage tuple, the producers covering it, its proofs and every row those proofs
/// own. Import-time coverage reasons belong to whole-revision snapshots only, never to an
/// answer projection, so they are dropped.
fn document_answer(snapshot: &SemanticSnapshot, path: &str) -> SemanticSnapshot {
    let coverage: Vec<_> = snapshot
        .coverage
        .iter()
        .filter(|c| c.document_path == path)
        .cloned()
        .map(|mut c| {
            c.reasons = None;
            c
        })
        .collect();
    let producers: BTreeSet<&str> = coverage.iter().map(|c| c.producer_id.as_str()).collect();
    let provenance: Vec<_> = snapshot
        .provenance
        .iter()
        .filter(|p| p.document.path == path)
        .cloned()
        .collect();
    let owned: BTreeSet<&str> = provenance.iter().map(|p| p.id.as_str()).collect();
    fn owned_by<T: Clone>(rows: &[T], owned: &BTreeSet<&str>, id: fn(&T) -> &str) -> Vec<T> {
        rows.iter()
            .filter(|r| owned.contains(id(r)))
            .cloned()
            .collect()
    }
    SemanticSnapshot {
        producers: snapshot
            .producers
            .iter()
            .filter(|p| producers.contains(p.id.as_str()))
            .cloned()
            .collect(),
        symbols: owned_by(&snapshot.symbols, &owned, |r| &r.provenance_id),
        declaration_bindings: owned_by(&snapshot.declaration_bindings, &owned, |r| {
            &r.provenance_id
        }),
        references: owned_by(&snapshot.references, &owned, |r| &r.provenance_id),
        call_bindings: owned_by(&snapshot.call_bindings, &owned, |r| &r.provenance_id),
        type_relationships: owned_by(&snapshot.type_relationships, &owned, |r| &r.provenance_id),
        coverage,
        provenance,
    }
}

/// Property 3's row check: an older-basis document read serves exactly P's rows for it.
fn check_older_basis_rows(
    label: &str,
    path: &str,
    rows: &SemanticSnapshot,
    expected: &[String],
) -> Result<(), String> {
    let actual = snapshot_rows(rows);
    if actual != expected {
        let missing: Vec<_> = expected.iter().filter(|r| !actual.contains(r)).collect();
        let extra: Vec<_> = actual.iter().filter(|r| !expected.contains(r)).collect();
        return Err(format!(
            "{label}: older-basis {path} rows differ from P ({} served, {} expected)\n\
             missing {missing:#?}\nextra {extra:#?}",
            actual.len(),
            expected.len()
        ));
    }
    Ok(())
}

/// A later pin of the same generation.
fn advances(next: IndexPin, prior: IndexPin) -> bool {
    next.index_generation == prior.index_generation && next.index_revision > prior.index_revision
}

fn pin_key(pin: IndexPin) -> String {
    format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision)
}

/// The test-support work counts S wrote for `pin`, if any.
fn work_counts(pin: IndexPin) -> Result<Option<serde_json::Value>, String> {
    let Some(file) = std::env::var_os("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE") else {
        return Ok(None);
    };
    let Ok(bytes) = fs::read(file) else {
        return Ok(None);
    };
    let work: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("work counts unreadable: {e}"))?;
    if work["indexPin"] != serde_json::to_value(pin).unwrap() {
        return Ok(None);
    }
    Ok(Some(work))
}

fn clear_work_counts() {
    if let Some(file) = std::env::var_os("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE") {
        let _ = fs::remove_file(file);
    }
}

fn copied_paths(work: &serde_json::Value) -> BTreeSet<String> {
    paths_of(work, "copiedDocuments")
}

fn paths_of(work: &serde_json::Value, field: &str) -> BTreeSet<String> {
    work[field]
        .as_array()
        .map(|docs| {
            docs.iter()
                .filter_map(|d| d["path"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Replace, everywhere except inside a historical `basis`, the old native revision and
/// mapped provenance IDs: the re-keying the copy ruling allows, expressed independently.
fn rekeyed(
    value: &serde_json::Value,
    old: &str,
    new: &str,
    ids: &BTreeMap<String, String>,
) -> serde_json::Value {
    use serde_json::Value as V;
    match value {
        V::String(text) if text == old => V::String(new.to_owned()),
        V::String(text) => V::String(ids.get(text).cloned().unwrap_or_else(|| text.clone())),
        V::Array(items) => V::Array(items.iter().map(|v| rekeyed(v, old, new, ids)).collect()),
        V::Object(map) => V::Object(
            map.iter()
                .map(|(k, v)| {
                    let v = if k == "basis" {
                        v.clone()
                    } else {
                        rekeyed(v, old, new, ids)
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Canonical rows of one document in a snapshot (every row kind, owner by provenance).
fn document_rows(snapshot: &SemanticSnapshot, path: &str) -> Vec<String> {
    let ids: BTreeSet<&str> = snapshot
        .provenance
        .iter()
        .filter(|p| p.document.path == path)
        .map(|p| p.id.as_str())
        .collect();
    let mut rows = vec![];
    let mut push = |kind: &str, owner: &str, row: serde_json::Value| {
        if ids.contains(owner) {
            rows.push(format!("{kind}:{row}"));
        }
    };
    for r in &snapshot.symbols {
        push("symbol", &r.provenance_id, serde_json::to_value(r).unwrap());
    }
    for r in &snapshot.declaration_bindings {
        push(
            "declaration",
            &r.provenance_id,
            serde_json::to_value(r).unwrap(),
        );
    }
    for r in &snapshot.references {
        push(
            "reference",
            &r.provenance_id,
            serde_json::to_value(r).unwrap(),
        );
    }
    for r in &snapshot.call_bindings {
        push("call", &r.provenance_id, serde_json::to_value(r).unwrap());
    }
    for r in &snapshot.type_relationships {
        push("type", &r.provenance_id, serde_json::to_value(r).unwrap());
    }
    for c in &snapshot.coverage {
        if c.document_path == path {
            rows.push(format!("coverage:{}", serde_json::to_value(c).unwrap()));
        }
    }
    rows.sort();
    rows
}

/// #669 H1: a copied document's S rows are exactly P's rows re-keyed to N: native
/// revision, rejoined provenance IDs, and the freshness rule (never `stale`; never
/// `fresh` when the recorded basis revision is not N's).
fn check_rekeyed(
    basis: &SemanticSnapshot,
    published: &SemanticSnapshot,
    path: &str,
    new: &str,
) -> Result<(), String> {
    let old = basis
        .coverage
        .iter()
        .find(|c| c.document_path == path)
        .map(|c| c.revision_id.clone())
        .ok_or_else(|| format!("copied {path} had no P coverage"))?;
    let before: Vec<_> = basis
        .provenance
        .iter()
        .filter(|p| p.document.path == path)
        .collect();
    let after: Vec<_> = published
        .provenance
        .iter()
        .filter(|p| p.document.path == path)
        .collect();
    if before.len() != after.len() {
        return Err(format!(
            "copied {path}: {} P proofs, {} S proofs",
            before.len(),
            after.len()
        ));
    }
    let mut ids = BTreeMap::new();
    for p in &before {
        let derived = serde_json::to_value(&p.derived_from).unwrap();
        let q = after
            .iter()
            .find(|q| {
                serde_json::to_value(&q.derived_from).unwrap() == derived
                    && q.evidence_kind == p.evidence_kind
            })
            .ok_or_else(|| format!("copied {path}: no S proof derived like P proof {}", p.id))?;
        if old != new && q.id == p.id {
            return Err(format!("copied {path}: provenance {} not re-keyed", p.id));
        }
        if std::env::var_os("TRELLIS_PROPTEST_TRACE").is_some() {
            eprintln!(
                "copied {path}: P freshness {:?} S freshness {:?} basis revision {:?} N {new}",
                p.freshness,
                q.freshness,
                q.basis.as_ref().map(|b| &b.revision_id)
            );
        }
        if q.freshness == Freshness::Stale {
            return Err(format!("copied {path}: copied proof is stale"));
        }
        if q.basis.as_ref().is_some_and(|b| b.revision_id != new) && q.freshness == Freshness::Fresh
        {
            return Err(format!("copied {path}: older-basis proof copied as fresh"));
        }
        if ids.insert(p.id.clone(), q.id.clone()).is_some() {
            return Err(format!("copied {path}: duplicate P proof {}", p.id));
        }
        let mut expected = rekeyed(&serde_json::to_value(p).unwrap(), &old, new, &ids);
        expected["freshness"] = serde_json::to_value(q.freshness).unwrap();
        if expected != serde_json::to_value(q).unwrap() {
            return Err(format!(
                "copied {path}: proof differs from re-keyed P\nexpected {expected}\nactual {}",
                serde_json::to_value(q).unwrap()
            ));
        }
    }
    let mut expected: Vec<String> = document_rows(basis, path)
        .into_iter()
        .map(|row| {
            let (kind, json) = row.split_once(':').unwrap();
            let value: serde_json::Value = serde_json::from_str(json).unwrap();
            format!("{kind}:{}", rekeyed(&value, &old, new, &ids))
        })
        .collect();
    expected.sort();
    let actual = document_rows(published, path);
    if expected != actual {
        return Err(format!(
            "copied {path}: rows differ from re-keyed P\nexpected {expected:#?}\nactual {actual:#?}"
        ));
    }
    Ok(())
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
/// A projected document read: its label and, when eligible, every row it serves.
type ProjectedRows = (SemanticProjection, Option<Vec<String>>);

#[derive(Debug, Clone, PartialEq)]
struct PinnedView {
    sources: BTreeMap<String, Option<String>>,
    projections: Projections,
    documents: BTreeMap<String, Option<ProjectedRows>>,
    /// Every row of the whole-revision semantic read, at a pin with no projected document
    /// (a projected pin refuses that unlabelled read; its rows are in `documents`).
    semantic: Option<Vec<String>>,
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
                .map(|(projection, rows)| (projection, rows.map(|r| snapshot_rows(&r))));
            documents.insert(path.clone(), document);
        }
        sources.insert(path.clone(), source);
    }
    let present: Vec<String> = sources
        .iter()
        .filter(|(_, s)| s.is_some())
        .map(|(p, _)| p.clone())
        .collect();
    let semantic = match world.store.semantic_at(pin) {
        Ok(snapshot) => Some(snapshot_rows(&snapshot)),
        Err(_) if documents.values().any(Option::is_some) => None,
        Err(e) => {
            return Err(format!(
                "semantic read at unprojected {pin:?} failed: {e:#}"
            ));
        }
    };
    Ok(PinnedView {
        projections: projections(world, pin, producer, &present)?,
        sources,
        documents,
        semantic,
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
    /// Corrupt a copy-eligible document's P rows, then run S (ends the history).
    CorruptCopy {
        pick: usize,
        /// Corrupt what the publisher consumes while writing the copied rows (the basis
        /// artifact hash) instead of what the copy reads (a symbol enum).
        write: bool,
    },
    /// Capture a batch, remove one document, then import below the head.
    DelayedReimport {
        covered: [bool; 4],
        flavor: u8,
        remove: usize,
    },
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
        1 => (0..4usize, any::<bool>()).prop_map(|(pick, write)| Op::CorruptCopy { pick, write }),
        2 => (any::<[bool; 4]>(), 0u8..16, 0..4usize)
            .prop_map(|(covered, flavor, remove)| Op::DelayedReimport { covered, flavor, remove }),
    ]
}

/// The longest generated history.
const MAX_HISTORY: usize = 12;

fn arb_history() -> impl Strategy<Value = Vec<Op>> {
    history_of(1..=MAX_HISTORY)
}

fn history_of(lengths: std::ops::RangeInclusive<usize>) -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(arb_op(), lengths)
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
    copied_docs: usize,
    missed_copies: usize,
    excluded_omitted: usize,
    excluded_captured: usize,
    corruptions_typed: usize,
    corruptions_unreached: usize,
    /// S units that exceeded a resource limit on the witnessed path and took FULL (#826).
    limit_fallbacks: usize,
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
    /// Source bytes the selected import captured (the copy's captured-version condition).
    captured: BTreeMap<String, String>,
    /// Basis rows were corrupted on purpose: the history ends at the typed failure.
    dead: bool,
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
                basis_targets: BTreeMap::new(),
                basis_snapshot: world
                    .store
                    .semantic_at(world.store.index_baseline().unwrap())
                    .map_err(err("initial"))?,
                basis_native: String::new(),
                producer: String::new(),
                basis_tuples: BTreeSet::new(),
                basis_omitted: BTreeSet::new(),
                outstanding: None,
            },
            retained: vec![],
            stats: Stats::default(),
            nonce: 0,
            captured: BTreeMap::new(),
            dead: false,
        };
        harness.reimport("setup", &[true, true, true, true], 0)?;
        Ok(harness)
    }

    fn reimport(&mut self, label: &str, covered: &[bool; 4], flavor: u8) -> Result<(), String> {
        self.import(label, covered, flavor, None)
    }

    /// Import a batch. With `delayed`, capture first, then remove that document and publish
    /// N and S before the import lands below the head: the import then omits associations
    /// whose internal target is gone (the #669 H1 copy exclusion).
    fn import(
        &mut self,
        label: &str,
        covered: &[bool; 4],
        flavor: u8,
        delayed: Option<usize>,
    ) -> Result<(), String> {
        self.nonce = self.nonce.wrapping_add(1);
        let world = self.world;
        if delayed.is_some() {
            self.catch_up(label)?;
        }
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
        let captured_at = world.files();
        if let Some(remove) = delayed
            && world.read(PATHS[remove]).is_some()
            && captured_at.len() > 1
        {
            fs::remove_file(world.root.join(PATHS[remove])).map_err(err(label))?;
            self.publish_native(label, false)?;
        }
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
        let captured = captured_at;
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
                self.captured = captured;
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
                if snapshot_rows(&copied.basis_snapshot)
                    != snapshot_rows(&self.model.basis_snapshot)
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
        let pending = self.model.pending(self.world, n, files)?;
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
                check_older_basis_rows(label, path, &rows, &self.model.rows_of(path))?;
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
                app_paths: None,
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
        let eligible = self.copy_eligible()?;
        let n_native = native_id(world, n)?;
        clear_work_counts();
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
        // #669 H1: only eligible documents are copied, and exactly as re-keyed P rows.
        let published = world
            .store
            .semantic_at(s)
            .map_err(|e| format!("{label}: S semantic read failed: {e:#}"))?;
        // #826: these histories have no corrupt selection, so S never ends unavailable.
        if let Some(coverage) = published.coverage.iter().find(|c| {
            c.diagnostic
                .as_deref()
                .is_some_and(|d| d.starts_with("selected raw capture unavailable"))
        }) {
            return Err(format!(
                "{label}: S published unavailable coverage: {coverage:?}"
            ));
        }
        if let Some(work) = work_counts(s)? {
            self.stats.limit_fallbacks += work["fallbackReasons"].as_array().map_or(0, |reasons| {
                reasons
                    .iter()
                    .filter(|reason| *reason == "witness_limit_exceeded")
                    .count()
            });
            let copied = copied_paths(&work);
            // #826: the report names exactly the documents the publisher wrote as copies, in
            // both directions, so a later check cannot pass on a report missing real copies.
            let published_copies = paths_of(&work, "publishedCopies");
            if copied != published_copies {
                return Err(format!(
                    "{label}: S reported copies {copied:?} but published copies {published_copies:?}"
                ));
            }
            if std::env::var_os("TRELLIS_PROPTEST_TRACE").is_some() {
                eprintln!(
                    "{label}: S {s:?} eligible {eligible:?} copied {copied:?} P omitted {:?} reused {}",
                    self.model.basis_omitted, work["reusedDocuments"]
                );
            }
            if let Some(path) = copied.iter().find(|p| !eligible.contains(*p)) {
                return Err(format!(
                    "{label}: S copied excluded document {path} (eligible {eligible:?}, P omitted {:?}, captured {:?}, current {:?})",
                    self.model.basis_omitted,
                    self.captured.get(path),
                    world.read(path)
                ));
            }
            for path in &copied {
                check_rekeyed(&self.model.basis_snapshot, &published, path, &n_native)
                    .map_err(|e| format!("{label}: {e}"))?;
            }
            self.stats.copied_docs += copied.len();
            self.stats.missed_copies += eligible.difference(&copied).count();
        }
        self.model = Model::at(world, s)?;
        self.retain(s)?;
        self.check_retained(label)
    }

    /// The model's copy eligibility at the outstanding N: not pending, P published a tuple
    /// that omitted nothing, and captured bytes equal current bytes.
    fn copy_eligible(&mut self) -> Result<BTreeSet<String>, String> {
        let files = self.world.files();
        let n = self.model.outstanding.ok_or("no outstanding N")?;
        let pending = self.model.pending(self.world, n, &files)?;
        let mut eligible = BTreeSet::new();
        for (path, content) in &files {
            if pending.contains(path) || !self.model.basis_tuples.contains(path) {
                continue;
            }
            if self.model.basis_omitted.contains(path) {
                self.stats.excluded_omitted += 1;
            } else if self.captured.get(path) != Some(content) {
                self.stats.excluded_captured += 1;
            } else {
                eligible.insert(path.clone());
            }
        }
        Ok(eligible)
    }

    /// #669 H1, #680: S copies a document by naming its basis payload without reading the
    /// payload's rows, so corruption in them (`write` damages the raw proof binding, otherwise
    /// a symbol's enum) is published, and every read of the shared payload fails typed and
    /// schedules a re-index.
    fn corrupt_copy(&mut self, label: &str, pick: usize, write: bool) -> Result<(), String> {
        let world = self.world;
        if self.model.outstanding.is_none()
            && let Some(content) = choose(world, pick % 4, pick)
        {
            world.write(PATHS[pick % 4], content);
            self.publish_native(label, true)?;
        }
        let Some(n) = self.model.outstanding else {
            return Ok(());
        };
        let eligible: Vec<String> = self.copy_eligible()?.into_iter().collect();
        if eligible.is_empty() {
            return Ok(());
        }
        let path = eligible[pick % eligible.len()].clone();
        let key = pin_key(self.model.basis);
        let corrupted = world
            .store
            .with_index_sql_for_tests(|db| {
                if write {
                    // No raw capture has this artifact hash: the publisher's capture lookup
                    // for the copied rows is where the corruption is found (#679 re-review).
                    return Ok(db.execute(
                        "UPDATE semantic_basis SET artifact_hash=?3 WHERE payload_id IN \
                         (SELECT payload_id FROM semantic_coverage WHERE revision_id=?1 AND path=?2)",
                        rusqlite::params![key, path, "0".repeat(64)],
                    )?);
                }
                db.execute_batch("PRAGMA ignore_check_constraints=ON")?;
                let changed = db.execute(
                    "UPDATE semantic_symbols SET key_scope='bogus' WHERE payload_id IN \
                     (SELECT payload_id FROM semantic_coverage WHERE revision_id=?1 AND path=?2)",
                    rusqlite::params![key, path],
                )?;
                db.execute_batch("PRAGMA ignore_check_constraints=OFF")?;
                Ok(changed)
            })
            .map_err(|e| format!("{label}: corruption failed: {e:#}"))?;
        if corrupted == 0 {
            return Ok(());
        }
        self.dead = true;
        // The product path, not the test oracle (which also reads the basis), must fail.
        let oracle = std::env::var_os("TRELLIS_TEST_LOCAL_FULL_ORACLE");
        // SAFETY: the isolated child runs one test on one thread.
        unsafe { std::env::remove_var("TRELLIS_TEST_LOCAL_FULL_ORACLE") };
        clear_work_counts();
        let result =
            world
                .store
                .complete_pending_semantic_for_tests(n, &self.session, &world.cancel);
        if let Some(oracle) = oracle {
            // SAFETY: as above.
            unsafe { std::env::set_var("TRELLIS_TEST_LOCAL_FULL_ORACLE", oracle) };
        }
        match result {
            Err(error) => {
                let text = format!("{error:#}");
                if !write
                    && !(text.contains("incompatible_index") || text.contains("invalid semantic"))
                {
                    return Err(format!(
                        "{label}: corrupt copy of {path} failed untyped: {text}"
                    ));
                }
                if !world.store.recovery_pending_for_tests() {
                    return Err(format!(
                        "{label}: corrupt copy of {path} did not schedule a re-index: {text}"
                    ));
                }
                if world.store.index_baseline().unwrap() != n || !world.stage_dirs().is_empty() {
                    return Err(format!(
                        "{label}: corrupt copy of {path} left a partial S or stage"
                    ));
                }
                self.stats.corruptions_typed += 1;
                Ok(())
            }
            Ok(Some(s)) => {
                let copied = work_counts(s)?
                    .map(|w| copied_paths(&w))
                    .unwrap_or_default();
                if copied.contains(&path) {
                    let text = match world.store.semantic_at(s) {
                        Ok(_) => {
                            return Err(format!(
                                "{label}: read of the corrupt shared payload of {path} succeeded"
                            ));
                        }
                        Err(error) => format!("{error:#}"),
                    };
                    let typed = if write {
                        "incompatible_index"
                    } else {
                        "invalid semantic"
                    };
                    if !text.contains(typed) {
                        return Err(format!(
                            "{label}: corrupt shared payload of {path} failed untyped: {text}"
                        ));
                    }
                    if !world.store.recovery_pending_for_tests() {
                        return Err(format!(
                            "{label}: corrupt shared payload of {path} did not schedule a re-index: {text}"
                        ));
                    }
                    self.stats.corruptions_typed += 1;
                    return Ok(());
                }
                self.stats.corruptions_unreached += 1;
                Ok(())
            }
            Ok(None) => Err(format!("{label}: outstanding {n:?} had no target")),
        }
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
            Op::CorruptCopy { pick, write } => self.corrupt_copy(&label, *pick, *write)?,
            Op::DelayedReimport {
                covered,
                flavor,
                remove,
            } => self.import(&label, covered, *flavor, Some(*remove))?,
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
        if harness.dead {
            return Ok(harness.stats);
        }
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
    isolated_with(test, &[])
}

/// [`isolated`], with `env` also set in the child.
fn isolated_with(test: &str, env: &[(&str, &str)]) -> bool {
    const CHILD: &str = "TRELLIS_PROPTEST_NATIVE_FIRST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    // Calibration only: check that the harness's own copy checks catch a mutation that
    // the in-S FULL oracle would otherwise catch first.
    if std::env::var_os("TRELLIS_PROPTEST_NATIVE_FIRST_NO_ORACLE").is_none() {
        command.env("TRELLIS_TEST_LOCAL_FULL_ORACLE", "1");
    }
    let output = command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env(
            "TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE",
            dir.path().join("counts.json"),
        )
        .env_remove("TRELLIS_TEST_FORCE_FULL_REJOIN")
        .env_remove("TRELLIS_TEST_SEMANTIC_LIMITS")
        .envs(env.iter().copied())
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
        // B (target) goes away: A (run calls target) is a dependent; C (helper calls run) is
        // not.
        Op::Remove {
            path: 1,
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

/// Decision 0021 D4: a comment edit and a body edit of the dependency `B` leave the targeted
/// declaration alone, so `A` stays older-basis (no dependent is pending) and S copies it; a
/// rename of the declaration makes `A` a dependent.
#[test]
fn native_first_dependency_term_control() {
    if !isolated("proptest_native_first::native_first_dependency_term_control") {
        return;
    }
    let comment_and_body = run_history(&[
        // CONTENTS[8]: `// note` before `target`, then CONTENTS[4]: a body edit.
        Op::Edit {
            path: 1,
            choice: 3,
            defer: false,
        },
        Op::Edit {
            path: 1,
            choice: 1,
            defer: false,
        },
    ])
    .unwrap();
    eprintln!("term control stats: {comment_and_body:?}");
    assert_eq!(comment_and_body.dependent_only, 0, "{comment_and_body:?}");
    assert!(
        comment_and_body.older_basis_docs > 0,
        "{comment_and_body:?}"
    );
    let removed = run_history(&[Op::Remove {
        path: 1,
        defer: true,
    }])
    .unwrap();
    assert!(removed.dependent_only > 0, "{removed:?}");
}

/// #669 H1 controls: a below-head import that lost B omits A's association to `target`,
/// so a later unrelated edit must not copy A; corrupt copied rows fail typed.
#[test]
fn native_first_copy_controls() {
    if !isolated("proptest_native_first::native_first_copy_controls") {
        return;
    }
    let omitted = run_history(&[
        Op::DelayedReimport {
            covered: [true, true, true, true],
            flavor: 0,
            remove: 1,
        },
        Op::Edit {
            path: 2,
            choice: 1,
            defer: false,
        },
        // The first S after a below-head import witnesses nothing; this one reuses A.
        Op::Edit {
            path: 2,
            choice: 2,
            defer: false,
        },
    ])
    .unwrap();
    eprintln!("omitted control stats: {omitted:?}");
    assert!(omitted.excluded_omitted > 0, "{omitted:?}");
    assert!(omitted.copied_docs > 0, "{omitted:?}");
    for write in [false, true] {
        let corrupt = run_history(&[Op::CorruptCopy { pick: 2, write }]).unwrap();
        eprintln!("corruption control (write {write}) stats: {corrupt:?}");
        assert_eq!(corrupt.corruptions_typed, 1, "{corrupt:?}");
    }
}

/// #759 mutation control: the older-basis document-read check and the retained-pin view
/// see every row kind, so a served row set with any row missing, extra, duplicated or
/// changed is caught. The rows are a real older-basis read (C, while only B changed).
#[test]
fn native_first_document_rows_control() {
    let world = world(populate);
    let mut harness = Harness::new(&world).unwrap();
    harness
        .apply(
            0,
            &Op::Edit {
                path: 1,
                choice: 1,
                defer: true,
            },
        )
        .unwrap();
    assert!(harness.stats.older_basis_docs > 0, "{:?}", harness.stats);
    let n = world.store.index_baseline().unwrap();
    let path = PATHS[2];
    let key = trellis::native_evidence::DocumentKey {
        source_set_id: source_set(&world, n).unwrap(),
        language: LANGUAGE.into(),
        path: path.into(),
    };
    let (projection, rows) = world
        .store
        .projected_semantic_document_at(n, &harness.model.producer, &key)
        .unwrap()
        .unwrap();
    assert_eq!(projection.semantic_status, SemanticStatus::OlderBasis);
    let rows = rows.expect("older-basis rows");
    let expected = harness.model.rows_of(path);
    check_older_basis_rows("control", path, &rows, &expected).unwrap();
    let view = pinned_view(&world, n, &harness.model.producer).unwrap();
    assert_eq!(
        view,
        pinned_view(&world, n, &harness.model.producer).unwrap()
    );
    assert!(
        view.semantic.is_none(),
        "projected N served an unlabelled read"
    );
    // The validated import pin is retained with its whole-revision rows.
    assert!(
        harness
            .retained
            .iter()
            .any(|(_, v)| v.semantic.as_ref().is_some_and(|rows| !rows.is_empty())),
        "no retained validated pin carries its semantic rows"
    );

    type Mutation = fn(&mut SemanticSnapshot) -> bool;
    fn drop_last<T>(rows: &mut Vec<T>) -> bool {
        rows.pop().is_some()
    }
    let mutations: [(&str, Mutation); 14] = [
        ("clear references", |s| {
            !std::mem::take(&mut s.references).is_empty()
        }),
        ("clear call bindings", |s| {
            !std::mem::take(&mut s.call_bindings).is_empty()
        }),
        ("drop producer", |s| drop_last(&mut s.producers)),
        ("drop coverage", |s| drop_last(&mut s.coverage)),
        ("drop provenance", |s| drop_last(&mut s.provenance)),
        ("drop symbol", |s| drop_last(&mut s.symbols)),
        ("drop declaration", |s| {
            drop_last(&mut s.declaration_bindings)
        }),
        ("drop reference", |s| drop_last(&mut s.references)),
        ("drop call binding", |s| drop_last(&mut s.call_bindings)),
        ("duplicate reference", |s| {
            let first = s.references.first().cloned();
            first.map(|r| s.references.push(r)).is_some()
        }),
        ("change reference spelling", |s| {
            s.references
                .first_mut()
                .map(|r| r.spelling.push('x'))
                .is_some()
        }),
        ("change call dispatch completeness", |s| {
            s.call_bindings
                .first_mut()
                .map(|c| c.possible_dispatch_complete = !c.possible_dispatch_complete)
                .is_some()
        }),
        ("change symbol display name", |s| {
            s.symbols
                .first_mut()
                .map(|r| r.display_name = Some("mutated".into()))
                .is_some()
        }),
        ("change coverage diagnostic", |s| {
            s.coverage
                .first_mut()
                .map(|c| c.diagnostic = Some("mutated".into()))
                .is_some()
        }),
    ];
    for (name, mutate) in mutations {
        let mut altered = rows.clone();
        // Every mutation must bite on this document, or the control proves nothing.
        assert!(mutate(&mut altered), "{name}: {path} has no row to alter");
        assert!(
            check_older_basis_rows("control", path, &altered, &expected).is_err(),
            "{name}: older-basis check missed it"
        );
        let mut changed = view.clone();
        let Some(Some((_, served))) = changed.documents.get_mut(path) else {
            panic!("{name}: {path} is not a projected document read at {n:?}");
        };
        *served = Some(snapshot_rows(&altered));
        assert_ne!(changed, view, "{name}: retained-pin view missed it");
    }
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
                    t.copied_docs += stats.copied_docs;
                    t.missed_copies += stats.missed_copies;
                    t.excluded_omitted += stats.excluded_omitted;
                    t.excluded_captured += stats.excluded_captured;
                    t.corruptions_typed += stats.corruptions_typed;
                    t.corruptions_unreached += stats.corruptions_unreached;
                    t.limit_fallbacks += stats.limit_fallbacks;
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

/// #826: the live budget (`unit_expansion_bytes`) of the reduced-limit histories, 160 KiB.
/// Every FULL (import or S) of this corpus fits in 128 KiB (not in 64 KiB); the witnessed
/// settlement of a pending dependent needs more than 192 KiB (it fits in 256 KiB).
const REDUCED_LIMITS: &str = r#"{"unit_expansion_bytes":163840}"#;

/// #826: under a live budget FULL fits but the witness does not, every S still ends caught
/// up and equal to FULL (the in-S oracle is on) with no unavailable coverage, because the
/// witnessed attempt falls back to FULL. This deterministic history proves the fallback is
/// reached; the generated bands below explore the rest under the same limit.
#[test]
fn native_first_reduced_limit_control() {
    let name = "proptest_native_first::native_first_reduced_limit_control";
    if !isolated_with(name, &[("TRELLIS_TEST_SEMANTIC_LIMITS", REDUCED_LIMITS)]) {
        return;
    }
    // B (target) goes away, so A (run calls target) is a pending dependent the witness settles.
    let control = run_history(&[
        Op::Remove {
            path: 1,
            defer: false,
        },
        Op::Edit {
            path: 2,
            choice: 1,
            defer: false,
        },
    ])
    .unwrap();
    eprintln!("reduced-limit control stats: {control:?}");
    assert!(
        control.limit_fallbacks > 0 && control.catch_ups > 0,
        "{control:?}"
    );
}

/// #838: the generated reduced-limit histories (#826), split by history length so each band
/// finishes well inside the nightly hang backstop with the in-S oracle on. The bands
/// partition `arb_history`'s lengths, and each band runs the share of `cases()` that the
/// undivided strategy (uniform over `1..=MAX_HISTORY`) gave its lengths, rounded up, so the
/// bands together run at least as many histories of every length as one test did.
fn reduced_limit_band(name: &'static str, lengths: std::ops::RangeInclusive<usize>) {
    if !isolated_with(name, &[("TRELLIS_TEST_SEMANTIC_LIMITS", REDUCED_LIMITS)]) {
        return;
    }
    let share = (lengths.end() - lengths.start() + 1) as u32;
    let band_cases = (cases() * share).div_ceil(MAX_HISTORY as u32);
    let config = ProptestConfig {
        cases: band_cases,
        source_file: Some(file!()),
        test_name: Some(name),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::WithSource("proptest-regressions"),
        )),
        ..ProptestConfig::default()
    };
    let fallbacks = std::cell::Cell::new(0);
    proptest::test_runner::TestRunner::new(config)
        .run(&history_of(lengths.clone()), |history| {
            match run_history(&history) {
                Ok(stats) => fallbacks.set(fallbacks.get() + stats.limit_fallbacks),
                Err(error) => prop_assert!(false, "{error}"),
            }
            Ok(())
        })
        .unwrap();
    eprintln!(
        "reduced-limit histories of length {lengths:?} over {band_cases} cases: {} limit fallbacks",
        fallbacks.get()
    );
}

/// One test per band of [`reduced_limit_band`]. A history's cost grows with its length, so
/// the short lengths share bands and each long length has its own; at the nightly case
/// count the longest band takes about a fifth of the undivided run (#838).
macro_rules! reduced_limit_bands {
    ($($test:ident => $lengths:expr),* $(,)?) => {$(
        #[test]
        fn $test() {
            reduced_limit_band(concat!("proptest_native_first::", stringify!($test)), $lengths);
        }
    )*
        const REDUCED_LIMIT_BANDS: &[std::ops::RangeInclusive<usize>] = &[$($lengths),*];
    };
}

reduced_limit_bands! {
    native_first_reduced_limit_histories_1_to_4 => 1..=4,
    native_first_reduced_limit_histories_5_to_6 => 5..=6,
    native_first_reduced_limit_histories_7 => 7..=7,
    native_first_reduced_limit_histories_8 => 8..=8,
    native_first_reduced_limit_histories_9 => 9..=9,
    native_first_reduced_limit_histories_10 => 10..=10,
    native_first_reduced_limit_histories_11 => 11..=11,
    native_first_reduced_limit_histories_12 => 12..=MAX_HISTORY,
}

/// The bands cover every generated history length exactly once, so together they explore
/// the same history space as the undivided strategy.
#[test]
fn reduced_limit_bands_partition_history_lengths() {
    let mut next = 1;
    for band in REDUCED_LIMIT_BANDS {
        assert_eq!(*band.start(), next, "{REDUCED_LIMIT_BANDS:?}");
        assert!(band.start() <= band.end(), "{REDUCED_LIMIT_BANDS:?}");
        next = band.end() + 1;
    }
    assert_eq!(next, MAX_HISTORY + 1, "{REDUCED_LIMIT_BANDS:?}");
}
