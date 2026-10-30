//! Pure graph engine tests over `GraphFacts` fixtures. Expected arrays are the hand-evaluated
//! ones in contract-v1.md ("Hand-evaluated graph cases" and "Worked warning checks").
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use trellis::{
    graph_query::{
        BoundaryReason::{
            self, Ambiguous, External, MissingEvidence, NodeLimit, Stale, Unresolved,
        },
        DocumentFacts, FrontierReason, GraphAnswer, GraphError, GraphErrorCode, GraphFacts,
        GraphIntegrity, GraphRequest, GraphResult, GraphSnapshot, GraphSource, HistoryFacts,
        SemanticDocument, Visit, WarningCode, evaluate, warnings_v1,
    },
    model::IndexPin,
    native_evidence::{self as native, DocumentKey},
    semantic_evidence::{self as sem, CoverageState, Dispatch, Freshness, Resolution, Target},
};

const SS: &str = "source-set:v1:s";
const REV: &str = "revision:v2:r2";
const PRODUCER: &str = "scip-test";

fn sid(n: u64) -> String {
    format!("sid:v1:{n:032x}")
}
fn occ(n: u64) -> String {
    format!("occ:v2:{n:032x}")
}
fn n_of(id: &str) -> u64 {
    u64::from_str_radix(id.rsplit(':').next().unwrap(), 16).unwrap()
}
fn dkey(path: &str) -> DocumentKey {
    let language = if path.ends_with(".js") {
        "javascript"
    } else {
        "rust"
    };
    DocumentKey {
        source_set_id: SS.into(),
        language: language.into(),
        path: path.into(),
    }
}
fn language(key: &DocumentKey) -> sem::Language {
    if key.language == "rust" {
        sem::Language::Rust
    } else {
        sem::Language::Javascript
    }
}
fn coverage(
    producer: &str,
    key: &DocumentKey,
    revision: &str,
    selected: bool,
    state: CoverageState,
) -> sem::Coverage {
    sem::Coverage {
        producer_id: producer.into(),
        language: language(key),
        source_set_id: key.source_set_id.clone(),
        document_path: key.path.clone(),
        revision_id: revision.into(),
        requested: true,
        selected,
        state,
        supported_roles: vec![],
        observed_roles: vec![],
        diagnostic: None,
    }
}
fn provenance(
    id: &str,
    producer: &str,
    key: &DocumentKey,
    freshness: Freshness,
) -> sem::Provenance {
    sem::Provenance {
        id: id.into(),
        producer_id: producer.into(),
        document: key.clone(),
        revision_id: REV.into(),
        content_hash: "h".into(),
        evidence_kind: sem::EvidenceKind::SemanticReference,
        basis: None,
        freshness,
        derived_from: None,
    }
}

#[derive(Clone)]
enum Tgt {
    Internal(u64),
    External,
    Nothing,
}

#[derive(Clone)]
struct Bind {
    call_id: bool,
    target: Tgt,
    resolution: Resolution,
    dispatch: Dispatch,
    stale_target: Option<bool>,
    freshness: Freshness,
    prov: Option<String>,
    possible: Vec<u64>,
}

fn direct(target: u64) -> Bind {
    Bind {
        call_id: true,
        target: Tgt::Internal(target),
        resolution: Resolution::Resolved,
        dispatch: Dispatch::Direct,
        stale_target: Some(false),
        freshness: Freshness::Fresh,
        prov: None,
        possible: vec![],
    }
}

struct World {
    producer: bool,
    scope: BTreeSet<String>,
    docs: BTreeMap<DocumentKey, DocumentFacts>,
    decl_doc: BTreeMap<u64, DocumentKey>,
    call_doc: BTreeMap<u64, DocumentKey>,
    call_owner_count: BTreeMap<u64, usize>,
    no_location: BTreeSet<u64>,
}

fn world(producer: bool) -> World {
    World {
        producer,
        scope: BTreeSet::from(["rust".to_string()]),
        docs: BTreeMap::new(),
        decl_doc: BTreeMap::new(),
        call_doc: BTreeMap::new(),
        call_owner_count: BTreeMap::new(),
        no_location: BTreeSet::new(),
    }
}

impl World {
    fn doc(&mut self, path: &str) -> &mut DocumentFacts {
        let producer = self.producer;
        let in_scope = self.scope.contains(&dkey(path).language);
        self.docs.entry(dkey(path)).or_insert_with(|| {
            let key = dkey(path);
            DocumentFacts {
                key: key.clone(),
                declarations: BTreeMap::new(),
                calls: BTreeMap::new(),
                native_coverage: coverage("native", &key, REV, true, CoverageState::Complete),
                native_provenance: sem::Provenance {
                    evidence_kind: sem::EvidenceKind::MeasuredSyntax,
                    ..provenance(&format!("native:{path}"), "native", &key, Freshness::Fresh)
                },
                semantic: (producer && in_scope).then(|| SemanticDocument {
                    coverage: coverage(PRODUCER, &key, REV, true, CoverageState::Complete),
                    provenance: BTreeMap::new(),
                    call_bindings: vec![],
                    declaration_bindings: vec![],
                    symbols: vec![],
                    type_relationships: vec![],
                }),
            }
        })
    }

    fn decl_named(&mut self, n: u64, path: &str, name: &str) {
        let key = dkey(path);
        let provenance_id = format!("native:{path}");
        let declaration_key = native::Key {
            kind: "function".into(),
            name: Some(name.into()),
            signature: None,
            ordinal: n as usize,
        };
        let declaration = native::Declaration {
            role: native::DeclarationRole::Production,
            syntax_id: sid(n),
            document: key.clone(),
            revision_id: REV.into(),
            kind: "function".into(),
            name: Some(name.into()),
            lookup_key: Some(name.into()),
            ancestors: vec![],
            key: declaration_key,
            range: native::Range { start: 0, end: 1 },
            name_range: None,
            header: native::Header {
                kind: "function".into(),
                name: Some(name.into()),
                modifiers: vec![],
                type_parameters: vec![],
                parameters: vec![],
                result_type: None,
                bases: vec![],
            },
            provenance_id,
        };
        self.doc(path).declarations.insert(sid(n), declaration);
        self.decl_doc.insert(n, key);
    }

    fn decl(&mut self, n: u64, path: &str) {
        self.decl_named(n, path, &format!("f{n}"));
    }

    /// Calls must be added in local source order so the ordinal matches.
    fn call(&mut self, owner: u64, id: u64, start: usize) {
        self.call_with(owner, id, start, start + 1);
    }

    fn call_with(&mut self, owner: u64, id: u64, start: usize, end: usize) {
        let key = self.decl_doc[&owner].clone();
        let ordinal = self.call_owner_count.entry(owner).or_default();
        let call = native::Call {
            id: occ(id),
            owner_syntax_id: sid(owner),
            ordinal: *ordinal,
            document: key.clone(),
            revision_id: REV.into(),
            range: native::Range { start, end },
            callee_range: None,
            spelling: Some(format!("c{id}")),
            region_ids: vec![],
            provenance_id: format!("native:{}", key.path),
        };
        *ordinal += 1;
        self.call_doc.insert(id, key.clone());
        self.doc(&key.path.clone())
            .calls
            .entry(sid(owner))
            .or_default()
            .push(call);
    }

    fn bind(&mut self, call: u64, bind: Bind) {
        let key = self.call_doc[&call].clone();
        let target_of = |world: &World, n: u64| Target::Internal {
            syntax_id: sid(n),
            document: world.decl_doc[&n].clone(),
            revision_id: REV.into(),
        };
        let declared_target = match &bind.target {
            Tgt::Internal(n) => Some(target_of(self, *n)),
            Tgt::External => Some(Target::External {
                symbol: sem::SymbolKey {
                    scheme: sem::SymbolScheme::Scip,
                    symbol: "ext".into(),
                    scope: sem::SymbolScope::Global,
                    document: None,
                },
            }),
            Tgt::Nothing => None,
        };
        let candidates = if bind.resolution == Resolution::Ambiguous {
            vec![target_of(self, 90), target_of(self, 91)]
        } else {
            vec![]
        };
        let possible_dispatch = bind.possible.iter().map(|n| target_of(self, *n)).collect();
        let prov = bind.prov.clone().unwrap_or_else(|| format!("pv:{call}"));
        let binding = sem::CallBinding {
            call_id: bind.call_id.then(|| occ(call)),
            join: sem::Join {
                anchor: sem::MeasuredAnchor {
                    document: key.clone(),
                    revision_id: REV.into(),
                    content_hash: "h".into(),
                    range: native::Range { start: 0, end: 1 },
                    kind: sem::JoinKind::Callee,
                },
                status: sem::JoinStatus::Exact,
                candidate_ids: vec![occ(call)],
                diagnostic: None,
            },
            resolution: bind.resolution,
            declared_target,
            candidates,
            dispatch: bind.dispatch,
            possible_dispatch,
            possible_dispatch_complete: false,
            stale_target: bind.stale_target,
            provenance_id: prov.clone(),
        };
        let semantic = self
            .doc(&key.path)
            .semantic
            .as_mut()
            .expect("binding needs an in-scope document");
        semantic
            .provenance
            .entry(prov.clone())
            .or_insert_with(|| provenance(&prov, PRODUCER, &key, bind.freshness));
        semantic.call_bindings.push(binding);
    }

    fn state(&mut self, path: &str, state: CoverageState, selected: bool) {
        let row = &mut self.doc(path).semantic.as_mut().unwrap().coverage;
        row.state = state;
        row.selected = selected;
    }

    fn build(self) -> GraphFacts {
        let mut facts = GraphFacts::complete(GraphSnapshot {
            source_set_id: SS.into(),
            revision_id: REV.into(),
        });
        if self.producer {
            facts.producers.insert(PRODUCER.into(), self.scope);
        }
        for (n, key) in &self.decl_doc {
            let location = (!self.no_location.contains(n)).then(|| key.clone());
            facts.locations.insert(sid(*n), location);
        }
        for (key, doc) in self.docs {
            facts.documents.insert(key, Arc::new(doc));
        }
        facts
    }
}

fn request(root: u64, producer: bool) -> GraphRequest {
    GraphRequest {
        source_set_id: SS.into(),
        revision_id: REV.into(),
        root_syntax_id: sid(root),
        semantic_producer_id: producer.then(|| PRODUCER.into()),
        depth: 2,
        max_nodes: 150,
        max_calls: 500,
        include_external: true,
        include_tests: true,
    }
}

fn run(facts: &mut GraphFacts, request: &GraphRequest) -> GraphResult {
    evaluate(request, facts)
        .expect("evaluation reads")
        .expect("valid answer")
}

type E = (u64, u64, Option<u64>, Visit, BoundaryReason);
type F = (
    FrontierReason,
    u64,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    u64,
);

fn nodes(r: &GraphResult) -> Vec<(u64, u64)> {
    r.nodes
        .iter()
        .map(|n| (n_of(&n.declaration.syntax_id), n.depth))
        .collect()
}
fn edges(r: &GraphResult) -> Vec<E> {
    r.edges
        .iter()
        .map(|e| {
            (
                n_of(&e.call.id),
                n_of(&e.from),
                e.to.as_deref().map(n_of),
                e.visit,
                e.boundary_reason,
            )
        })
        .collect()
}
fn frontier(r: &GraphResult) -> Vec<F> {
    r.frontier
        .iter()
        .map(|f| {
            (
                f.reason,
                n_of(&f.node_id),
                f.call_id.as_deref().map(n_of),
                f.target_id.as_deref().map(n_of),
                f.next_ordinal,
                f.omitted_calls,
            )
        })
        .collect()
}
fn keys(warnings: &[trellis::graph_query::Warning]) -> Vec<(WarningCode, Option<String>)> {
    warnings
        .iter()
        .map(|w| (w.code, w.provenance_id.clone()))
        .collect()
}

/// A world with `direct` bindings for each (owner, call, target) triple, ordered by call start.
fn graph(calls: &[(u64, u64, u64)], producer: bool) -> World {
    let mut w = world(producer);
    let mut ids = BTreeSet::new();
    for (owner, _, target) in calls {
        ids.insert(*owner);
        ids.insert(*target);
    }
    for n in ids {
        w.decl(n, "src/lib.rs");
    }
    for (i, (owner, call, target)) in calls.iter().enumerate() {
        w.call(*owner, *call, 10 + i * 10);
        if producer {
            w.bind(*call, direct(*target));
        }
    }
    w
}

// Declarations are 1..: A=1, B=2, C=3, D=4, F=5, G=6.
const A: u64 = 1;
const B: u64 = 2;
const C: u64 = 3;
const D: u64 = 4;

#[test]
fn case1_source_order_versus_evaluation() {
    let (f, g) = (5, 6);
    let mut w = world(true);
    for n in [A, f, g] {
        w.decl(n, "src/lib.rs");
    }
    w.call_with(A, 101, 12, 18);
    w.call_with(A, 100, 10, 20);
    // ordinals follow insertion, so give both bindings and let the engine sort by range.
    w.bind(101, direct(g));
    w.bind(100, direct(f));
    let mut facts = w.build();
    let r = run(&mut facts, &request(A, true));
    assert_eq!(nodes(&r), [(A, 0), (f, 1), (g, 1)]);
    assert_eq!(
        edges(&r),
        [
            (100, A, Some(f), Visit::New, NONE),
            (101, A, Some(g), Visit::New, NONE)
        ]
    );
    assert!(r.frontier.is_empty());
}

const NONE: BoundaryReason = BoundaryReason::None;

#[test]
fn case2_callback_reference_is_not_a_call() {
    let (s, callback) = (5, 6);
    let mut w = world(true);
    for n in [A, s, callback] {
        w.decl(n, "src/lib.rs");
    }
    w.call(A, 200, 10);
    w.bind(200, direct(s));
    let mut facts = w.build();
    let mut req = request(A, true);
    req.depth = 1;
    let r = run(&mut facts, &req);
    assert_eq!(nodes(&r), [(A, 0), (s, 1)]);
    assert_eq!(edges(&r), [(200, A, Some(s), Visit::New, NONE)]);
    assert!(r.frontier.is_empty());
    assert!(!r.truncated && !r.partial);
}

#[test]
fn case3_cycle_and_self_loop() {
    let mut facts = graph(&[(A, 1000, B), (B, 2000, A)], true).build();
    let r = run(&mut facts, &request(A, true));
    assert_eq!(nodes(&r), [(A, 0), (B, 1)]);
    assert_eq!(
        edges(&r),
        [
            (1000, A, Some(B), Visit::New, NONE),
            (2000, B, Some(A), Visit::Seen, NONE)
        ]
    );
    assert!(r.frontier.is_empty());

    let mut facts = graph(&[(A, 1000, A)], true).build();
    let r = run(&mut facts, &request(A, true));
    assert_eq!(nodes(&r), [(A, 0)]);
    assert_eq!(edges(&r), [(1000, A, Some(A), Visit::Seen, NONE)]);
    assert!(r.frontier.is_empty());
}

#[test]
fn case4_queued_visited_diamond() {
    // Call starts follow the slice order: a0, a1, b0, c0.
    let mut facts = graph(&[(A, 10, B), (A, 11, C), (B, 20, D), (C, 30, D)], true).build();
    let mut req = request(A, true);
    req.depth = 3;
    let r = run(&mut facts, &req);
    assert_eq!(nodes(&r), [(A, 0), (B, 1), (C, 1), (D, 2)]);
    assert_eq!(
        edges(&r),
        [
            (10, A, Some(B), Visit::New, NONE),
            (11, A, Some(C), Visit::New, NONE),
            (20, B, Some(D), Visit::New, NONE),
            (30, C, Some(D), Visit::Seen, NONE),
        ]
    );
    assert!(r.frontier.is_empty());
}

#[test]
fn case5_depth_one_and_zero() {
    let mut req = request(A, true);
    req.depth = 1;
    let mut facts = graph(&[(A, 10, B), (B, 20, C)], true).build();
    let r = run(&mut facts, &req);
    assert_eq!(nodes(&r), [(A, 0), (B, 1)]);
    assert_eq!(edges(&r), [(10, A, Some(B), Visit::New, NONE)]);
    assert_eq!(
        frontier(&r),
        [(FrontierReason::Depth, B, None, None, Some(0), 1)]
    );
    assert!(r.truncated && r.partial);

    req.depth = 0;
    let r = run(&mut facts, &req);
    assert_eq!(nodes(&r), [(A, 0)]);
    assert!(r.edges.is_empty());
    assert_eq!(
        frontier(&r),
        [(FrontierReason::Depth, A, None, None, Some(0), 1)]
    );
}

#[test]
fn case6_node_cap_with_later_seen_edge() {
    let mut facts = graph(&[(A, 10, B), (A, 11, C), (A, 12, A)], true).build();
    let mut req = request(A, true);
    req.max_nodes = 2;
    let r = run(&mut facts, &req);
    assert_eq!(nodes(&r), [(A, 0), (B, 1)]);
    assert_eq!(
        edges(&r),
        [
            (10, A, Some(B), Visit::New, NONE),
            (11, A, None, Visit::Boundary, NodeLimit),
            (12, A, Some(A), Visit::Seen, NONE),
        ]
    );
    assert_eq!(
        frontier(&r),
        [(FrontierReason::NodeLimit, A, Some(11), Some(C), None, 0)]
    );
}

#[test]
fn case7_global_call_cap_three_subcases() {
    let mut facts = graph(&[(A, 10, B), (A, 11, C), (B, 20, D)], true).build();
    let mut req = request(A, true);
    req.max_calls = 1;
    let r = run(&mut facts, &req);
    assert_eq!(nodes(&r), [(A, 0), (B, 1)]);
    assert_eq!(edges(&r), [(10, A, Some(B), Visit::New, NONE)]);
    assert_eq!(
        frontier(&r),
        [
            (FrontierReason::CallLimit, A, None, None, None, 1),
            (FrontierReason::CallLimit, B, None, None, None, 1),
        ]
    );

    req.max_calls = 0;
    let r = run(&mut facts, &req);
    assert_eq!(nodes(&r), [(A, 0)]);
    assert!(r.edges.is_empty());
    assert_eq!(
        frontier(&r),
        [(FrontierReason::CallLimit, A, None, None, None, 2)]
    );

    let mut facts = graph(&[(A, 10, B)], true).build();
    let mut req = request(A, true);
    req.max_calls = 1;
    let r = run(&mut facts, &req);
    assert_eq!(nodes(&r), [(A, 0), (B, 1)]);
    assert_eq!(edges(&r).len(), 1);
    assert!(r.frontier.is_empty() && !r.truncated);
}

#[test]
fn case8a_virtual_dispatch_boundary() {
    let v = 5;
    let mut w = world(true);
    for n in [A, B, v] {
        w.decl(n, "src/lib.rs");
    }
    w.call(A, 10, 10);
    w.bind(
        10,
        Bind {
            dispatch: Dispatch::Virtual,
            possible: vec![v],
            ..direct(B)
        },
    );
    let mut facts = w.build();
    let r = run(&mut facts, &request(A, true));
    assert_eq!(nodes(&r), [(A, 0)]);
    assert_eq!(edges(&r), [(10, A, None, Visit::Boundary, DISPATCH)]);
    assert!(r.frontier.is_empty());
    assert!(r.partial && !r.truncated);
}
const DISPATCH: BoundaryReason = BoundaryReason::Dispatch;

#[test]
fn case8b_possibly_stale_precedes_dispatch() {
    let v = 5;
    let mut w = world(true);
    for n in [A, B, v] {
        w.decl(n, "src/lib.rs");
    }
    w.call(A, 10, 10);
    w.call(A, 11, 20);
    let stale = |call: u64| Bind {
        freshness: Freshness::PossiblyStale,
        prov: Some(format!("p{call}")),
        ..direct(B)
    };
    w.bind(
        10,
        Bind {
            dispatch: Dispatch::Virtual,
            possible: vec![v],
            ..stale(10)
        },
    );
    w.bind(11, stale(11));
    let mut facts = w.build();
    let r = run(&mut facts, &request(A, true));
    assert_eq!(nodes(&r), [(A, 0)]);
    assert_eq!(
        edges(&r),
        [
            (10, A, None, Visit::Boundary, Stale),
            (11, A, None, Visit::Boundary, Stale)
        ]
    );
    assert!(r.frontier.is_empty() && r.partial);
}

fn key_array(items: &[(WarningCode, Option<&str>)]) -> Vec<(WarningCode, Option<String>)> {
    items
        .iter()
        .map(|(c, p)| (*c, p.map(str::to_string)))
        .collect()
}

/// Asserts the key array through both `evaluate` and the standalone `warnings_v1`.
fn assert_keys(
    request: &GraphRequest,
    result: &GraphResult,
    expected: &[(WarningCode, Option<&str>)],
) {
    assert_eq!(keys(&result.warnings), key_array(expected));
    let again = warnings_v1(request, &result.coverage, &result.provenance, &result.edges);
    assert_eq!(again, result.warnings);
}

#[test]
fn warning_check1_syntax_only() {
    let mut w = world(false);
    w.decl(A, "src/lib.rs");
    let mut facts = w.build();
    let req = request(A, false);
    let r = run(&mut facts, &req);
    assert!(!r.partial);
    assert_keys(&req, &r, &[(WarningCode::SyntaxOnly, None)]);
}

#[test]
fn warning_check2_two_ambiguous_bindings_one_provenance() {
    let mut w = world(true);
    for n in [A, 90, 91] {
        w.decl(n, "src/lib.rs");
    }
    w.call(A, 10, 10);
    w.call(A, 11, 20);
    for call in [10, 11] {
        w.bind(
            call,
            Bind {
                target: Tgt::Nothing,
                resolution: Resolution::Ambiguous,
                stale_target: None,
                prov: Some("pA".into()),
                ..direct(0)
            },
        );
    }
    let mut facts = w.build();
    let req = request(A, true);
    let r = run(&mut facts, &req);
    assert!(r.edges.iter().all(|e| e.boundary_reason == Ambiguous));
    assert!(r.partial);
    assert_keys(&req, &r, &[(WarningCode::BindingAmbiguous, Some("pA"))]);
}

#[test]
fn warning_check3_three_possibly_stale_provenances() {
    let mut w = world(true);
    for n in [A, B] {
        w.decl(n, "src/lib.rs");
    }
    for (i, p) in ["p1", "p2", "p3"].iter().enumerate() {
        w.call(A, 10 + i as u64, 10 + i * 10);
        w.bind(
            10 + i as u64,
            Bind {
                freshness: Freshness::PossiblyStale,
                prov: Some((*p).into()),
                ..direct(B)
            },
        );
    }
    let mut facts = w.build();
    let req = request(A, true);
    let r = run(&mut facts, &req);
    assert_eq!(
        r.provenance
            .iter()
            .filter(|p| p.freshness == Freshness::PossiblyStale)
            .count(),
        3
    );
    assert_keys(&req, &r, &[(WarningCode::StaleEvidence, None)]);
}

#[test]
fn warning_check4_mixed_freshness_with_stale_boundary() {
    // A (document D1) has c0 bound to possiblyStale p3 and c1 bound fresh to G in D2,
    // whose current producer row is omitted; its history holds p1 (possiblyStale) and p2 (stale).
    let g = 6;
    let mut w = world(true);
    w.decl(A, "src/lib.rs");
    w.decl(B, "src/lib.rs");
    w.decl(g, "src/other.rs");
    w.call(A, 10, 10);
    w.call(A, 11, 20);
    w.bind(
        10,
        Bind {
            freshness: Freshness::PossiblyStale,
            prov: Some("p3".into()),
            ..direct(B)
        },
    );
    w.bind(
        11,
        Bind {
            prov: Some("pF".into()),
            ..direct(g)
        },
    );
    w.doc("src/other.rs");
    w.state("src/other.rs", CoverageState::Omitted, false);
    let history_key = dkey("src/other.rs");
    let mut facts = w.build();
    facts.history.insert(
        (history_key.clone(), sid(g)),
        Some(Arc::new(HistoryFacts {
            coverage: coverage(
                PRODUCER,
                &history_key,
                "revision:v2:r1",
                true,
                CoverageState::Complete,
            ),
            provenance: vec![
                provenance("p1", PRODUCER, &history_key, Freshness::PossiblyStale),
                provenance("p2", PRODUCER, &history_key, Freshness::Stale),
            ],
        })),
    );
    let req = request(A, true);
    let r = run(&mut facts, &req);
    assert_eq!(
        edges(&r),
        [
            (11, A, Some(g), Visit::New, NONE),
            (10, A, None, Visit::Boundary, Stale)
        ]
    );
    assert!(r.partial && !r.truncated);
    assert_keys(
        &req,
        &r,
        &[
            (WarningCode::StaleEvidence, None),
            (WarningCode::StaleEvidence, Some("p2")),
        ],
    );
}

#[test]
fn warning_check5_coverage_versus_other_partial_causes() {
    let build = |state: CoverageState| {
        let mut w = world(true);
        w.decl(A, "src/lib.rs");
        w.decl(B, "src/other.rs");
        w.call(A, 10, 10);
        w.bind(10, direct(B));
        w.state("src/lib.rs", state, true);
        w.state("src/other.rs", CoverageState::Unsupported, false);
        w.build()
    };
    let req = request(A, true);
    let mut facts = build(CoverageState::Partial);
    let r = run(&mut facts, &req);
    assert!(r.partial && !r.truncated);
    assert_keys(&req, &r, &[(WarningCode::CoverageIncomplete, None)]);

    let mut facts = build(CoverageState::Complete);
    let r = run(&mut facts, &req);
    assert!(r.frontier.is_empty() && !r.partial);
    assert_keys(&req, &r, &[]);

    let depth_zero = GraphRequest { depth: 0, ..req };
    let r = run(&mut facts, &depth_zero);
    assert!(r.partial && r.truncated);
    assert_keys(&depth_zero, &r, &[]);
}

/// Outcome of the single call `A -> B` (call 10) after `setup` changes the world.
fn matrix_row(producer: bool, setup: impl FnOnce(&mut World)) -> (Visit, BoundaryReason) {
    let mut w = world(producer);
    w.decl(A, "src/lib.rs");
    w.decl(B, "src/lib.rs");
    for n in [5, 90, 91] {
        w.decl(n, "src/lib.rs");
    }
    w.call(A, 10, 10);
    setup(&mut w);
    let mut facts = w.build();
    let r = run(&mut facts, &request(A, producer));
    assert_eq!(r.edges.len(), 1);
    (r.edges[0].visit, r.edges[0].boundary_reason)
}

fn row(bind: Bind) -> (Visit, BoundaryReason) {
    matrix_row(true, |w| w.bind(10, bind))
}

#[test]
fn expansion_matrix() {
    let missing = (Visit::Boundary, MissingEvidence);
    let stale = (Visit::Boundary, Stale);
    // 1: no producer, and a producer without a binding.
    assert_eq!(matrix_row(false, |_| {}), missing);
    assert_eq!(matrix_row(true, |_| {}), missing);
    // 2-5: in-scope row that cannot authorize a binding, binding present.
    for state in [
        CoverageState::Failed,
        CoverageState::Omitted,
        CoverageState::Unsupported,
        CoverageState::NotRequested,
    ] {
        let outcome = matrix_row(true, |w| {
            w.bind(10, direct(B));
            w.state("src/lib.rs", state, state == CoverageState::Failed);
        });
        assert_eq!(outcome, missing, "{state:?}");
    }
    // 6: document outside the producer scope.
    let mut w = world(true);
    w.scope = BTreeSet::from(["javascript".to_string()]);
    w.decl(A, "src/lib.rs");
    w.call(A, 10, 10);
    let mut facts = w.build();
    let r = run(&mut facts, &request(A, true));
    assert_eq!((r.edges[0].visit, r.edges[0].boundary_reason), missing);
    assert_eq!(r.coverage.len(), 1);
    assert!(r.warnings.is_empty());
    // 7: call_id null.
    assert_eq!(
        row(Bind {
            call_id: false,
            ..direct(B)
        }),
        missing
    );
    // 8-9: provenance freshness.
    for freshness in [Freshness::PossiblyStale, Freshness::Stale] {
        assert_eq!(
            row(Bind {
                freshness,
                ..direct(B)
            }),
            stale
        );
    }
    // 10-11: internal target with staleTarget null or true.
    for stale_target in [None, Some(true)] {
        assert_eq!(
            row(Bind {
                stale_target,
                ..direct(B)
            }),
            stale
        );
    }
    // 12-14: resolution reasons.
    assert_eq!(
        row(Bind {
            target: Tgt::Nothing,
            resolution: Resolution::Ambiguous,
            stale_target: None,
            ..direct(0)
        }),
        (Visit::Boundary, Ambiguous)
    );
    assert_eq!(
        row(Bind {
            target: Tgt::Nothing,
            resolution: Resolution::Unresolved,
            stale_target: None,
            ..direct(0)
        }),
        (Visit::Boundary, Unresolved)
    );
    assert_eq!(
        row(Bind {
            target: Tgt::External,
            resolution: Resolution::External,
            stale_target: None,
            ..direct(0)
        }),
        (Visit::Boundary, External)
    );
    // 15-18: non-static dispatch, including a possible target that is never admitted.
    for dispatch in [
        Dispatch::Virtual,
        Dispatch::Interface,
        Dispatch::Dynamic,
        Dispatch::Unknown,
    ] {
        let outcome = row(Bind {
            dispatch,
            possible: vec![5],
            ..direct(B)
        });
        assert_eq!(outcome, (Visit::Boundary, DISPATCH), "{dispatch:?}");
    }
    // 19: resolved internal target not located. 20: located in another document.
    assert_eq!(
        matrix_row(true, |w| {
            w.bind(10, direct(B));
            w.no_location.insert(B);
        }),
        stale
    );
    let mut w = world(true);
    w.decl(A, "src/lib.rs");
    w.decl(B, "src/lib.rs");
    w.call(A, 10, 10);
    w.bind(10, direct(B));
    let mut facts = w.build();
    facts
        .locations
        .insert(sid(B), Some(dkey("src/elsewhere.rs")));
    let r = run(&mut facts, &request(A, true));
    assert_eq!((r.edges[0].visit, r.edges[0].boundary_reason), stale);
    // 21-22: fresh exact internal direct and constructor.
    for dispatch in [Dispatch::Direct, Dispatch::Constructor] {
        assert_eq!(
            row(Bind {
                dispatch,
                ..direct(B)
            }),
            (Visit::New, NONE)
        );
    }
}

#[test]
fn stale_evidence_and_history_warnings() {
    // Stale and possiblyStale provenance on bound edges.
    let mut w = world(true);
    for n in [A, B, C] {
        w.decl(n, "src/lib.rs");
    }
    w.call(A, 10, 10);
    w.call(A, 11, 20);
    w.bind(
        10,
        Bind {
            freshness: Freshness::Stale,
            prov: Some("ps".into()),
            ..direct(B)
        },
    );
    w.bind(
        11,
        Bind {
            freshness: Freshness::PossiblyStale,
            prov: Some("pp".into()),
            ..direct(C)
        },
    );
    let mut facts = w.build();
    let req = request(A, true);
    let r = run(&mut facts, &req);
    assert_keys(
        &req,
        &r,
        &[
            (WarningCode::StaleEvidence, None),
            (WarningCode::StaleEvidence, Some("ps")),
        ],
    );

    // History for an in-scope document whose current row failed or omitted.
    for (state, selected) in [
        (CoverageState::Failed, true),
        (CoverageState::Omitted, false),
    ] {
        let mut w = world(true);
        w.decl(A, "src/lib.rs");
        w.decl(B, "src/old.rs");
        w.decl(C, "src/old.rs");
        w.call(A, 10, 10);
        w.call(A, 11, 20);
        w.call(B, 20, 10);
        w.bind(10, direct(B));
        w.bind(11, direct(C));
        w.state("src/old.rs", state, selected);
        let old = dkey("src/old.rs");
        let mut facts = w.build();
        let history = |id: &str| {
            Some(Arc::new(HistoryFacts {
                coverage: coverage(
                    PRODUCER,
                    &old,
                    "revision:v2:r1",
                    true,
                    CoverageState::Complete,
                ),
                provenance: vec![provenance(id, PRODUCER, &old, Freshness::Stale)],
            }))
        };
        facts.history.insert((old.clone(), sid(B)), history("hB"));
        facts.history.insert((old.clone(), sid(C)), history("hC"));
        let r = run(&mut facts, &req);
        // B's own call stays missing evidence: the old binding never applies to it.
        assert!(
            r.edges
                .iter()
                .any(|e| n_of(&e.call.id) == 20 && e.boundary_reason == MissingEvidence)
        );
        let history_rows: Vec<_> = r
            .coverage
            .iter()
            .filter(|c| c.revision_id == "revision:v2:r1")
            .collect();
        assert_eq!(history_rows.len(), 1, "{state:?}");
        let ids: Vec<_> = r.provenance.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"hB") && ids.contains(&"hC"));
        let mut expected = vec![
            (WarningCode::StaleEvidence, Some("hB")),
            (WarningCode::StaleEvidence, Some("hC")),
        ];
        if selected {
            expected.insert(0, (WarningCode::CoverageIncomplete, None));
        }
        assert_keys(&req, &r, &expected);
    }

    // An out-of-scope document gets no history lookup.
    let mut w = world(true);
    w.decl(A, "web/a.js");
    w.call(A, 10, 10);
    let mut facts = w.build();
    let mut recorder = Recorder::new(&mut facts);
    let r = evaluate(&req, &mut recorder).unwrap().unwrap();
    assert_eq!(r.coverage.len(), 1);
    assert!(recorder.memo.history.is_empty());
}

/// Delegates to complete facts and records every answer in memo facts.
struct Recorder<'a> {
    from: &'a mut GraphFacts,
    memo: GraphFacts,
    work_checks: usize,
}

impl<'a> Recorder<'a> {
    fn new(from: &'a mut GraphFacts) -> Self {
        let memo = GraphFacts::memo(from.snapshot().clone());
        Self {
            from,
            memo,
            work_checks: 0,
        }
    }
}

impl GraphSource for Recorder<'_> {
    fn snapshot(&self) -> &GraphSnapshot {
        self.from.snapshot()
    }
    fn producer_languages(&mut self, producer_id: &str) -> Result<Option<BTreeSet<String>>> {
        let found = self.from.producer_languages(producer_id)?;
        if let Some(found) = &found {
            self.memo
                .producers
                .insert(producer_id.into(), found.clone());
        }
        Ok(found)
    }
    fn locate(&mut self, syntax_id: &str) -> Result<Option<DocumentKey>> {
        let found = self.from.locate(syntax_id)?;
        self.memo.locations.insert(syntax_id.into(), found.clone());
        Ok(found)
    }
    fn document(&mut self, key: &DocumentKey) -> Result<Arc<DocumentFacts>> {
        let found = self.from.document(key)?;
        self.memo.documents.insert(key.clone(), found.clone());
        Ok(found)
    }
    fn history(&mut self, key: &DocumentKey, syntax_id: &str) -> Result<Option<Arc<HistoryFacts>>> {
        let found = self.from.history(key, syntax_id)?;
        self.memo
            .history
            .insert((key.clone(), syntax_id.into()), found.clone());
        Ok(found)
    }
    fn check(&mut self) -> Result<()> {
        Ok(())
    }
    fn check_work(&mut self) -> Result<()> {
        self.work_checks += 1;
        Ok(())
    }
}

#[test]
fn facts_cache_miss_is_an_error() {
    let snapshot = GraphSnapshot {
        source_set_id: SS.into(),
        revision_id: REV.into(),
    };
    let mut memo = GraphFacts::memo(snapshot.clone());
    let key = dkey("src/lib.rs");
    let miss = "store_unavailable: graph facts miss";
    assert_eq!(memo.locate(&sid(1)).unwrap_err().to_string(), miss);
    assert_eq!(memo.producer_languages("p").unwrap_err().to_string(), miss);
    assert_eq!(memo.document(&key).unwrap_err().to_string(), miss);
    assert_eq!(memo.history(&key, &sid(1)).unwrap_err().to_string(), miss);

    let mut complete = GraphFacts::complete(snapshot);
    assert_eq!(complete.locate(&sid(1)).unwrap(), Option::None);
    assert!(complete.producer_languages("p").unwrap().is_none());
    assert!(complete.history(&key, &sid(1)).unwrap().is_none());
}

#[test]
fn memo_replay_records_negative_lookups() {
    let (unlocated, failed, fresh) = (B, C, D);
    let mut w = world(true);
    w.decl(A, "src/lib.rs");
    w.decl(unlocated, "src/lib.rs");
    w.decl(failed, "src/old.rs");
    w.decl(fresh, "src/lib.rs");
    for call in 0..3 {
        w.call(A, 10 + call, 10 + call as usize * 10);
    }
    w.call(fresh, 20, 10);
    w.call(fresh, 21, 20);
    w.bind(10, direct(unlocated));
    w.bind(11, direct(failed));
    w.bind(12, direct(fresh));
    w.bind(20, direct(A));
    w.bind(21, direct(unlocated));
    w.state("src/old.rs", CoverageState::Failed, true);
    w.no_location.insert(unlocated);
    let mut facts = w.build();
    let old = dkey("src/old.rs");
    let req = request(A, true);

    let mut recorder = Recorder::new(&mut facts);
    let full = evaluate(&req, &mut recorder).unwrap().unwrap();
    let memo = recorder.memo;
    assert_eq!(memo.locations.get(&sid(unlocated)), Some(&Option::None));
    assert_eq!(memo.history.get(&(old, sid(failed))), Some(&Option::None));

    let answer = GraphAnswer {
        basis: IndexPin {
            index_generation: uuid::Uuid::from_u128(1),
            index_revision: 1,
        },
        requested: req.clone(),
        outcome: Ok(full.clone()),
        facts: memo,
    };
    assert_eq!(full.edges.len(), 5);
    for k in 0..=full.edges.len() {
        let prefix = answer.prefix(k).expect("prefix uses only frozen facts");
        assert_eq!(prefix.request, req);
        assert_eq!(prefix.edges, full.edges[..k]);
        let retained: BTreeSet<_> = prefix
            .nodes
            .iter()
            .map(|n| n.declaration.syntax_id.as_str())
            .collect();
        assert!(prefix.edges.iter().all(|e| {
            retained.contains(e.from.as_str())
                && e.to
                    .as_ref()
                    .is_none_or(|to| retained.contains(to.as_str()))
        }));
        assert!(
            prefix
                .frontier
                .iter()
                .all(|f| retained.contains(f.node_id.as_str()))
        );
        assert_eq!(
            prefix.omitted.response_bytes,
            full.edges[k..]
                .iter()
                .filter(|e| retained.contains(e.from.as_str()))
                .map(|e| e.call_site_count)
                .sum::<u64>()
        );
    }
}

#[test]
fn graph_records_serialize_as_contract_json() {
    let mut w = world(true);
    for n in [A, B] {
        w.decl(n, "src/lib.rs");
    }
    w.call(A, 10, 10);
    w.call(A, 11, 20);
    w.call(B, 12, 10);
    w.bind(10, direct(B));
    let mut facts = w.build();
    let mut req = request(A, true);
    req.depth = 1;
    let r = run(&mut facts, &req);
    let value = serde_json::to_value(&r).unwrap();
    assert_eq!(
        serde_json::from_value::<GraphResult>(value.clone()).unwrap(),
        r
    );
    assert_eq!(value["resolvedRevisionId"], REV);
    assert_eq!(value["request"]["semanticProducerId"], PRODUCER);
    assert_eq!(value["request"]["maxNodes"], 150);
    let edge = &value["edges"][1];
    assert!(edge["to"].is_null() && edge["binding"].is_null());
    assert_eq!(edge["visit"], "boundary");
    assert_eq!(edge["boundaryReason"], "missingEvidence");
    assert_eq!(value["edges"][0]["visit"], "new");
    assert_eq!(value["edges"][0]["boundaryReason"], "none");
    let depth = &value["frontier"][0];
    assert_eq!(depth["reason"], "depth");
    assert!(depth["callId"].is_null() && depth["targetId"].is_null());
    assert_eq!(depth["nextOrdinal"], 0);

    let wire = |v: &dyn erased::Wire| v.wire();
    assert_eq!(
        [Visit::New, Visit::Seen, Visit::Boundary].map(|v| wire(&v)),
        ["new", "seen", "boundary"]
    );
    assert_eq!(
        [
            NONE,
            External,
            Unresolved,
            Ambiguous,
            DISPATCH,
            Stale,
            NodeLimit,
            MissingEvidence
        ]
        .map(|v| wire(&v)),
        [
            "none",
            "external",
            "unresolved",
            "ambiguous",
            "dispatch",
            "stale",
            "nodeLimit",
            "missingEvidence"
        ]
    );
    assert_eq!(
        [
            FrontierReason::Depth,
            FrontierReason::NodeLimit,
            FrontierReason::CallLimit
        ]
        .map(|v| wire(&v)),
        ["depth", "nodeLimit", "callLimit"]
    );
    assert_eq!(
        [
            WarningCode::CoverageIncomplete,
            WarningCode::StaleEvidence,
            WarningCode::StaleTarget,
            WarningCode::BindingAmbiguous,
            WarningCode::SyntaxOnly
        ]
        .map(|v| wire(&v)),
        [
            "coverageIncomplete",
            "staleEvidence",
            "staleTarget",
            "bindingAmbiguous",
            "syntaxOnly"
        ]
    );

    let mut unknown = value.clone();
    unknown["bogus"] = true.into();
    assert!(serde_json::from_value::<GraphResult>(unknown).is_err());
    let mut unknown = value.clone();
    unknown["request"]["bogus"] = true.into();
    assert!(serde_json::from_value::<GraphResult>(unknown).is_err());
    let mut omitted = value.clone();
    omitted["request"]
        .as_object_mut()
        .unwrap()
        .remove("semanticProducerId");
    assert!(serde_json::from_value::<GraphResult>(omitted).is_err());
    let mut omitted = value;
    omitted["edges"][1].as_object_mut().unwrap().remove("to");
    assert!(serde_json::from_value::<GraphResult>(omitted).is_err());
}

mod erased {
    pub trait Wire {
        fn wire(&self) -> String;
    }
    impl<T: serde::Serialize> Wire for T {
        fn wire(&self) -> String {
            serde_json::to_value(self)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        }
    }
}

#[test]
fn high_fanout_caps_nodes_then_calls() {
    let mut w = world(true);
    w.decl(A, "src/lib.rs");
    for i in 0..600u64 {
        w.decl(1000 + i, "src/lib.rs");
        w.call(A, 5000 + i, 10 + i as usize);
        w.bind(5000 + i, direct(1000 + i));
    }
    let mut facts = w.build();
    let r = run(&mut facts, &request(A, true));
    assert_eq!(r.nodes.len(), 150);
    assert_eq!(r.edges.len(), 500);
    let new = r.edges.iter().filter(|e| e.visit == Visit::New).count();
    assert_eq!(new, 149);
    let limited = r
        .frontier
        .iter()
        .filter(|f| f.reason == FrontierReason::NodeLimit)
        .count();
    // The root takes one of the 150 nodes, so 500 - 149 calls are refused by the node cap.
    assert_eq!(limited, 500 - 149);
    let last = r.frontier.last().unwrap();
    assert_eq!(r.frontier.len(), limited + 1);
    assert_eq!(
        (last.reason, last.next_ordinal, last.omitted_calls),
        (FrontierReason::CallLimit, None, 100)
    );
    assert!(r.truncated && r.partial);
}

#[test]
fn duplicate_names_expand_only_the_bound_syntax_id() {
    let (one, two) = (B, C);
    let mut w = world(true);
    w.decl(A, "src/lib.rs");
    w.decl_named(one, "src/lib.rs", "run");
    w.decl_named(two, "src/other.rs", "run");
    w.call(A, 10, 10);
    w.bind(10, direct(two));
    let mut facts = w.build();
    let r = run(&mut facts, &request(A, true));
    assert_eq!(nodes(&r), [(A, 0), (two, 1)]);
    assert_eq!(edges(&r), [(10, A, Some(two), Visit::New, NONE)]);
}

#[test]
fn request_validation_rejects_out_of_range_without_clamping() {
    let facts_for = || graph(&[(A, 10, B)], true).build();
    let base = request(A, true);
    let cases: Vec<(GraphRequest, &str)> = vec![
        (
            GraphRequest {
                depth: 6,
                ..base.clone()
            },
            "depth",
        ),
        (
            GraphRequest {
                max_nodes: 0,
                ..base.clone()
            },
            "maxNodes",
        ),
        (
            GraphRequest {
                max_nodes: 151,
                ..base.clone()
            },
            "maxNodes",
        ),
        (
            GraphRequest {
                max_calls: 501,
                ..base.clone()
            },
            "maxCalls",
        ),
        (
            GraphRequest {
                root_syntax_id: "sid:v1:XYZ".into(),
                ..base.clone()
            },
            "rootSyntaxId",
        ),
        (
            GraphRequest {
                semantic_producer_id: Some(String::new()),
                ..base.clone()
            },
            "semanticProducerId",
        ),
        (
            GraphRequest {
                source_set_id: String::new(),
                ..base.clone()
            },
            "sourceSetId",
        ),
        (
            GraphRequest {
                revision_id: String::new(),
                ..base.clone()
            },
            "revisionId",
        ),
    ];
    for (request, field) in cases {
        let error = evaluate(&request, &mut facts_for()).unwrap().unwrap_err();
        assert_eq!(error.code, GraphErrorCode::InvalidRequest, "{field}");
        assert_eq!(error.field.as_deref(), Some(field));
    }
    // Boundary values are valid.
    for request in [
        GraphRequest {
            depth: 5,
            max_nodes: 1,
            max_calls: 0,
            ..base.clone()
        },
        GraphRequest {
            depth: 0,
            max_nodes: 150,
            max_calls: 500,
            ..base.clone()
        },
    ] {
        assert!(evaluate(&request, &mut facts_for()).unwrap().is_ok());
    }

    // Typed order: sourceSetDenied, revisionUnavailable, rootMissing, producerUnavailable.
    let code = |request: &GraphRequest| -> GraphError {
        evaluate(request, &mut facts_for()).unwrap().unwrap_err()
    };
    let foreign = GraphRequest {
        source_set_id: "other".into(),
        revision_id: "other".into(),
        root_syntax_id: sid(99),
        semantic_producer_id: Some("nope".into()),
        ..base.clone()
    };
    assert_eq!(code(&foreign).code, GraphErrorCode::SourceSetDenied);
    let foreign = GraphRequest {
        source_set_id: SS.into(),
        ..foreign
    };
    assert_eq!(code(&foreign).code, GraphErrorCode::RevisionUnavailable);
    let foreign = GraphRequest {
        revision_id: REV.into(),
        ..foreign
    };
    let root_missing = code(&foreign);
    assert_eq!(root_missing.code, GraphErrorCode::RootMissing);
    assert_eq!(root_missing.field.as_deref(), Some("rootSyntaxId"));
    let foreign = GraphRequest {
        root_syntax_id: sid(A),
        ..foreign
    };
    let unavailable = code(&foreign);
    assert_eq!(unavailable.code, GraphErrorCode::ProducerUnavailable);
    assert_eq!(unavailable.field.as_deref(), Some("semanticProducerId"));
    assert_ne!(root_missing.message, unavailable.message);
    assert_eq!(
        root_missing.message,
        "Call graph root not found in the admitted snapshot"
    );
    assert_eq!(
        unavailable.message,
        "Semantic producer not published for the admitted revision"
    );
}

/// Fails `check()` on the n-th dequeued node.
struct Cancel {
    inner: GraphFacts,
    checks: usize,
    fail_at: usize,
}

impl GraphSource for Cancel {
    fn snapshot(&self) -> &GraphSnapshot {
        self.inner.snapshot()
    }
    fn producer_languages(&mut self, p: &str) -> Result<Option<BTreeSet<String>>> {
        self.inner.producer_languages(p)
    }
    fn locate(&mut self, s: &str) -> Result<Option<DocumentKey>> {
        self.inner.locate(s)
    }
    fn document(&mut self, k: &DocumentKey) -> Result<Arc<DocumentFacts>> {
        self.inner.document(k)
    }
    fn history(&mut self, k: &DocumentKey, s: &str) -> Result<Option<Arc<HistoryFacts>>> {
        self.inner.history(k, s)
    }
    fn check(&mut self) -> Result<()> {
        self.checks += 1;
        anyhow::ensure!(self.checks != self.fail_at, "deadline_exceeded: test");
        Ok(())
    }
}

#[test]
fn cancellation_stops_without_a_result() {
    let facts = graph(&[(A, 10, B), (B, 20, C), (C, 30, D)], true).build();
    let mut source = Cancel {
        inner: facts,
        checks: 0,
        fail_at: 2,
    };
    let error = evaluate(&request(A, true), &mut source).unwrap_err();
    assert!(error.to_string().contains("deadline_exceeded"));
    assert_eq!(source.checks, 2);

    // Without a failure the check runs once per dequeued node.
    source.checks = 0;
    source.fail_at = 0;
    let r = evaluate(&request(A, true), &mut source).unwrap().unwrap();
    assert_eq!(source.checks, r.nodes.len());
}

#[test]
fn in_scope_document_without_producer_row_fails_without_result() {
    let mut w = world(true);
    w.decl(A, "src/lib.rs");
    w.call(A, 10, 10);
    w.doc("src/lib.rs").semantic = Option::None;
    let mut facts = w.build();
    let error = evaluate(&request(A, true), &mut facts).unwrap_err();
    let integrity = error.downcast_ref::<GraphIntegrity>().expect("integrity");
    assert!(integrity.0.contains("missing producer coverage tuple"));
}

#[test]
fn out_of_scope_document_is_missing_evidence_without_row() {
    let mut w = world(true);
    w.decl(A, "web/a.js");
    w.call(A, 10, 10);
    w.call(A, 11, 20);
    let mut facts = w.build();
    let r = run(&mut facts, &request(A, true));
    assert!(
        r.edges
            .iter()
            .all(|e| e.boundary_reason == MissingEvidence && e.binding.is_none())
    );
    assert_eq!(r.edges.len(), 2);
    assert_eq!(r.coverage.len(), 1);
    assert_eq!(r.coverage[0].producer_id, "native");
    assert!(r.warnings.is_empty());
    assert!(r.partial);
}

#[test]
fn binding_with_anchor_hash_unlike_provenance_hash_is_still_classified_by_freshness() {
    for (freshness, expect_pid) in [(Freshness::Stale, true), (Freshness::PossiblyStale, false)] {
        let mut w = world(true);
        w.decl(A, "src/lib.rs");
        w.decl(B, "src/lib.rs");
        w.call(A, 10, 10);
        w.bind(
            10,
            Bind {
                freshness,
                prov: Some("pe".into()),
                ..direct(B)
            },
        );
        for binding in &mut w.doc("src/lib.rs").semantic.as_mut().unwrap().call_bindings {
            binding.join.anchor.content_hash = "current".into();
        }
        let mut facts = w.build();
        let req = request(A, true);
        let r = run(&mut facts, &req);
        assert_eq!(edges(&r), [(10, A, None, Visit::Boundary, Stale)]);
        assert!(r.edges[0].binding.is_some());
        assert!(r.partial);
        assert_eq!(
            keys(&r.warnings),
            [(WarningCode::StaleEvidence, expect_pid.then(|| "pe".into()))]
        );
    }
}

#[test]
fn two_bindings_for_one_call_are_typed_integrity_corruption() {
    let mut w = world(true);
    w.decl(A, "src/lib.rs");
    w.decl(B, "src/lib.rs");
    w.call(A, 10, 10);
    w.bind(
        10,
        Bind {
            prov: Some("p1".into()),
            ..direct(B)
        },
    );
    w.bind(
        10,
        Bind {
            prov: Some("p2".into()),
            ..direct(B)
        },
    );
    let mut facts = w.build();
    let error =
        evaluate(&request(A, true), &mut facts).expect_err("no answer for ambiguous binding rows");
    assert!(
        error.downcast_ref::<GraphIntegrity>().is_some(),
        "{error:#}"
    );
}

#[test]
fn trimmed_groups_exact_targets_and_proofs_not_occurrence_anchors() {
    let mut w = world(true);
    w.decl(A, "src/lib.rs");
    w.decl(B, "src/lib.rs");
    for n in 1..=501 {
        w.call(A, n, n as usize);
        w.bind(
            n,
            Bind {
                prov: Some("shared".into()),
                ..direct(B)
            },
        );
    }
    w.call(A, 502, 502);
    w.bind(
        502,
        Bind {
            prov: Some("distinct".into()),
            ..direct(B)
        },
    );
    let mut facts = w.build();
    let req = GraphRequest {
        max_calls: 2,
        ..request(A, true)
    };
    let r = run(&mut facts, &req);
    assert_eq!(r.edges.len(), 2);
    assert_eq!(r.edges[0].call_site_count, 501);
    assert_eq!(r.edges[1].call_site_count, 1);
    assert_eq!(r.edges[0].call.id, occ(1));
    assert!(!r.partial && !r.truncated);
}

#[test]
fn trimmed_external_filter_keeps_test_root_and_unknown_calls() {
    let mut w = world(true);
    w.decl(A, "src/lib.rs");
    w.doc("src/lib.rs")
        .declarations
        .get_mut(&sid(A))
        .unwrap()
        .role = native::DeclarationRole::Test;
    w.call(A, 10, 10);
    w.bind(
        10,
        Bind {
            target: Tgt::External,
            resolution: Resolution::External,
            ..direct(B)
        },
    );
    let mut facts = w.build();
    let req = GraphRequest {
        include_external: false,
        include_tests: false,
        ..request(A, true)
    };
    let r = run(&mut facts, &req);
    assert_eq!(r.nodes.len(), 1);
    assert_eq!(r.omitted.filtered, 1);
    assert_eq!(r.hints, ["includeExternal"]);
    assert!(r.partial && !r.truncated);
    assert!(r.warnings.is_empty());
    assert_eq!(r.frontier[0].call_id, None);
    assert_eq!(r.frontier[0].target_id, None);
    assert_eq!(r.frontier[0].next_ordinal, None);

    let mut w = world(true);
    w.decl(A, "src/lib.rs");
    w.call(A, 10, 10);
    let mut facts = w.build();
    let r = run(&mut facts, &req);
    assert_eq!(r.edges.len(), 1);
    assert_eq!(r.omitted.filtered, 0);
}

#[test]
fn trimmed_late_internal_priority_keeps_closure_cycles_and_exact_omissions() {
    let mut w = world(true);
    for n in [A, B, C] {
        w.decl(n, "src/lib.rs");
    }
    for n in 1..=501 {
        w.call(A, n, n as usize);
        w.bind(
            n,
            Bind {
                target: Tgt::External,
                resolution: Resolution::External,
                ..direct(B)
            },
        );
    }
    w.call(A, 502, 502);
    w.bind(502, direct(B));
    w.call(B, 503, 503);
    w.bind(503, direct(C));
    w.call(C, 504, 504);
    w.bind(504, direct(A));
    let mut facts = w.build();
    let req = GraphRequest {
        max_calls: 3,
        depth: 5,
        ..request(A, true)
    };
    let r = run(&mut facts, &req);
    assert_eq!(
        r.edges.iter().map(|e| n_of(&e.call.id)).collect::<Vec<_>>(),
        [502, 503, 504]
    );
    assert_eq!(r.edges[2].visit, Visit::Seen);
    assert_eq!(r.omitted.call_limit, 501);
    let answer = graph_answer(req, r, facts);
    let prefix = answer.prefix(1).unwrap();
    assert_eq!(nodes(&prefix), [(A, 0), (B, 1)]);
    assert_eq!(prefix.omitted.call_limit, 501);
    assert_eq!(prefix.omitted.response_bytes, 1);
    assert_eq!(prefix.request.max_calls, 3);
    assert_eq!(
        prefix.omitted.response_bytes,
        prefix
            .frontier
            .iter()
            .filter(|f| f.reason == FrontierReason::ResponseBytes)
            .map(|f| f.omitted_calls)
            .sum::<u64>()
    );
    assert_eq!(
        prefix.warnings,
        warnings_v1(
            &prefix.request,
            &prefix.coverage,
            &prefix.provenance,
            &prefix.edges
        )
    );
}

fn graph_answer(req: GraphRequest, result: GraphResult, facts: GraphFacts) -> GraphAnswer {
    GraphAnswer {
        basis: IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 1,
        },
        requested: req,
        outcome: Ok(result),
        facts,
    }
}

#[test]
fn trimmed_depth_precedes_filters_and_call_cap() {
    let mut w = graph(&[(A, 10, B)], true);
    w.doc("src/lib.rs")
        .declarations
        .get_mut(&sid(B))
        .unwrap()
        .role = native::DeclarationRole::Test;
    let mut facts = w.build();
    let req = GraphRequest {
        depth: 0,
        max_calls: 0,
        include_tests: false,
        ..request(A, true)
    };
    let r = run(&mut facts, &req);
    assert_eq!(r.omitted.depth, 1);
    assert_eq!(r.omitted.filtered, 0);
    assert_eq!(r.omitted.call_limit, 0);
    assert_eq!(r.frontier[0].next_ordinal, Some(0));
}

#[test]
fn trimmed_distinct_binding_outcome_and_freshness_do_not_group() {
    let mut w = graph(&[(A, 10, B), (A, 11, B), (A, 12, B)], true);
    let semantic = w.doc("src/lib.rs").semantic.as_mut().unwrap();
    semantic.call_bindings[1].dispatch = Dispatch::Virtual;
    semantic.provenance.get_mut("pv:12").unwrap().freshness = Freshness::Stale;
    let mut facts = w.build();
    let r = run(&mut facts, &request(A, true));
    assert_eq!(r.edges.len(), 3);
    assert!(r.edges.iter().all(|e| e.call_site_count == 1));
}

#[test]
fn trimmed_missing_or_mismatched_internal_locations_do_not_group_identical_proofs() {
    for location in [None, Some(dkey("src/other.rs"))] {
        let mut w = world(true);
        w.decl(A, "src/lib.rs");
        w.decl(B, "src/target.rs");
        for n in [10, 11] {
            w.call(A, n, n as usize);
            w.bind(
                n,
                Bind {
                    prov: Some("shared".into()),
                    ..direct(B)
                },
            );
        }
        let mut facts = w.build();
        facts.locations.insert(sid(B), location);
        let r = run(&mut facts, &request(A, true));
        assert_eq!(r.edges.len(), 2);
        assert!(r.edges.iter().all(|edge| {
            edge.call_site_count == 1
                && edge.visit == Visit::Boundary
                && edge.boundary_reason == Stale
        }));
    }
}

#[test]
fn trimmed_located_stale_targets_group_when_proof_and_disposition_match() {
    let mut w = world(true);
    w.decl(A, "src/lib.rs");
    w.decl(B, "src/target.rs");
    for n in [10, 11] {
        w.call(A, n, n as usize);
        w.bind(
            n,
            Bind {
                prov: Some("shared".into()),
                freshness: Freshness::Stale,
                ..direct(B)
            },
        );
    }
    let mut facts = w.build();
    let r = run(&mut facts, &request(A, true));
    assert_eq!(r.edges.len(), 1);
    assert_eq!(r.edges[0].call_site_count, 2);
    assert_eq!(r.edges[0].boundary_reason, Stale);
    assert_eq!(r.edges[0].visit, Visit::Boundary);
}

#[test]
fn trimmed_filter_hints_are_captured_once_and_follow_prefix_owners() {
    let mut w = world(true);
    for (n, path) in [(A, "src/root.rs"), (B, "src/owner.rs"), (C, "src/test.rs")] {
        w.decl(n, path);
    }
    w.doc("src/test.rs")
        .declarations
        .get_mut(&sid(C))
        .unwrap()
        .role = native::DeclarationRole::Test;
    w.call(A, 10, 10);
    w.bind(
        10,
        Bind {
            target: Tgt::External,
            resolution: Resolution::External,
            ..direct(B)
        },
    );
    w.call(A, 11, 20);
    w.bind(11, direct(B));
    w.call(B, 20, 10);
    w.bind(20, direct(C));
    let mut facts = w.build();
    let req = GraphRequest {
        include_external: false,
        include_tests: false,
        ..request(A, true)
    };
    let mut recorder = Recorder::new(&mut facts);
    let result = evaluate(&req, &mut recorder).unwrap().unwrap();
    assert_eq!(recorder.work_checks, 3, "each call is classified only once");
    assert_eq!(result.hints, ["includeExternal", "includeTests"]);
    assert!(
        serde_json::to_value(&result)
            .unwrap()
            .get("filteredHints")
            .is_none()
    );
    let mut memo = recorder.memo;
    for doc in memo.documents.values_mut() {
        Arc::make_mut(doc).calls.clear();
    }
    let answer = graph_answer(req, result, memo);
    let prefix = answer.prefix(0).unwrap();
    assert_eq!(nodes(&prefix), [(A, 0)]);
    assert_eq!(prefix.hints, ["includeExternal", "rootSyntaxId"]);
    assert_eq!(prefix.omitted.filtered, 1);
    assert_eq!(prefix.filtered_hints.len(), 1);
    assert!(prefix.filtered_hints.contains_key(&sid(A)));
}

#[test]
fn trimmed_test_root_keeps_production_and_unknown_calls_but_filters_test_callees() {
    let mut w = world(true);
    for n in [A, B, C] {
        w.decl(n, "src/lib.rs");
    }
    for n in [A, C] {
        w.doc("src/lib.rs")
            .declarations
            .get_mut(&sid(n))
            .unwrap()
            .role = native::DeclarationRole::Test;
    }
    w.call(A, 10, 10);
    w.bind(10, direct(B));
    w.call(A, 11, 20);
    w.bind(11, direct(C));
    w.call(A, 12, 30);
    let mut facts = w.build();
    let req = GraphRequest {
        include_tests: false,
        ..request(A, true)
    };
    let r = run(&mut facts, &req);
    assert_eq!(nodes(&r), [(A, 0), (B, 1)]);
    assert_eq!(
        edges(&r),
        [
            (10, A, Some(B), Visit::New, NONE),
            (12, A, None, Visit::Boundary, MissingEvidence),
        ]
    );
    assert_eq!(r.omitted.filtered, 1);
    assert_eq!(r.hints, ["includeTests"]);
    assert!(r.partial && !r.truncated);
    let included = run(
        &mut facts,
        &GraphRequest {
            include_tests: true,
            ..req
        },
    );
    assert_eq!(included.edges.len(), 3);
    assert_eq!(included.omitted.filtered, 0);
}

#[test]
fn trimmed_nonroot_test_owner_remains_excluded_and_is_not_expanded() {
    let mut w = graph(&[(A, 10, B), (B, 20, C)], true);
    w.doc("src/lib.rs")
        .declarations
        .get_mut(&sid(B))
        .unwrap()
        .role = native::DeclarationRole::Test;
    let mut facts = w.build();
    let req = GraphRequest {
        include_tests: false,
        ..request(A, true)
    };
    let r = run(&mut facts, &req);
    assert_eq!(nodes(&r), [(A, 0)]);
    assert!(r.edges.is_empty());
    assert_eq!(
        r.omitted.filtered, 1,
        "unexpanded descendants are not counted"
    );
    assert_eq!(r.hints, ["includeTests"]);
    let included = run(
        &mut facts,
        &GraphRequest {
            include_tests: true,
            ..req
        },
    );
    assert_eq!(nodes(&included), [(A, 0), (B, 1), (C, 2)]);
    assert_eq!(included.edges.len(), 2);
    assert_eq!(included.omitted.filtered, 0);
}

#[test]
fn trimmed_stale_external_bindings_are_still_filtered_as_external() {
    let mut w = world(true);
    w.decl(A, "src/lib.rs");
    w.call(A, 10, 10);
    w.bind(
        10,
        Bind {
            target: Tgt::External,
            resolution: Resolution::External,
            freshness: Freshness::Stale,
            ..direct(B)
        },
    );
    let mut facts = w.build();
    let req = GraphRequest {
        include_external: false,
        ..request(A, true)
    };
    let r = run(&mut facts, &req);
    assert!(r.edges.is_empty());
    assert_eq!(r.omitted.filtered, 1);
    assert_eq!(r.hints, ["includeExternal"]);
    assert!(
        r.warnings.is_empty(),
        "filtered proof does not produce warnings"
    );
    let included = run(
        &mut facts,
        &GraphRequest {
            include_external: true,
            ..req
        },
    );
    assert_eq!(edges(&included), [(10, A, None, Visit::Boundary, Stale)]);
    assert_eq!(included.omitted.filtered, 0);
    assert_eq!(
        keys(&included.warnings),
        [(WarningCode::StaleEvidence, Some("pv:10".into()))]
    );
}

/// #466: the importer publishes contradictory evidence at one call site as one ambiguous row per
/// provenance, all sharing `call_id` and each carrying the full candidate union.
fn contradictory_call(rows: &[(&str, Bind)]) -> World {
    let mut w = world(true);
    for n in [A, B, 90, 91, 92] {
        w.decl(n, "src/lib.rs");
    }
    w.call(A, 10, 10);
    for (prov, bind) in rows {
        w.bind(
            10,
            Bind {
                prov: Some((*prov).into()),
                ..bind.clone()
            },
        );
    }
    w
}

fn ambiguous() -> Bind {
    Bind {
        target: Tgt::Nothing,
        resolution: Resolution::Ambiguous,
        stale_target: None,
        ..direct(0)
    }
}

#[test]
fn ambiguous_rows_sharing_a_call_are_one_edge_with_the_full_union() {
    let mut w = contradictory_call(&[
        ("pC", ambiguous()),
        ("pA", ambiguous()),
        ("pB", ambiguous()),
    ]);
    let union: Vec<_> = [90, 91, 92]
        .into_iter()
        .map(|n| Target::Internal {
            syntax_id: sid(n),
            document: dkey("src/lib.rs"),
            revision_id: REV.into(),
        })
        .collect();
    for binding in &mut w.doc("src/lib.rs").semantic.as_mut().unwrap().call_bindings {
        binding.candidates = union.clone();
    }
    let mut facts = w.build();
    let req = request(A, true);
    let r = run(&mut facts, &req);
    assert_eq!(edges(&r), vec![(10, A, None, Visit::Boundary, Ambiguous)]);
    let binding = r.edges[0].binding.as_ref().expect("ambiguous binding");
    assert_eq!(binding.provenance_id, "pA", "lowest provenance represents");
    assert_eq!(binding.resolution, Resolution::Ambiguous);
    assert_eq!(binding.candidates, union);
    let proofs: Vec<_> = r
        .provenance
        .iter()
        .filter(|p| p.producer_id == PRODUCER)
        .map(|p| p.id.as_str())
        .collect();
    assert_eq!(proofs, vec!["pA", "pB", "pC"]);
    assert!(r.partial);
    assert_keys(&req, &r, &[(WarningCode::BindingAmbiguous, Some("pA"))]);
}

#[test]
fn non_ambiguous_multi_match_for_one_call_stays_integrity() {
    let external = Bind {
        target: Tgt::External,
        resolution: Resolution::External,
        stale_target: None,
        ..direct(0)
    };
    for (case, rows) in [
        (
            "duplicate resolved",
            vec![("pA", direct(B)), ("pB", direct(B))],
        ),
        (
            "ambiguous+resolved",
            vec![("pA", ambiguous()), ("pB", direct(B))],
        ),
        (
            "ambiguous+external",
            vec![("pA", ambiguous()), ("pB", external.clone())],
        ),
    ] {
        let mut facts = contradictory_call(&rows).build();
        let error = evaluate(&request(A, true), &mut facts).unwrap_err();
        let integrity = error
            .downcast_ref::<GraphIntegrity>()
            .unwrap_or_else(|| panic!("{case}: {error:#}"));
        assert!(
            integrity
                .0
                .contains("multiple call bindings match one call"),
            "{case}"
        );
    }
}
