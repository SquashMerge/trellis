//! Pure test model for the uncommitted #620 addendum snapshot. No product adapter.
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

type Doc = u8;
type Producer = u8;
// Abstract independent selection = producer and scope; scope membership is a fixture policy.
type Selection = (Producer, u8);
type Key = (Doc, Producer);
type Pin = u64;
type Token = u64;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NativeRevision(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Document {
    identity: u8,
    vid: u8,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Capture {
    docs: BTreeMap<Doc, Document>,
    // Explicit producer/document membership; absence means not selected.
    required: BTreeSet<Key>,
    // Fixture assumption: these producer/scope contexts are native-captured.
    // External semantic-only context identity is not separately modeled.
    contexts: BTreeMap<(Producer, u8), [u8; 3]>,
    // Initial synthetic fixture table; it is not the durable selected input.
    output: BTreeMap<Key, Record>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct SemanticInputs {
    identities: BTreeMap<Selection, Pin>,
    rows: BTreeMap<Key, Record>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Freshness {
    Fresh,
    PossiblyStale,
    Stale,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    Resolved,
    Unknown,
    Unresolved,
    Ambiguous,
    External,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Symbol,
    DeclCandidate,
    RefDeclared,
    RefCandidate,
    CallDeclared,
    CallCandidate,
    Dispatch,
    TypeFrom,
    TypeTo,
    Lookup,
    Consumes,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Edge {
    kind: Kind,
    target: Key,
    mapped: bool,
    context: Option<(Producer, u8)>,
    external: bool,
    consumed_basis: Option<Pin>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    Evidence(Claim),
    Empty,
    NotRequested,
    Failed,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    outcome: Outcome,
    freshness: Freshness,
    edges: Vec<Edge>,
    complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PublicRecord {
    outcome: Outcome,
    freshness: Freshness,
    complete: bool,
}
impl From<&Record> for PublicRecord {
    fn from(r: &Record) -> Self {
        Self {
            outcome: r.outcome.clone(),
            freshness: r.freshness,
            complete: r.complete,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Semantic {
    capture: Capture,
    rows: BTreeMap<Key, Record>,
    input_ids: BTreeMap<Selection, Pin>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct View {
    native: Pin,
    revision: NativeRevision,
    basis: BTreeMap<Selection, Pin>,
    input_ids: BTreeMap<Selection, Pin>,
    capture: Capture,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Labels {
    pin: Pin,
    native: Pin,
    basis: Pin,
    basis_native: Pin,
    native_revision: NativeRevision,
    basis_native_revision: NativeRevision,
    older: bool,
    historical: Freshness,
    effective: Freshness,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    edge: Edge,
    labels: Labels,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Read {
    Pending {
        pin: Pin,
        native: Pin,
        basis: Pin,
        basis_native: Pin,
        native_revision: NativeRevision,
        basis_native_revision: NativeRevision,
    },
    Corrupt,
    Recorded {
        labels: Labels,
        record: PublicRecord,
        targets: Vec<Target>,
    },
    UnresolvedDependency,
    NotCovered,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Hop {
    Row(Key, Read),
    Boundary(Key, Read),
    AuthorityBoundary(Key, Target, Pin),
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Endpoint {
    Internal(Target, Read),
    External(Target),
    UnresolvedAuthority(Target, Read),
    PendingBoundary(Target, Read),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueryError {
    Pending,
    Corrupt,
    Unresolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    PendingAbsence,
    IgnoreCandidate,
    UnlabelNested,
    CrossPending,
    Rechain,
    StaleCas,
    OldCleanup,
    GlobalBasis,
    IgnoreContext,
    MissingAsPending,
    OldAbsence,
    CopyPromotion,
    ExternalCollision,
    NoopInvalidation,
    ConsumeLatest,
    IgnoreCoverage,
    OverwriteImport,
    RetargetAuthority,
    PartialCoverage,
    CopyOldInputs,
    ConsumeStaleFresh,
    HidePending,
    PublishIncomplete,
    UnselectedIsCorrupt,
}
#[derive(Clone, Debug)]
struct Stage {
    token: Token,
    target: Pin,
    revision: NativeRevision,
    target_id: u64,
    head: Pin,
    marker: u64,
    selected: Capture,
    semantic_inputs: SemanticInputs,
    selections: BTreeSet<Selection>,
    remaining: u8,
    accumulated: BTreeMap<Key, Record>,
}
#[derive(Clone, Debug)]
struct TargetState {
    id: u64,
    native: Pin,
    revision: NativeRevision,
    selected: Capture,
    semantic_inputs: SemanticInputs,
    outstanding: BTreeSet<Selection>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Commit {
    Published,
    Conflict,
    Missing,
    Incomplete,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recheck {
    Fulfilled,
    Outstanding,
    Superseded,
}
#[derive(Clone, Debug)]
struct Model {
    working: Capture,
    native: Pin,
    revision: NativeRevision,
    head: Pin,
    semantic: Pin,
    bases: BTreeMap<Selection, Pin>,
    semantic_inputs: SemanticInputs,
    target: TargetState,
    marker: u64,
    next: u64,
    views: BTreeMap<Pin, View>,
    semantics: BTreeMap<Pin, Semantic>,
    stages: BTreeMap<Token, Stage>,
    active: Option<Token>,
    fault: Fault,
}

fn recompute(c: &Capture) -> BTreeMap<Key, Record> {
    // Synthetic FULL is driven by the explicitly captured producer output table.
    c.output
        .iter()
        .filter(|(key, _)| c.required.contains(key) && c.docs.contains_key(&key.0))
        .map(|(k, v)| (*k, v.clone()))
        .collect()
}
fn reference_full(c: &Capture, inputs: &SemanticInputs) -> BTreeMap<Key, Record> {
    // Deliberately separate enumeration from the iterator-based stage computation.
    let mut result = BTreeMap::new();
    for &doc in c.docs.keys() {
        for producer in 0..2 {
            if c.required.contains(&(doc, producer))
                && let Some(record) = inputs.rows.get(&(doc, producer))
            {
                result.insert((doc, producer), record.clone());
            }
        }
    }
    result
}
fn reference_selected(
    c: &Capture,
    inputs: &SemanticInputs,
    selected: &BTreeSet<Selection>,
) -> BTreeMap<Key, Record> {
    reference_full(c, inputs)
        .into_iter()
        .filter(|((d, p), _)| selected.contains(&(*p, *d % 2)))
        .collect()
}
impl Model {
    fn new(c: Capture, fault: Fault) -> Self {
        let rows = recompute(&c);
        let inputs = SemanticInputs {
            identities: BTreeMap::from([((0, 0), 1), ((0, 1), 1), ((1, 0), 1), ((1, 1), 1)]),
            rows: rows.clone(),
        };
        let view = View {
            native: 1,
            revision: NativeRevision(1),
            basis: BTreeMap::from([((0, 0), 1), ((0, 1), 1), ((1, 0), 1), ((1, 1), 1)]),
            input_ids: inputs.identities.clone(),
            capture: c.clone(),
        };
        Self {
            working: c.clone(),
            native: 1,
            revision: NativeRevision(1),
            head: 1,
            semantic: 1,
            bases: BTreeMap::from([((0, 0), 1), ((0, 1), 1), ((1, 0), 1), ((1, 1), 1)]),
            semantic_inputs: inputs.clone(),
            target: TargetState {
                id: 1,
                native: 1,
                revision: NativeRevision(1),
                selected: c.clone(),
                semantic_inputs: inputs.clone(),
                outstanding: BTreeSet::from([(0, 0), (0, 1), (1, 0), (1, 1)]),
            },
            marker: 0,
            next: 2,
            views: BTreeMap::from([(1, view)]),
            semantics: BTreeMap::from([(
                1,
                Semantic {
                    capture: c,
                    rows,
                    input_ids: inputs.identities.clone(),
                },
            )]),
            stages: BTreeMap::new(),
            active: None,
            fault,
        }
    }
    fn id(&mut self) -> u64 {
        let n = self.next;
        self.next += 1;
        n
    }
    fn publish_native(&mut self) -> Pin {
        let published = &self.views[&self.native].capture;
        if self.fault != Fault::NoopInvalidation
            && self.working.docs == published.docs
            && self.working.contexts == published.contexts
            && self.working.required == published.required
        {
            return self.head;
        }
        let added: Vec<Key> = self
            .working
            .required
            .difference(&published.required)
            .copied()
            .collect();
        let previous = self.head;
        let p = self.id();
        // Additions have explicitly generated producer results, never implicit
        // success. A missing selected result stays missing and S rejects it.
        for key in added {
            if let Some(row) = self.working.output.get(&key) {
                self.semantic_inputs.rows.insert(key, row.clone());
                self.semantic_inputs
                    .identities
                    .insert((key.1, key.0 % 2), p);
            }
        }
        self.revision = NativeRevision(self.revision.0 + 1);
        let basis = if self.fault == Fault::Rechain && previous != self.semantic {
            // Fault: treat the previous native overlay as the next semantic
            // capture basis, rather than recomputing from validated P.
            let overlay = self.views[&previous].clone();
            let rows = self.semantics[&overlay.basis[&(0, 0)]].rows.clone();
            self.semantics.insert(
                previous,
                Semantic {
                    capture: overlay.capture,
                    rows,
                    input_ids: self.semantic_inputs.identities.clone(),
                },
            );
            BTreeMap::from([
                ((0, 0), previous),
                ((0, 1), previous),
                ((1, 0), previous),
                ((1, 1), previous),
            ])
        } else {
            self.bases.clone()
        };
        self.views.insert(
            p,
            View {
                native: p,
                revision: self.revision,
                basis,
                input_ids: self.semantic_inputs.identities.clone(),
                capture: self.working.clone(),
            },
        );
        self.native = p;
        self.head = p;
        self.marker += 1;
        self.target = TargetState {
            id: p,
            native: p,
            revision: self.revision,
            selected: self.working.clone(),
            semantic_inputs: if self.fault == Fault::OverwriteImport {
                SemanticInputs {
                    identities: self.semantic_inputs.identities.clone(),
                    rows: recompute(&self.working),
                }
            } else {
                self.semantic_inputs.clone()
            },
            outstanding: BTreeSet::from([(0, 0), (0, 1), (1, 0), (1, 1)]),
        };
        p
    }
    fn start(&mut self) -> Option<Token> {
        if self.target.outstanding.is_empty()
            || self.target.native != self.native
            || self.target.revision != self.revision
            || self.target.selected != self.views[&self.native].capture
            || (self.fault != Fault::OverwriteImport
                && self.target.semantic_inputs != self.semantic_inputs)
        {
            return None;
        }
        let token = self.id();
        self.stages.insert(
            token,
            Stage {
                token,
                target: self.native,
                revision: self.revision,
                target_id: self.target.id,
                head: self.head,
                marker: self.marker,
                selected: self.views[&self.native].capture.clone(),
                semantic_inputs: self.target.semantic_inputs.clone(),
                selections: self.target.outstanding.clone(),
                remaining: 2,
                accumulated: BTreeMap::new(),
            },
        );
        self.active = Some(token);
        Some(token)
    }
    fn cancel(&mut self, token: Token) {
        self.stages.remove(&token);
        if self.active == Some(token) || self.fault == Fault::OldCleanup {
            self.active = None;
        }
    }
    fn unit(&mut self, token: Token) {
        if let Some(stage) = self.stages.get_mut(&token)
            && stage.remaining > 0
        {
            let producer = 2 - stage.remaining;
            for (&key, row) in &stage.semantic_inputs.rows {
                if key.1 == producer
                    && stage.selections.contains(&(key.1, key.0 % 2))
                    && stage.selected.docs.contains_key(&key.0)
                    && stage.selected.required.contains(&key)
                {
                    stage.accumulated.insert(key, row.clone());
                }
            }
            stage.remaining -= 1;
        }
    }
    fn finish(&mut self, token: Token) {
        self.unit(token);
        self.unit(token);
    }
    fn commit(&mut self, token: Token) -> Commit {
        let Some(stage) = self.stages.remove(&token) else {
            return Commit::Missing;
        };
        let owns = self.active == Some(token);
        if self.active == Some(token) {
            self.active = None;
        }
        let exact = owns
            && stage.remaining == 0
            && stage.token == token
            && stage.target == self.native
            && stage.revision == self.revision
            && self.target.revision == self.revision
            && stage.target_id == self.target.id
            && stage.head == self.head
            && stage.marker == self.marker
            && stage.selected == self.views[&self.native].capture;
        let exact = exact
            && !stage.selections.is_empty()
            && stage.selections == self.target.outstanding
            && stage.selected == self.target.selected
            && stage.semantic_inputs == self.target.semantic_inputs
            && (self.fault == Fault::OverwriteImport
                || stage.semantic_inputs == self.semantic_inputs);
        if !exact && self.fault != Fault::StaleCas {
            return Commit::Conflict;
        }
        if self.fault != Fault::PublishIncomplete
            && stage.selected.required.iter().any(|&(d, p)| {
                stage.selected.docs.contains_key(&d)
                    && stage.selections.contains(&(p, d % 2))
                    && !stage.accumulated.contains_key(&(d, p))
            })
        {
            return Commit::Incomplete;
        }
        self.publish_scopes_inner(
            stage.target,
            stage.selected.clone(),
            stage.accumulated,
            &stage.selections.iter().copied().collect::<Vec<_>>(),
            false,
        );
        Commit::Published
    }
    fn publish_semantic(
        &mut self,
        native: Pin,
        capture: Capture,
        rows: BTreeMap<Key, Record>,
    ) -> Pin {
        self.publish_selected(native, capture, rows, &[0, 1])
    }
    fn publish_selected(
        &mut self,
        native: Pin,
        capture: Capture,
        rows: BTreeMap<Key, Record>,
        selected: &[Producer],
    ) -> Pin {
        let scopes: Vec<_> = selected.iter().flat_map(|&p| [(p, 0), (p, 1)]).collect();
        self.publish_scopes(native, capture, rows, &scopes)
    }
    fn publish_scopes(
        &mut self,
        native: Pin,
        capture: Capture,
        rows: BTreeMap<Key, Record>,
        selected: &[Selection],
    ) -> Pin {
        self.publish_scopes_inner(native, capture, rows, selected, true)
    }
    fn publish_scopes_inner(
        &mut self,
        native: Pin,
        capture: Capture,
        rows: BTreeMap<Key, Record>,
        selected: &[Selection],
        explicit_import: bool,
    ) -> Pin {
        if explicit_import
            && capture.required.iter().any(|&(d, p)| {
                capture.docs.contains_key(&d)
                    && selected.contains(&(p, d % 2))
                    && !rows.contains_key(&(d, p))
            })
        {
            return self.head;
        }
        let p = self.id();
        if explicit_import {
            let selected_set: BTreeSet<_> = selected.iter().copied().collect();
            let previous = self.semantic_inputs.rows.clone();
            self.semantic_inputs
                .rows
                .retain(|&(d, p), _| !selected_set.contains(&(p, d % 2)));
            for (&(d, producer), row) in &rows {
                if selected_set.contains(&(producer, d % 2)) {
                    self.semantic_inputs.rows.insert((d, producer), row.clone());
                }
            }
            for &selection in selected {
                let previous_rows: BTreeMap<_, _> = previous
                    .iter()
                    .filter(|&(&(d, producer), _)| (producer, d % 2) == selection)
                    .collect();
                let new_rows: BTreeMap<_, _> = self
                    .semantic_inputs
                    .rows
                    .iter()
                    .filter(|&(&(d, producer), _)| (producer, d % 2) == selection)
                    .collect();
                if previous_rows != new_rows {
                    self.semantic_inputs.identities.insert(selection, p);
                }
            }
        }
        let mut basis = self.views[&self.head].basis.clone();
        for selection in selected {
            basis.insert(*selection, p);
            self.bases.insert(*selection, p);
        }
        if self.fault == Fault::GlobalBasis {
            for producer in 0..2 {
                for scope in 0..2 {
                    basis.insert((producer, scope), p);
                    self.bases.insert((producer, scope), p);
                }
            }
        }
        if native == self.target.native && capture == self.target.selected {
            if explicit_import {
                self.target.semantic_inputs = self.semantic_inputs.clone();
            }
            for selection in selected {
                self.target.outstanding.remove(selection);
            }
        }
        self.views.insert(
            p,
            View {
                native,
                revision: self.views[&native].revision,
                basis,
                input_ids: self.semantic_inputs.identities.clone(),
                capture: capture.clone(),
            },
        );
        self.semantics.insert(
            p,
            Semantic {
                capture,
                rows,
                input_ids: self.semantic_inputs.identities.clone(),
            },
        );
        self.head = p;
        self.semantic = p;
        self.marker += 1;
        p
    }
    fn import(&mut self, complete: bool) -> Pin {
        let c = self.views[&self.native].capture.clone();
        let mut rows = self.semantic_inputs.rows.clone();
        if !complete {
            rows.insert(
                (0, 0),
                Record {
                    outcome: Outcome::NotRequested,
                    freshness: Freshness::Fresh,
                    edges: vec![],
                    complete: false,
                },
            );
        }
        if complete {
            self.publish_semantic(self.native, c, rows)
        } else {
            self.publish_selected(self.native, c, rows, &[1])
        }
    }
    fn copy_same_native(&mut self, source: Pin) -> Option<Pin> {
        let view = self.views.get(&source)?;
        if view.native != self.native {
            return None;
        }
        let basis = view.basis[&(0, 0)];
        if view.basis.values().any(|&p| p != basis) {
            return None;
        }
        if self.fault != Fault::CopyPromotion && self.views[&basis].revision != self.revision {
            return None;
        }
        let semantic = self.semantics.get(&basis)?.clone();
        if self.fault != Fault::CopyOldInputs
            && semantic.input_ids != self.semantic_inputs.identities
        {
            return None;
        }
        Some(self.publish_scopes_inner(
            self.native,
            semantic.capture,
            semantic.rows,
            &[(0, 0), (0, 1), (1, 0), (1, 1)],
            false,
        ))
    }
    fn recheck(&self, stage: &Stage) -> Recheck {
        if self.native != stage.target || self.target.id != stage.target_id {
            return Recheck::Superseded;
        }
        if self.target.selected == stage.selected && self.target.outstanding.is_empty() {
            Recheck::Fulfilled
        } else {
            Recheck::Outstanding
        }
    }
    fn base_for(&self, view: &View, key: Key) -> Option<&Semantic> {
        self.semantics.get(view.basis.get(&(key.1, key.0 % 2))?)
    }
    fn read(&self, pin: Pin, key: Key) -> Read {
        let Some(view) = self.views.get(&pin) else {
            return Read::Corrupt;
        };
        if view.capture.docs.contains_key(&key.0) && !view.capture.required.contains(&key) {
            return Read::NotCovered;
        }
        let Some(base) = self.base_for(view, key) else {
            return Read::Corrupt;
        };
        let basis = view.basis[&(key.1, key.0 % 2)];
        let is_eligible = match eligible(self, view, key, None, &mut BTreeSet::new()) {
            Ok(value) => value,
            Err(QueryError::Corrupt) => return Read::Corrupt,
            Err(_) => return Read::UnresolvedDependency,
        };
        if !is_eligible {
            if self.fault == Fault::PendingAbsence {
                return Read::Recorded {
                    labels: Labels {
                        pin,
                        native: view.native,
                        basis,
                        basis_native: self.views[&basis].native,
                        native_revision: view.revision,
                        basis_native_revision: self.views[&basis].revision,
                        older: false,
                        historical: Freshness::Fresh,
                        effective: Freshness::Fresh,
                    },
                    targets: vec![],
                    record: PublicRecord {
                        outcome: Outcome::Empty,
                        freshness: Freshness::Fresh,
                        complete: true,
                    },
                };
            }
            return Read::Pending {
                pin,
                native: view.native,
                basis,
                basis_native: self.views[&basis].native,
                native_revision: view.revision,
                basis_native_revision: self.views[&basis].revision,
            };
        }
        let Some(record) = base.rows.get(&key) else {
            return Read::Corrupt;
        };
        let older = view.revision != self.views[&basis].revision;
        let effective = match (older, record.freshness) {
            (true, Freshness::Fresh) => Freshness::PossiblyStale,
            (_, f) => f,
        };
        let labels = Labels {
            pin,
            native: view.native,
            basis,
            basis_native: self.views[&basis].native,
            native_revision: view.revision,
            basis_native_revision: self.views[&basis].revision,
            older,
            historical: record.freshness,
            effective,
        };
        Read::Recorded {
            targets: record
                .edges
                .iter()
                .filter(|e| !matches!(e.kind, Kind::Lookup | Kind::Consumes))
                .cloned()
                .map(|edge| Target {
                    edge,
                    labels: labels.clone(),
                })
                .collect(),
            labels,
            record: record.into(),
        }
    }
    fn proves_absence(&self, pin: Pin, key: Key) -> bool {
        if self.fault == Fault::OldAbsence {
            return matches!(
                self.read(pin, key),
                Read::Recorded {
                    record: PublicRecord {
                        outcome: Outcome::Empty,
                        ..
                    },
                    ..
                }
            );
        }
        if self.fault == Fault::IgnoreCoverage {
            return matches!(
                self.read(pin, key),
                Read::Recorded {
                    labels: Labels {
                        older: false,
                        effective: Freshness::Fresh,
                        ..
                    },
                    record: PublicRecord {
                        outcome: Outcome::Empty,
                        ..
                    },
                    ..
                }
            );
        }
        matches!(
            self.read(pin, key),
            Read::Recorded {
                labels: Labels {
                    older: false,
                    effective: Freshness::Fresh,
                    ..
                },
                record: PublicRecord {
                    outcome: Outcome::Empty,
                    complete: true,
                    ..
                },
                ..
            }
        )
    }
    // Caller supplies the complete required selection/document set for this
    // abstract query. An empty or unenumerated coverage set is not proof.
    fn proves_query_absence(&self, pin: Pin, required: &[Key]) -> bool {
        let Some(view) = self.views.get(&pin) else {
            return false;
        };
        let expected: BTreeSet<Key> = view
            .capture
            .required
            .iter()
            .copied()
            .filter(|(d, _)| view.capture.docs.contains_key(d))
            .collect();
        let supplied: BTreeSet<Key> = required.iter().copied().collect();
        let represented: BTreeSet<Selection> = supplied.iter().map(|&(d, p)| (p, d % 2)).collect();
        !expected.is_empty()
            && (self.fault == Fault::PartialCoverage
                || (supplied == expected
                    && represented == expected.iter().map(|&(d, p)| (p, d % 2)).collect()))
            && supplied.iter().all(|&key| self.proves_absence(pin, key))
    }
    fn endpoints(&self, pin: Pin, key: Key) -> Result<Vec<Endpoint>, QueryError> {
        let Read::Recorded { targets, .. } = self.read(pin, key) else {
            return match self.read(pin, key) {
                Read::Corrupt => Err(QueryError::Corrupt),
                Read::UnresolvedDependency => Err(QueryError::Unresolved),
                _ => Err(QueryError::Pending),
            };
        };
        let mut result = Vec::new();
        for target in targets {
            if target.edge.external {
                result.push(Endpoint::External(target));
                continue;
            }
            let read = self.read(pin, target.edge.target);
            if read == Read::Corrupt {
                return Err(QueryError::Corrupt);
            }
            if read == Read::UnresolvedDependency {
                return Err(QueryError::Unresolved);
            }
            if read == Read::NotCovered {
                return Err(QueryError::Unresolved);
            }
            if matches!(read, Read::Pending { .. }) {
                if self.fault == Fault::HidePending {
                    result.push(Endpoint::UnresolvedAuthority(target, read));
                } else {
                    result.push(Endpoint::PendingBoundary(target, read));
                }
                continue;
            }
            let selection = (target.edge.target.1, target.edge.target.0 % 2);
            let source_identity = self
                .semantics
                .get(&target.labels.basis)
                .and_then(|s| s.input_ids.get(&selection));
            let current_identity = self.views[&pin].input_ids.get(&selection);
            if source_identity != current_identity && self.fault != Fault::RetargetAuthority {
                result.push(Endpoint::UnresolvedAuthority(target, read));
                continue;
            }
            result.push(Endpoint::Internal(target, read));
        }
        Ok(result)
    }
    fn traverse(&self, pin: Pin, path: &[Key]) -> Result<Vec<Hop>, QueryError> {
        let mut out = Vec::new();
        for (i, &key) in path.iter().enumerate() {
            if i > 0 {
                match out.last() {
                    Some(Hop::Row(
                        _,
                        Read::Recorded {
                            record, targets, ..
                        },
                    )) if record.outcome == Outcome::Evidence(Claim::Resolved)
                        && targets.iter().any(|t| {
                            let e = &t.edge;
                            e.target == key
                                && (!e.external || self.fault == Fault::ExternalCollision)
                                && matches!(
                                    e.kind,
                                    Kind::Symbol
                                        | Kind::RefDeclared
                                        | Kind::CallDeclared
                                        | Kind::TypeFrom
                                        | Kind::TypeTo
                                )
                        }) => {}
                    Some(Hop::Boundary(_, _)) if self.fault == Fault::CrossPending => {}
                    _ => break,
                }
            }
            if i > 0
                && self.fault == Fault::HidePending
                && let Some(Hop::Row(_, Read::Recorded { targets, .. })) = out.last()
                && let Some(target) = targets
                    .iter()
                    .find(|t| !t.edge.external && t.edge.target == key)
                && let Some(&basis) = self.views[&pin].basis.get(&(key.1, key.0 % 2))
                && basis != target.labels.basis
            {
                out.push(Hop::AuthorityBoundary(key, target.clone(), basis));
                break;
            }
            let mut read = self.read(pin, key);
            if i > 0
                && self.fault == Fault::UnlabelNested
                && let Read::Recorded { targets, .. } = &mut read
            {
                for target in targets {
                    target.labels.basis = 0;
                }
            }
            if read == Read::Corrupt {
                return Err(QueryError::Corrupt);
            }
            if read == Read::UnresolvedDependency {
                return Err(QueryError::Unresolved);
            }
            if read == Read::NotCovered {
                return Err(QueryError::Unresolved);
            }
            if matches!(read, Read::Pending { .. }) {
                if self.fault == Fault::CrossPending {
                    out.push(Hop::Boundary(key, read));
                    continue;
                }
                out.push(Hop::Boundary(key, read));
                break;
            }
            if i > 0
                && let Some(Hop::Row(_, Read::Recorded { targets, .. })) = out.last()
                && let Some(target) = targets
                    .iter()
                    .find(|t| !t.edge.external && t.edge.target == key)
                && let Some(&basis) = self.views[&pin].basis.get(&(key.1, key.0 % 2))
                && self
                    .semantics
                    .get(&target.labels.basis)
                    .and_then(|s| s.input_ids.get(&(key.1, key.0 % 2)))
                    != self.views[&pin].input_ids.get(&(key.1, key.0 % 2))
                && self.fault != Fault::RetargetAuthority
            {
                out.push(Hop::AuthorityBoundary(key, target.clone(), basis));
                break;
            }
            out.push(Hop::Row(key, read));
        }
        Ok(out)
    }
}

fn eligible(
    model: &Model,
    view: &View,
    key: Key,
    explicit: Option<Pin>,
    seen: &mut BTreeSet<(Key, Pin)>,
) -> Result<bool, QueryError> {
    let basis = explicit
        .or_else(|| view.basis.get(&(key.1, key.0 % 2)).copied())
        .ok_or(QueryError::Corrupt)?;
    if !seen.insert((key, basis)) {
        return Ok(true);
    }
    let base = model.semantics.get(&basis).ok_or(QueryError::Corrupt)?;
    let now = &view.capture;
    if !base.capture.docs.contains_key(&key.0) {
        return Ok(false);
    }
    // An existing document newly admitted to this producer/scope has no P
    // tuple by design; only a missing tuple that P required is corruption.
    if !base.capture.required.contains(&key) && model.fault != Fault::UnselectedIsCorrupt {
        return Ok(false);
    }
    let Some(record) = base.rows.get(&key) else {
        return if model.fault == Fault::MissingAsPending {
            Ok(false)
        } else {
            Err(QueryError::Corrupt)
        };
    };
    let scope = (key.1, key.0 % 2);
    let old_context = base
        .capture
        .contexts
        .get(&scope)
        .ok_or(QueryError::Corrupt)?;
    let new_context = now.contexts.get(&scope).ok_or(QueryError::Corrupt)?;
    let mut valid =
        base.capture.docs.get(&key.0) == now.docs.get(&key.0) && old_context == new_context;
    for edge in &record.edges {
        if edge.kind == Kind::Lookup && !edge.mapped {
            continue;
        }
        if model.fault == Fault::IgnoreCandidate
            && matches!(
                edge.kind,
                Kind::DeclCandidate | Kind::RefCandidate | Kind::CallCandidate
            )
        {
            continue;
        }
        if !edge.external
            && (!base.capture.docs.contains_key(&edge.target.0)
                || base.capture.docs.get(&edge.target.0) != now.docs.get(&edge.target.0))
        {
            valid = false;
        }
        if let Some(scope) = edge.context {
            let old_context = base
                .capture
                .contexts
                .get(&scope)
                .ok_or(QueryError::Corrupt)?;
            let new_context = now.contexts.get(&scope).ok_or(QueryError::Corrupt)?;
            if model.fault != Fault::IgnoreContext && old_context != new_context {
                valid = false;
            }
        }
        if edge.kind == Kind::Consumes && !edge.external {
            let consumed = if model.fault == Fault::ConsumeLatest {
                view.basis[&(edge.target.1, edge.target.0 % 2)]
            } else {
                edge.consumed_basis.ok_or(QueryError::Unresolved)?
            };
            if !eligible(model, view, edge.target, Some(consumed), seen)? {
                valid = false;
            }
            let selection = (edge.target.1, edge.target.0 % 2);
            let consumed_identity = model
                .semantics
                .get(&consumed)
                .and_then(|s| s.input_ids.get(&selection))
                .ok_or(QueryError::Corrupt)?;
            let current_identity = view.input_ids.get(&selection).ok_or(QueryError::Corrupt)?;
            if model.fault != Fault::ConsumeStaleFresh && consumed_identity != current_identity {
                return Err(QueryError::Unresolved);
            }
        }
    }
    Ok(valid)
}

// Independent finite fixed-point formulation for complete tuple tables. This
// propagates only explicit result-consumption edges, across producer slots.
fn reference_pending(model: &Model, view: &View) -> BTreeSet<Key> {
    let now = &view.capture;
    let mut bad = BTreeSet::new();
    for d in 0..3 {
        for p in 0..2 {
            // A currently unselected existing document is NotCovered, not
            // pending, regardless of whether P selected it.
            if now.docs.contains_key(&d) && !now.required.contains(&(d, p)) {
                continue;
            }
            let base = model.base_for(view, (d, p)).unwrap();
            if !base.capture.docs.contains_key(&d)
                || !base.capture.required.contains(&(d, p))
                || base.capture.docs.get(&d) != now.docs.get(&d)
                || base.capture.contexts.get(&(p, d % 2)) != now.contexts.get(&(p, d % 2))
            {
                bad.insert((d, p));
            }
        }
    }
    loop {
        let before = bad.len();
        for p in 0..2 {
            for d in 0..3 {
                if now.docs.contains_key(&d) && !now.required.contains(&(d, p)) {
                    continue;
                }
                let base = model.base_for(view, (d, p)).unwrap();
                for (&key, row) in &base.rows {
                    if key != (d, p) {
                        continue;
                    }
                    for e in &row.edges {
                        if e.kind == Kind::Lookup && !e.mapped {
                            continue;
                        }
                        if (!e.external
                            && (!base.capture.docs.contains_key(&e.target.0)
                                || base.capture.docs.get(&e.target.0) != now.docs.get(&e.target.0)))
                            || e.context.is_some_and(|scope| {
                                base.capture.contexts.get(&scope) != now.contexts.get(&scope)
                            })
                            || (e.kind == Kind::Consumes && !e.external && bad.contains(&e.target))
                        {
                            bad.insert(key);
                        }
                    }
                }
            }
        }
        if bad.len() == before {
            return bad;
        }
    }
}

// Separate fixed-point reference over (record key, recorded semantic basis).
// Used only with complete, internally bound fixture graphs.
fn reference_bound_graph(model: &Model, view: &View) -> BTreeSet<(Key, Pin)> {
    let mut bad = BTreeSet::new();
    for (&pin, base) in &model.semantics {
        for &key in base.rows.keys() {
            if base.capture.docs.get(&key.0) != view.capture.docs.get(&key.0)
                || base.capture.contexts.get(&(key.1, key.0 % 2))
                    != view.capture.contexts.get(&(key.1, key.0 % 2))
            {
                bad.insert((key, pin));
            }
        }
    }
    loop {
        let before = bad.len();
        for (&pin, base) in &model.semantics {
            for (&key, row) in &base.rows {
                for e in &row.edges {
                    if e.external || (e.kind == Kind::Lookup && !e.mapped) {
                        continue;
                    }
                    if base.capture.docs.get(&e.target.0) != view.capture.docs.get(&e.target.0)
                        || (e.kind == Kind::Consumes
                            && e.consumed_basis
                                .is_some_and(|b| bad.contains(&(e.target, b))))
                    {
                        bad.insert((key, pin));
                    }
                }
            }
        }
        if before == bad.len() {
            return bad;
        }
    }
}

fn record(outcome: Outcome, edges: Vec<Edge>) -> Record {
    Record {
        outcome,
        freshness: Freshness::Fresh,
        edges,
        complete: true,
    }
}
fn edge(kind: Kind, doc: Doc, producer: Producer) -> Edge {
    Edge {
        kind,
        target: (doc, producer),
        mapped: true,
        context: None,
        external: false,
        consumed_basis: if kind == Kind::Consumes {
            Some(1)
        } else {
            None
        },
    }
}
fn fixture() -> Capture {
    let docs = (0..3)
        .map(|d| {
            (
                d,
                Document {
                    identity: d,
                    vid: 0,
                },
            )
        })
        .collect();
    let contexts = BTreeMap::from([
        ((0, 0), [0; 3]),
        ((0, 1), [0; 3]),
        ((1, 0), [0; 3]),
        ((1, 1), [0; 3]),
    ]);
    let mut output = BTreeMap::new();
    for d in 0..3 {
        for p in 0..2 {
            output.insert((d, p), record(Outcome::Empty, vec![]));
        }
    }
    output.insert(
        (0, 0),
        record(
            Outcome::Evidence(Claim::Resolved),
            vec![edge(Kind::CallDeclared, 1, 0)],
        ),
    );
    output.insert(
        (1, 0),
        record(
            Outcome::Evidence(Claim::Resolved),
            vec![edge(Kind::CallDeclared, 2, 0)],
        ),
    );
    Capture {
        docs,
        required: (0..3).flat_map(|d| (0..2).map(move |p| (d, p))).collect(),
        contexts,
        output,
    }
}

// Outcome 0 deliberately omits a required result; the other four are explicit
// validated outcomes. The mask is the producer membership of the new document.
fn add_document(m: &mut Model, mask: u8, outcome: u8) {
    if m.working.docs.contains_key(&3) {
        return;
    }
    m.working.docs.insert(
        3,
        Document {
            identity: 3,
            vid: 0,
        },
    );
    for p in 0..2 {
        let key = (3, p);
        m.working.required.remove(&key);
        m.working.output.remove(&key);
        if mask & (1 << p) == 0 {
            continue;
        }
        m.working.required.insert(key);
        let value = match outcome {
            0 => None,
            1 => Some(Outcome::Empty),
            2 => Some(Outcome::NotRequested),
            3 => Some(Outcome::Failed),
            _ => Some(Outcome::Evidence(Claim::Resolved)),
        };
        if let Some(value) = value {
            m.working.output.insert(key, record(value, vec![]));
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Edit(Doc, u8),
    Add(u8, u8),
    Delete(Doc),
    Rename(Doc, u8),
    Restore(Doc),
    Context(Producer, u8, u8),
    Set(Key, Record),
    Publish,
    Start,
    Unit,
    Commit,
    CancelOld,
    Import(bool),
    Read(Key),
    Traverse,
}
fn operations() -> impl Strategy<Value = Vec<Op>> {
    let claim = prop_oneof![
        Just(Outcome::Empty),
        Just(Outcome::NotRequested),
        Just(Outcome::Failed),
        Just(Outcome::Evidence(Claim::Unknown)),
        Just(Outcome::Evidence(Claim::Unresolved)),
        Just(Outcome::Evidence(Claim::Ambiguous)),
        Just(Outcome::Evidence(Claim::External)),
        Just(Outcome::Evidence(Claim::Resolved))
    ];
    let kinds = [
        Kind::Symbol,
        Kind::DeclCandidate,
        Kind::RefDeclared,
        Kind::RefCandidate,
        Kind::CallDeclared,
        Kind::CallCandidate,
        Kind::Dispatch,
        Kind::TypeFrom,
        Kind::TypeTo,
        Kind::Lookup,
        Kind::Consumes,
    ];
    let edge_strategy = (
        0..11usize,
        0..3u8,
        0..2u8,
        any::<bool>(),
        prop::option::of((0..2u8, 0..2u8)),
        any::<bool>(),
    )
        .prop_map(move |(k, d, p, m, context, external)| Edge {
            kind: kinds[k],
            target: (d, p),
            mapped: m,
            context,
            external,
            consumed_basis: None,
        });
    let rec = (
        claim,
        prop::collection::vec(edge_strategy, 0..4),
        0..3u8,
        any::<bool>(),
    )
        .prop_map(move |(outcome, edges, f, complete)| Record {
            outcome,
            freshness: [Freshness::Fresh, Freshness::PossiblyStale, Freshness::Stale][f as usize],
            edges,
            complete,
        });
    prop::collection::vec(
        prop_oneof![
            (0..3u8, 0..4u8).prop_map(|(d, v)| Op::Edit(d, v)),
            (1..4u8, 0..5u8).prop_map(|(mask, outcome)| Op::Add(mask, outcome)),
            (0..3u8).prop_map(Op::Delete),
            (0..3u8, 0..4u8).prop_map(|(d, v)| Op::Rename(d, v)),
            (0..3u8).prop_map(Op::Restore),
            (0..2u8, 0..2u8, 0..3u8).prop_map(|(p, s, v)| Op::Context(p, s, v)),
            ((0..3u8, 0..2u8), rec).prop_map(|(k, r)| Op::Set(k, r)),
            Just(Op::Publish),
            Just(Op::Start),
            Just(Op::Unit),
            Just(Op::Commit),
            Just(Op::CancelOld),
            any::<bool>().prop_map(Op::Import),
            (0..3u8, 0..2u8).prop_map(|(d, p)| Op::Read((d, p))),
            Just(Op::Traverse)
        ],
        1..55,
    )
}

fn bound_graph_edges() -> impl Strategy<Value = Vec<(u8, u8, u8, u8, bool)>> {
    (
        prop::collection::vec((0..3u8, 0..2u8, 0..3u8, 0..2u8, any::<bool>()), 0..9),
        (0..3u8, 0..2u8, 0..3u8, 0..2u8),
    )
        .prop_map(|(mut edges, (d, p, t, tp))| {
            edges.push((d, p, t, tp, true));
            edges
        })
}

fn newly_selected_model(fault: Fault) -> (Model, Pin) {
    let key = (2, 1);
    let mut capture = fixture();
    capture.required.remove(&key);
    capture.output.remove(&key);
    let mut model = Model::new(capture, fault);
    model.working.required.insert(key);
    model
        .working
        .output
        .insert(key, record(Outcome::Empty, vec![]));
    let pin = model.publish_native();
    (model, pin)
}

// Identical observations/assertions are used for the healthy and faulty model.
// Parameters vary the history while preserving the critical interaction.
fn scenario(fault: Fault, case: u8, variant: u8) -> bool {
    let mut c = fixture();
    if case == 1 {
        c.output.insert(
            (0, 0),
            record(
                Outcome::Evidence(Claim::Ambiguous),
                vec![
                    edge(Kind::RefCandidate, 1, 0),
                    edge(Kind::RefCandidate, 2, 1),
                ],
            ),
        );
    }
    let mut m = Model::new(c, fault);
    match case {
        0 => {
            m.working.docs.get_mut(&1).unwrap().vid = variant + 1;
            let p = m.publish_native();
            matches!(m.read(p, (0, 0)), Read::Pending { .. })
        }
        1 => {
            m.working.docs.get_mut(&2).unwrap().vid = variant + 1;
            let p = m.publish_native();
            matches!(m.read(p, (0, 0)), Read::Pending { .. })
        }
        2 => {
            m.working.contexts.get_mut(&(1, 0)).unwrap()[0] = 1;
            let p = m.publish_native();
            let hops = m.traverse(p, &[(0, 0), (1, 0)]).unwrap();
            matches!(&hops[..], [Hop::Row(_, Read::Recorded { labels: a, .. }), Hop::Row(_, Read::Recorded { labels: b, targets, .. })] if a.pin == p && b.pin == p && a.native == p && b.native == p && a.basis == 1 && b.basis == 1 && a.older && b.older && !targets.is_empty() && targets.iter().all(|t| t.labels == *b))
        }
        3 => {
            m.working.docs.get_mut(&2).unwrap().vid = variant + 1;
            let p = m.publish_native();
            let hops = m.traverse(p, &[(0, 0), (1, 0), (2, 0)]).unwrap();
            matches!(&hops[..], [Hop::Row((0,0), Read::Recorded { .. }), Hop::Boundary((1,0), Read::Pending { pin, native, basis, basis_native, .. })] if *pin == p && *native == p && *basis == 1 && *basis_native == 1)
        }
        4 => {
            m.working.docs.get_mut(&2).unwrap().vid = variant + 1;
            m.publish_native();
            m.working.docs.get_mut(&2).unwrap().vid = 0;
            let p = m.publish_native();
            matches!(
                m.read(p, (1, 0)),
                Read::Recorded {
                    labels: Labels {
                        basis: 1,
                        older: true,
                        ..
                    },
                    ..
                }
            )
        }
        5 => {
            let stage = m.start().unwrap();
            m.finish(stage);
            let imported = m.import(true);
            m.commit(stage) == Commit::Conflict && m.head == imported
        }
        6 => {
            let old = m.start().unwrap();
            let successor = m.start().unwrap();
            m.cancel(old);
            m.active == Some(successor) && m.stages.contains_key(&successor)
        }
        7 => {
            let n = m.publish_native();
            let c = m.views[&n].capture.clone();
            m.publish_selected(n, c.clone(), recompute(&c), &[0]);
            let n2 = m.publish_native();
            m.views[&n2].basis[&(1, 0)] == 1
        }
        8 => {
            let mut e = edge(Kind::RefCandidate, 1, 0);
            e.context = Some((1, 1));
            m.semantics
                .get_mut(&1)
                .unwrap()
                .rows
                .get_mut(&(0, 0))
                .unwrap()
                .edges = vec![e];
            m.working.contexts.get_mut(&(1, 1)).unwrap()[0] = variant + 1;
            let n = m.publish_native();
            matches!(m.read(n, (0, 0)), Read::Pending { .. })
        }
        9 => {
            m.semantics.get_mut(&1).unwrap().rows.remove(&(0, 0));
            m.working.docs.get_mut(&0).unwrap().vid = variant + 1;
            let n = m.publish_native();
            m.read(n, (0, 0)) == Read::Corrupt
        }
        10 => {
            m.semantics
                .get_mut(&1)
                .unwrap()
                .rows
                .get_mut(&(0, 0))
                .unwrap()
                .edges = vec![edge(Kind::Consumes, 1, 1)];
            m.semantics.get_mut(&1).unwrap().rows.remove(&(1, 1));
            m.working.docs.get_mut(&0).unwrap().vid = variant + 1;
            let n = m.publish_native();
            m.read(n, (0, 0)) == Read::Corrupt
        }
        11 => {
            m.working.contexts.get_mut(&(1, 0)).unwrap()[0] = 1;
            let n = m.publish_native();
            !m.proves_absence(n, (2, 0))
        }
        12 => {
            m.working.docs.get_mut(&2).unwrap().vid = variant + 1;
            let n = m.publish_native();
            m.copy_same_native(n).is_none()
        }
        13 => {
            let mut e = edge(Kind::CallDeclared, 1, 0);
            e.external = true;
            m.semantics
                .get_mut(&1)
                .unwrap()
                .rows
                .get_mut(&(0, 0))
                .unwrap()
                .edges = vec![e];
            m.traverse(1, &[(0, 0), (1, 0)]).unwrap().len() == 1
        }
        14 => {
            let t = m.start().unwrap();
            m.finish(t);
            m.working.docs.get_mut(&0).unwrap().vid = 0;
            m.publish_native();
            m.commit(t) == Commit::Published
        }
        15 => {
            m.semantics
                .get_mut(&1)
                .unwrap()
                .rows
                .get_mut(&(0, 0))
                .unwrap()
                .edges = vec![edge(Kind::Consumes, 1, 1)];
            m.semantics
                .get_mut(&1)
                .unwrap()
                .rows
                .get_mut(&(1, 1))
                .unwrap()
                .edges = vec![edge(Kind::RefCandidate, 2, 0)];
            m.working.docs.get_mut(&2).unwrap().vid = variant + 1;
            m.working.output.get_mut(&(1, 1)).unwrap().edges.clear();
            let n = m.publish_native();
            let c = m.views[&n].capture.clone();
            let p = m.publish_selected(n, c.clone(), recompute(&c), &[1]);
            matches!(m.read(p, (0, 0)), Read::Pending { .. })
        }
        16 => {
            m.semantics
                .get_mut(&1)
                .unwrap()
                .rows
                .get_mut(&(2, 0))
                .unwrap()
                .complete = false;
            !m.proves_absence(1, (2, 0))
        }
        17 => {
            m.working.docs.get_mut(&2).unwrap().vid = variant + 1;
            let n = m.publish_native();
            let c = m.views[&n].capture.clone();
            let mut rows = m.semantic_inputs.rows.clone();
            rows.get_mut(&(0, 0)).unwrap().outcome = Outcome::Evidence(Claim::External);
            m.publish_scopes(n, c, rows, &[(0, 0)]);
            m.working.docs.get_mut(&2).unwrap().vid = variant + 4;
            m.publish_native();
            let s = m.start().unwrap();
            m.finish(s);
            m.commit(s) == Commit::Published
                && m.semantics[&m.semantic].rows[&(0, 0)].outcome
                    == Outcome::Evidence(Claim::External)
        }
        18 => {
            let c = m.views[&m.native].capture.clone();
            let mut rows = m.semantic_inputs.rows.clone();
            rows.get_mut(&(1, 0)).unwrap().freshness = Freshness::Stale;
            let p = m.publish_scopes(m.native, c, rows, &[(0, 1)]);
            matches!(
                m.endpoints(p, (0, 0)).unwrap().as_slice(),
                [Endpoint::UnresolvedAuthority(_, _)]
            )
        }
        19 => !m.proves_query_absence(1, &[(2, 0)]),
        20 => {
            let old = m.head;
            let c = m.views[&m.native].capture.clone();
            let mut rows = m.semantic_inputs.rows.clone();
            rows.get_mut(&(0, 0)).unwrap().outcome = Outcome::Evidence(Claim::External);
            m.publish_scopes(m.native, c, rows, &[(0, 0)]);
            m.copy_same_native(old).is_none()
        }
        21 => {
            m.semantics
                .get_mut(&1)
                .unwrap()
                .rows
                .get_mut(&(0, 0))
                .unwrap()
                .edges = vec![edge(Kind::Consumes, 1, 1)];
            let c = m.views[&m.native].capture.clone();
            let mut rows = m.semantic_inputs.rows.clone();
            rows.get_mut(&(1, 1)).unwrap().outcome = Outcome::Failed;
            let p = m.publish_scopes(m.native, c, rows, &[(1, 1)]);
            m.read(p, (0, 0)) == Read::UnresolvedDependency
        }
        22 | 23 => {
            let c = m.views[&m.native].capture.clone();
            let mut rows = m.semantic_inputs.rows.clone();
            rows.get_mut(&(1, 0)).unwrap().freshness = Freshness::Stale;
            let p = m.publish_scopes(m.native, c, rows, &[(0, 1)]);
            if case == 23 {
                m.semantics.get_mut(&p).unwrap().rows.remove(&(1, 0));
                m.traverse(p, &[(0, 0), (1, 0)]) == Err(QueryError::Corrupt)
            } else {
                m.working.docs.get_mut(&2).unwrap().vid = variant + 1;
                let n = m.publish_native();
                matches!(
                    m.traverse(n, &[(0, 0), (1, 0)]).unwrap().as_slice(),
                    [Hop::Row(..), Hop::Boundary((1, 0), Read::Pending { .. })]
                )
            }
        }
        24 => {
            add_document(&mut m, 1, 0);
            m.publish_native();
            let t = m.start().unwrap();
            m.finish(t);
            m.commit(t) == Commit::Incomplete
        }
        25 => {
            let (model, pin) = newly_selected_model(fault);
            matches!(model.read(pin, (2, 1)), Read::Pending { .. })
        }
        _ => unreachable!(),
    }
}

proptest! {
    #[test]
    fn generated_same_assertion_mutation_calibration(variant in 0..3u8) {
        let faults = [Fault::PendingAbsence,Fault::IgnoreCandidate,Fault::UnlabelNested,Fault::CrossPending,Fault::Rechain,Fault::StaleCas,Fault::OldCleanup,Fault::GlobalBasis,Fault::IgnoreContext,Fault::MissingAsPending,Fault::MissingAsPending,Fault::OldAbsence,Fault::CopyPromotion,Fault::ExternalCollision,Fault::NoopInvalidation,Fault::ConsumeLatest,Fault::IgnoreCoverage,Fault::OverwriteImport,Fault::RetargetAuthority,Fault::PartialCoverage,Fault::CopyOldInputs,Fault::ConsumeStaleFresh,Fault::HidePending,Fault::HidePending,Fault::PublishIncomplete,Fault::UnselectedIsCorrupt];
        for (case, fault) in faults.into_iter().enumerate() {
            prop_assert!(scenario(Fault::None,case as u8,variant), "healthy case {}", case);
            prop_assert!(!scenario(fault,case as u8,variant), "surviving mutant {:?}", fault);
        }
    }
}

proptest! {
    #[test]
    fn generated_same_native_consumed_result_replacement_is_uncertain(
        replacement in prop_oneof![Just(Outcome::Failed),Just(Outcome::NotRequested),Just(Outcome::Evidence(Claim::Unknown)),Just(Outcome::Evidence(Claim::Resolved))],
        freshness in 0..3usize,
    ) {
        let mut c=fixture(); c.output.insert((0,0),record(Outcome::Evidence(Claim::Resolved),vec![edge(Kind::Consumes,1,1)]));
        let mut m=Model::new(c,Fault::None); let before=m.read(1,(0,0)); let native=m.native; let revision=m.revision;
        let cap=m.views[&native].capture.clone(); let mut rows=m.semantic_inputs.rows.clone();
        rows.insert((1,1),Record {outcome:replacement,freshness:[Freshness::Fresh,Freshness::PossiblyStale,Freshness::Stale][freshness],edges:vec![],complete:true});
        let pin=m.publish_scopes(native,cap,rows,&[(1,1)]);
        prop_assert_eq!(m.native,native); prop_assert_eq!(m.revision,revision);
        prop_assert_eq!(m.read(pin,(0,0)),Read::UnresolvedDependency);
        prop_assert_eq!(m.read(1,(0,0)),before);
    }
}

fn added_document_case(mask: u8, outcome: u8) -> bool {
    let mut m = Model::new(fixture(), Fault::None);
    add_document(&mut m, mask, outcome);
    let n = m.publish_native();
    let stage = m.start().unwrap();
    let s = m.stages[&stage].clone();
    m.finish(stage);
    if outcome == 0 {
        return m.commit(stage) == Commit::Incomplete && m.head == n;
    }
    if m.commit(stage) != Commit::Published
        || m.semantics[&m.semantic].rows != reference_full(&s.selected, &s.semantic_inputs)
    {
        return false;
    }
    for p in 0..2 {
        let read = m.read(m.head, (3, p));
        if mask & (1 << p) == 0 {
            if read != Read::NotCovered {
                return false;
            }
        } else {
            let expected = match outcome {
                1 => Outcome::Empty,
                2 => Outcome::NotRequested,
                3 => Outcome::Failed,
                _ => Outcome::Evidence(Claim::Resolved),
            };
            if !matches!(read,Read::Recorded {record:PublicRecord {outcome,..},..} if outcome==expected)
            {
                return false;
            }
        }
    }
    true
}

#[test]
fn all_new_document_membership_and_result_combinations() {
    for mask in 1..4 {
        for outcome in 0..5 {
            assert!(
                added_document_case(mask, outcome),
                "mask={mask} outcome={outcome}"
            );
        }
    }
}

#[test]
fn existing_document_newly_selected_is_pending_but_missing_required_is_corrupt() {
    let (mut model, pin) = newly_selected_model(Fault::None);
    assert_eq!(model.read(1, (2, 1)), Read::NotCovered);
    assert!(matches!(model.read(pin, (2, 1)), Read::Pending { .. }));
    let stage = model.start().unwrap();
    model.finish(stage);
    assert_eq!(model.commit(stage), Commit::Published);
    assert!(matches!(
        model.read(model.head, (2, 1)),
        Read::Recorded {
            record: PublicRecord {
                outcome: Outcome::Empty,
                ..
            },
            ..
        }
    ));

    let mut corrupt = Model::new(fixture(), Fault::None);
    corrupt.semantics.get_mut(&1).unwrap().rows.remove(&(2, 1));
    corrupt.working.contexts.get_mut(&(1, 0)).unwrap()[0] = 1;
    let changed = corrupt.publish_native();
    assert_eq!(corrupt.read(changed, (2, 1)), Read::Corrupt);
}

#[test]
fn reference_and_model_distinguish_selection_and_missing_rows() {
    #[derive(Debug, PartialEq, Eq)]
    enum State {
        NotCovered,
        Pending,
        Corrupt,
        Recorded,
    }
    fn model_state(m: &Model, pin: Pin, key: Key) -> State {
        match m.read(pin, key) {
            Read::NotCovered => State::NotCovered,
            Read::Pending { .. } => State::Pending,
            Read::Corrupt => State::Corrupt,
            Read::Recorded { .. } => State::Recorded,
            other => panic!("unexpected {other:?}"),
        }
    }
    fn reference_state(m: &Model, pin: Pin, key: Key) -> State {
        let view = &m.views[&pin];
        if view.capture.docs.contains_key(&key.0) && !view.capture.required.contains(&key) {
            return State::NotCovered;
        }
        let base = m.base_for(view, key).unwrap();
        if base.capture.docs.contains_key(&key.0)
            && base.capture.required.contains(&key)
            && !base.rows.contains_key(&key)
        {
            return State::Corrupt;
        }
        if reference_pending(m, view).contains(&key) {
            State::Pending
        } else {
            State::Recorded
        }
    }
    for (selected_p, selected_n, missing, expected) in [
        (true, true, false, State::Recorded),
        (false, false, false, State::NotCovered),
        (false, true, false, State::Pending),
        (true, false, false, State::NotCovered),
        (true, true, true, State::Corrupt),
    ] {
        let key = (2, 1);
        let mut c = fixture();
        if !selected_p {
            c.required.remove(&key);
            c.output.remove(&key);
        }
        let mut m = Model::new(c, Fault::None);
        if missing {
            m.semantics.get_mut(&1).unwrap().rows.remove(&key);
        }
        if selected_n {
            m.working.required.insert(key);
            m.working.output.insert(key, record(Outcome::Empty, vec![]));
        } else {
            m.working.required.remove(&key);
        }
        let pin = m.publish_native();
        assert_eq!(
            reference_state(&m, pin, key),
            expected,
            "reference P={selected_p} N={selected_n} missing={missing}"
        );
        assert_eq!(
            model_state(&m, pin, key),
            expected,
            "model P={selected_p} N={selected_n} missing={missing}"
        );
    }
}

#[test]
fn observer_stops_at_newly_selected_target_without_tainting_source() {
    let key = (2, 1);
    let mut c = fixture();
    c.required.remove(&key);
    c.output.remove(&key);
    c.output.insert(
        (0, 0),
        record(
            Outcome::Evidence(Claim::Resolved),
            vec![edge(Kind::CallDeclared, 2, 1)],
        ),
    );
    let mut m = Model::new(c, Fault::None);
    m.working.required.insert(key);
    m.working.output.insert(key, record(Outcome::Empty, vec![]));
    let n = m.publish_native();
    let view = &m.views[&n];
    let pending = reference_pending(&m, view);
    assert!(!pending.contains(&(0, 0)));
    assert!(pending.contains(&key));
    assert!(matches!(
        m.traverse(n, &[(0, 0), key]).unwrap().as_slice(),
        [
            Hop::Row((0, 0), Read::Recorded { .. }),
            Hop::Boundary((2, 1), Read::Pending { .. })
        ]
    ));
}
proptest! {
    #[test]
    fn generated_new_documents(mask in 1..4u8,outcome in 0..5u8) {
        prop_assert!(added_document_case(mask,outcome));
    }
}

proptest! {
    #[test]
    fn generated_bound_consumption_matches_fixed_point(
        edges in bound_graph_edges(),
        changed in 0..3u8,
        target_scope in 0..2u8,
    ) {
        let mut c=fixture();
        for r in c.output.values_mut() { r.edges.clear(); r.outcome=Outcome::Evidence(Claim::Resolved); }
        for (d,p,t,tp,consume) in edges {
            c.output.get_mut(&(d,p)).unwrap().edges.push(edge(if consume {Kind::Consumes} else {Kind::RefCandidate},t,tp));
        }
        let mut m=Model::new(c,Fault::None);
        m.working.docs.get_mut(&changed).unwrap().vid=1;
        let n=m.publish_native(); let cap=m.views[&n].capture.clone();
        let mut rows=m.semantic_inputs.rows.clone();
        for (&(d,p),row) in &mut rows { if p==1 && d%2==target_scope { row.edges.clear(); } }
        let paired=m.publish_scopes(n,cap,rows,&[(1,target_scope)]);
        let view=&m.views[&paired]; let bad=reference_bound_graph(&m,view);
        for d in 0..3 { for p in 0..2 {
            let basis=view.basis[&(p,d%2)];
            let observed=m.read(paired,(d,p));
            if observed != Read::UnresolvedDependency { prop_assert_eq!(matches!(observed,Read::Pending { .. }),bad.contains(&((d,p),basis)),"key {:?} basis {:?}",(d,p),basis); }
        }}
    }
}

#[test]
fn scoped_context_outcomes_and_traversal() {
    let mut c = fixture();
    c.output.insert(
        (2, 0),
        Record {
            outcome: Outcome::Empty,
            freshness: Freshness::Stale,
            edges: vec![],
            complete: true,
        },
    );
    c.output
        .insert((2, 1), record(Outcome::NotRequested, vec![]));
    let mut m = Model::new(c, Fault::None);
    m.working.contexts.get_mut(&(0, 0)).unwrap()[0] = 1;
    let p = m.publish_native();
    assert!(matches!(m.read(p, (2, 0)), Read::Pending { .. }));
    assert!(matches!(m.read(p, (1, 0)), Read::Recorded { .. }));
    assert!(matches!(
        m.read(p, (2, 1)),
        Read::Recorded {
            record: PublicRecord {
                outcome: Outcome::NotRequested,
                ..
            },
            labels: Labels {
                older: true,
                effective: Freshness::PossiblyStale,
                ..
            },
            ..
        }
    ));
    m.working.contexts.get_mut(&(0, 0)).unwrap()[0] = 0;
    let p = m.publish_native();
    assert!(matches!(
        m.read(p, (2, 0)),
        Read::Recorded {
            record: PublicRecord {
                outcome: Outcome::Empty,
                ..
            },
            labels: Labels {
                older: true,
                historical: Freshness::Stale,
                effective: Freshness::Stale,
                ..
            },
            ..
        }
    ));
    m.semantics.get_mut(&1).unwrap().rows.remove(&(2, 1));
    assert_eq!(m.read(p, (2, 1)), Read::Corrupt);
}

#[test]
fn historical_edges_lookup_and_cross_producer_consumption() {
    let mut c = fixture();
    let mut unmapped = edge(Kind::Lookup, 2, 0);
    unmapped.mapped = false;
    c.output.insert(
        (0, 1),
        record(Outcome::Evidence(Claim::External), vec![unmapped]),
    );
    c.output.insert(
        (0, 0),
        record(
            Outcome::Evidence(Claim::Unresolved),
            vec![edge(Kind::Consumes, 1, 1)],
        ),
    );
    c.output.insert(
        (1, 1),
        record(
            Outcome::Evidence(Claim::Ambiguous),
            vec![edge(Kind::CallCandidate, 2, 0)],
        ),
    );
    let mut m = Model::new(c, Fault::None);
    m.working.docs.get_mut(&2).unwrap().vid = 1;
    let p = m.publish_native();
    assert!(matches!(m.read(p, (0, 1)), Read::Recorded { .. }));
    assert!(matches!(m.read(p, (0, 0)), Read::Pending { .. }));
}

#[test]
fn unresolved_candidates_are_not_traversed_as_current_resolution() {
    let mut c = fixture();
    c.output.insert(
        (0, 0),
        record(
            Outcome::Evidence(Claim::Ambiguous),
            vec![edge(Kind::RefCandidate, 1, 0)],
        ),
    );
    let mut m = Model::new(c, Fault::None);
    let pin = m.publish_native();
    let hops = m.traverse(pin, &[(0, 0), (1, 0)]).unwrap();
    assert!(matches!(
        &hops[..],
        [Hop::Row(
            (0, 0),
            Read::Recorded {
                record: PublicRecord {
                    outcome: Outcome::Evidence(Claim::Ambiguous),
                    ..
                },
                ..
            }
        )]
    ));
}

#[test]
fn unpublished_edit_semantic_conflict_and_stage_ownership() {
    let mut m = Model::new(fixture(), Fault::None);
    let unfinished = m.start().unwrap();
    m.unit(unfinished);
    assert_eq!(m.commit(unfinished), Commit::Conflict);
    let s = m.start().unwrap();
    m.finish(s);
    let selected = m.stages[&s].selected.clone();
    let selected_inputs = m.stages[&s].semantic_inputs.clone();
    m.working.docs.get_mut(&0).unwrap().vid = selected.docs[&0].vid; // no-op edit
    m.working.docs.get_mut(&2).unwrap().vid = 1;
    assert_eq!(m.commit(s), Commit::Published);
    assert_eq!(
        m.semantics[&m.semantic].rows,
        reference_full(&selected, &selected_inputs)
    );
    m.working.docs.get_mut(&1).unwrap().vid = 1;
    m.publish_native();
    let s = m.start().unwrap();
    m.finish(s);
    let captured = m.stages[&s].clone();
    m.import(true);
    assert_eq!(m.commit(s), Commit::Conflict);
    assert_eq!(m.recheck(&captured), Recheck::Fulfilled);
    m.working.docs.get_mut(&1).unwrap().vid = 2;
    m.publish_native();
    let s = m.start().unwrap();
    m.finish(s);
    let captured = m.stages[&s].clone();
    m.import(false);
    let imported = m.head;
    assert_eq!(m.commit(s), Commit::Conflict);
    assert_eq!(m.recheck(&captured), Recheck::Outstanding);
    let restage = m.start().unwrap();
    let restaged_selections = m.stages[&restage].selections.clone();
    let restaged_inputs = m.stages[&restage].semantic_inputs.clone();
    m.finish(restage);
    assert_eq!(m.commit(restage), Commit::Published);
    assert_eq!(
        m.semantics[&m.semantic].rows,
        reference_selected(&captured.selected, &restaged_inputs, &restaged_selections)
    );
    assert_eq!(m.views[&m.head].basis[&(1, 0)], imported);
    m.working.docs.get_mut(&1).unwrap().vid = 3;
    m.publish_native();
    let successor = m.start().unwrap();
    m.finish(successor);
    m.cancel(s);
    assert_eq!(m.active, Some(successor));
    let captured = m.stages[&successor].clone();
    m.working.docs.get_mut(&2).unwrap().vid = 2;
    m.publish_native();
    assert_eq!(m.commit(successor), Commit::Conflict);
    assert_eq!(m.recheck(&captured), Recheck::Superseded);
}

#[test]
fn per_producer_bases_and_partial_fulfilment() {
    let mut m = Model::new(fixture(), Fault::None);
    let n = m.publish_native();
    let stage = m.start().unwrap();
    m.finish(stage);
    let captured = m.stages[&stage].clone();
    let c = m.views[&n].capture.clone();
    let p = m.publish_selected(n, c.clone(), recompute(&c), &[0]);
    assert_eq!(m.target.outstanding, BTreeSet::from([(1, 0), (1, 1)]));
    assert_eq!(m.commit(stage), Commit::Conflict);
    assert_eq!(m.recheck(&captured), Recheck::Outstanding);
    let n2 = m.publish_native();
    assert_eq!(m.views[&n2].basis[&(0, 0)], p);
    assert_eq!(m.views[&n2].basis[&(1, 0)], 1);
    assert!(
        matches!(m.read(n2,(2,0)),Read::Recorded { labels: Labels { basis, .. }, .. } if basis == p)
    );
    assert!(matches!(
        m.read(n2, (2, 1)),
        Read::Recorded {
            labels: Labels { basis: 1, .. },
            ..
        }
    ));
}

#[test]
fn recorded_context_and_required_corruption() {
    let mut c = fixture();
    let mut e = edge(Kind::RefCandidate, 1, 1);
    e.context = Some((1, 1));
    c.output
        .insert((0, 0), record(Outcome::Evidence(Claim::Ambiguous), vec![e]));
    let mut m = Model::new(c, Fault::None);
    m.working.contexts.get_mut(&(1, 0)).unwrap()[0] = 1;
    let n = m.publish_native();
    assert!(matches!(m.read(n, (0, 0)), Read::Recorded { .. }));
    m.working.contexts.get_mut(&(1, 1)).unwrap()[0] = 1;
    let n = m.publish_native();
    assert!(matches!(m.read(n, (0, 0)), Read::Pending { .. }));
    m.semantics.get_mut(&1).unwrap().rows.remove(&(0, 0));
    m.working.docs.get_mut(&0).unwrap().vid = 3;
    let n = m.publish_native();
    assert_eq!(m.read(n, (0, 0)), Read::Corrupt);
    let mut c = fixture();
    c.output.insert(
        (0, 0),
        record(
            Outcome::Evidence(Claim::Unresolved),
            vec![edge(Kind::Consumes, 1, 1)],
        ),
    );
    let mut m = Model::new(c, Fault::None);
    m.semantics.get_mut(&1).unwrap().rows.remove(&(1, 1));
    m.working.docs.get_mut(&0).unwrap().vid = 2;
    m.working.docs.get_mut(&1).unwrap().vid = 2;
    let n = m.publish_native();
    assert_eq!(m.read(n, (0, 0)), Read::Corrupt);
}

#[test]
fn negative_conclusions_and_detachable_candidates() {
    let mut c = fixture();
    let mut external = edge(Kind::Dispatch, 2, 1);
    external.external = true;
    c.output.insert(
        (0, 0),
        record(
            Outcome::Evidence(Claim::Ambiguous),
            vec![
                edge(Kind::RefCandidate, 1, 0),
                edge(Kind::RefCandidate, 2, 0),
                external,
            ],
        ),
    );
    let mut m = Model::new(c, Fault::None);
    assert!(m.proves_absence(1, (2, 0)));
    m.working.contexts.get_mut(&(1, 1)).unwrap()[0] = 1;
    let n = m.publish_native();
    assert!(!m.proves_absence(n, (2, 0)));
    match m.read(n, (0, 0)) {
        Read::Recorded {
            labels, targets, ..
        } => {
            assert_eq!(targets.len(), 3);
            assert!(targets.iter().all(|t| t.labels == labels));
            assert!(targets[2].edge.external);
        }
        other => panic!("unexpected {other:?}"),
    }
    let endpoints = m.endpoints(n, (0, 0)).unwrap();
    assert!(
        matches!(&endpoints[..], [Endpoint::Internal(a,Read::Recorded { .. }), Endpoint::Internal(b,Read::Recorded { .. }), Endpoint::External(c)] if a.edge.target == (1,0) && b.edge.target == (2,0) && c.edge.external && a.labels.basis == 1 && b.labels.basis_native == 1)
    );
    let mut corrupt = m.clone();
    corrupt.semantics.get_mut(&1).unwrap().rows.remove(&(2, 0));
    assert_eq!(corrupt.endpoints(n, (0, 0)), Err(QueryError::Corrupt));
    m.working.docs.get_mut(&2).unwrap().vid = 1;
    let n2 = m.publish_native();
    assert!(matches!(m.read(n2, (0, 0)), Read::Pending { .. }));
    assert_eq!(m.endpoints(n2, (0, 0)), Err(QueryError::Pending));
    assert!(!m.proves_absence(n2, (0, 0)));
}

#[test]
fn exact_native_semantic_copy_freshness() {
    let mut m = Model::new(fixture(), Fault::None);
    let imported = m.import(true);
    let copied = m.copy_same_native(imported).unwrap();
    assert!(matches!(
        m.read(copied, (2, 0)),
        Read::Recorded {
            labels: Labels {
                older: false,
                effective: Freshness::Fresh,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        m.read(imported, (2, 0)),
        Read::Recorded {
            labels: Labels {
                older: false,
                effective: Freshness::Fresh,
                ..
            },
            ..
        }
    ));
    m.working.contexts.get_mut(&(1, 1)).unwrap()[0] = 1;
    let n = m.publish_native();
    assert_eq!(m.copy_same_native(copied), None);
    assert_eq!(m.copy_same_native(n), None);
    assert!(matches!(
        m.read(n, (2, 0)),
        Read::Recorded {
            labels: Labels {
                older: true,
                effective: Freshness::PossiblyStale,
                ..
            },
            ..
        }
    ));
}

#[test]
fn selection_scopes_restage_without_overwriting_explicit_import() {
    let mut m = Model::new(fixture(), Fault::None);
    m.working.docs.get_mut(&2).unwrap().vid = 1;
    let n = m.publish_native();
    let c = m.views[&n].capture.clone();
    let mut rows = recompute(&c);
    rows.get_mut(&(0, 0)).unwrap().outcome = Outcome::Evidence(Claim::External);
    let revision = m.revision;
    let p0 = m.publish_scopes(n, c.clone(), rows, &[(0, 0)]);
    assert_eq!(m.revision, revision);
    assert_eq!(m.publish_native(), p0); // semantic-only import is not a native change
    let captured = m.start().unwrap();
    m.finish(captured);
    let p1 = m.publish_scopes(n, c.clone(), recompute(&c), &[(0, 1)]);
    assert_eq!(m.commit(captured), Commit::Conflict);
    let restage = m.start().unwrap();
    m.finish(restage);
    assert_eq!(m.commit(restage), Commit::Published);
    assert_eq!(m.views[&m.head].basis[&(0, 0)], p0);
    assert_eq!(m.views[&m.head].basis[&(0, 1)], p1);
    assert!(matches!(
        m.read(m.head, (0, 0)),
        Read::Recorded {
            record: PublicRecord {
                outcome: Outcome::Evidence(Claim::External),
                ..
            },
            ..
        }
    ));
    m.working.docs.get_mut(&2).unwrap().vid = 2;
    let n2 = m.publish_native();
    assert_eq!(m.views[&n2].basis[&(0, 0)], p0);
    assert_eq!(m.views[&n2].basis[&(0, 1)], p1);
    let s2 = m.start().unwrap();
    m.finish(s2);
    assert_eq!(m.commit(s2), Commit::Published);
    assert_eq!(
        m.semantics[&m.semantic].rows[&(0, 0)].outcome,
        Outcome::Evidence(Claim::External)
    );
}

#[test]
fn same_native_distinct_import_rejects_stale_copy_and_old_stage() {
    let mut m = Model::new(fixture(), Fault::None);
    let old = m.head;
    let native = m.native;
    let rev = m.revision;
    let old_stage = m.start().unwrap();
    m.finish(old_stage);
    let capture = m.views[&native].capture.clone();
    let mut rows = m.semantic_inputs.rows.clone();
    rows.get_mut(&(0, 0)).unwrap().outcome = Outcome::Evidence(Claim::External);
    let imported = m.publish_scopes(native, capture, rows, &[(0, 0)]);
    assert_eq!(m.native, native);
    assert_eq!(m.revision, rev);
    assert_eq!(m.publish_native(), imported);
    assert_eq!(m.copy_same_native(old), None);
    assert_eq!(m.commit(old_stage), Commit::Conflict);
    let remaining = m.start().unwrap();
    m.finish(remaining);
    assert_eq!(m.commit(remaining), Commit::Published);
    m.working.docs.get_mut(&2).unwrap().vid = 1;
    m.publish_native();
    let catchup = m.start().unwrap();
    m.finish(catchup);
    assert_eq!(m.commit(catchup), Commit::Published);
    assert_eq!(
        m.semantics[&m.semantic].rows[&(0, 0)].outcome,
        Outcome::Evidence(Claim::External)
    );
}

#[test]
fn consumed_authority_is_pinned_and_missing_authority_is_unresolved() {
    let mut c = fixture();
    c.output.insert(
        (0, 0),
        record(
            Outcome::Evidence(Claim::Resolved),
            vec![edge(Kind::Consumes, 1, 1)],
        ),
    );
    c.output.insert(
        (1, 1),
        record(
            Outcome::Evidence(Claim::Resolved),
            vec![edge(Kind::RefCandidate, 2, 0)],
        ),
    );
    let mut m = Model::new(c, Fault::None);
    m.working.docs.get_mut(&2).unwrap().vid = 1;
    m.working.output.get_mut(&(1, 1)).unwrap().edges.clear();
    let n = m.publish_native();
    let c = m.views[&n].capture.clone();
    let p1 = m.publish_selected(n, c.clone(), recompute(&c), &[1]);
    assert_eq!(m.read(p1, (0, 0)), Read::UnresolvedDependency);
    let mut unbound = m.clone();
    unbound
        .semantics
        .get_mut(&1)
        .unwrap()
        .rows
        .get_mut(&(0, 0))
        .unwrap()
        .edges[0]
        .consumed_basis = None;
    assert_eq!(unbound.read(p1, (0, 0)), Read::UnresolvedDependency);
}

#[test]
fn required_context_coverage_and_external_identity() {
    let mut c = fixture();
    c.contexts.remove(&(1, 1));
    let mut ext = edge(Kind::CallDeclared, 1, 0);
    ext.external = true;
    let mut lookup = edge(Kind::Lookup, 1, 0);
    lookup.mapped = false;
    c.output.insert(
        (0, 0),
        record(
            Outcome::Evidence(Claim::Resolved),
            vec![ext, lookup, edge(Kind::Consumes, 1, 1)],
        ),
    );
    // Avoid a missing required context on the consumed selection in this fixture.
    c.output.get_mut(&(0, 0)).unwrap().edges.pop();
    c.output.get_mut(&(2, 0)).unwrap().complete = false;
    let m = Model::new(c, Fault::None);
    assert!(!m.proves_absence(1, (2, 0)));
    assert!(matches!(
        m.endpoints(1, (0, 0)).unwrap().as_slice(),
        [Endpoint::External(_)]
    ));
    assert_eq!(m.traverse(1, &[(0, 0), (1, 0)]).unwrap().len(), 1);
    let mut required = m.clone();
    required
        .semantics
        .get_mut(&1)
        .unwrap()
        .rows
        .get_mut(&(0, 0))
        .unwrap()
        .edges[0]
        .context = Some((1, 1));
    assert_eq!(required.read(1, (0, 0)), Read::Corrupt);
}

#[test]
fn query_absence_requires_complete_coverage_of_every_selection() {
    let mut c = fixture();
    for row in c.output.values_mut() {
        *row = record(Outcome::Empty, vec![]);
    }
    let m = Model::new(c, Fault::None);
    let all = [(0, 0), (0, 1), (1, 0), (1, 1), (2, 0), (2, 1)];
    assert!(m.proves_query_absence(1, &all));
    assert!(!m.proves_query_absence(1, &[]));
    assert!(!m.proves_query_absence(1, &[(2, 0), (2, 1)]));
    let mut failed = m.clone();
    failed
        .semantics
        .get_mut(&1)
        .unwrap()
        .rows
        .get_mut(&(2, 1))
        .unwrap()
        .outcome = Outcome::Failed;
    assert!(!failed.proves_query_absence(1, &all));
}

#[test]
fn traversal_keeps_source_p_authority_when_selected_target_p_changes() {
    let mut m = Model::new(fixture(), Fault::None);
    let c = m.views[&m.native].capture.clone();
    let rows = m.semantic_inputs.rows.clone();
    let equivalent = m.publish_scopes(m.native, c.clone(), rows.clone(), &[(0, 1)]);
    assert!(matches!(
        m.endpoints(equivalent, (0, 0)).unwrap().as_slice(),
        [Endpoint::Internal(_, Read::Recorded { .. })]
    ));
    assert!(matches!(
        m.traverse(equivalent, &[(0, 0), (1, 0)])
            .unwrap()
            .as_slice(),
        [Hop::Row(..), Hop::Row(..)]
    ));
    let mut changed = rows;
    changed.get_mut(&(1, 0)).unwrap().freshness = Freshness::Stale;
    let p1 = m.publish_scopes(m.native, c, changed, &[(0, 1)]);
    assert!(
        matches!(m.endpoints(p1,(0,0)).unwrap().as_slice(),[Endpoint::UnresolvedAuthority(t,Read::Recorded {labels,..})] if t.labels.basis==1 && labels.basis==p1)
    );
    assert!(
        matches!(m.traverse(p1,&[(0,0),(1,0)]).unwrap().as_slice(),[Hop::Row(..),Hop::AuthorityBoundary((1,0),_,basis)] if *basis==p1)
    );
    let mut corrupt = m.clone();
    corrupt.semantics.get_mut(&p1).unwrap().rows.remove(&(1, 0));
    assert_eq!(corrupt.endpoints(p1, (0, 0)), Err(QueryError::Corrupt));
    assert_eq!(
        corrupt.traverse(p1, &[(0, 0), (1, 0)]),
        Err(QueryError::Corrupt)
    );
    m.working.docs.get_mut(&2).unwrap().vid = 1;
    let n = m.publish_native();
    assert!(matches!(
        m.endpoints(n, (0, 0)).unwrap().as_slice(),
        [Endpoint::PendingBoundary(_, Read::Pending { .. })]
    ));
    assert!(matches!(
        m.traverse(n, &[(0, 0), (1, 0)]).unwrap().as_slice(),
        [Hop::Row(..), Hop::Boundary((1, 0), Read::Pending { .. })]
    ));
}

#[test]
fn no_op_keeps_stage_and_native_revision_distinct_from_paired_pin() {
    let mut m = Model::new(fixture(), Fault::None);
    let native = m.native;
    let revision = m.revision;
    let stage = m.start().unwrap();
    m.finish(stage);
    assert_eq!(m.publish_native(), 1);
    assert_eq!(m.native, native);
    assert_eq!(m.revision, revision);
    assert_eq!(m.commit(stage), Commit::Published);
    assert_ne!(m.head, native);
    assert_eq!(m.views[&m.head].revision, revision);
    assert_eq!(m.start(), None);
    m.working.docs.get_mut(&0).unwrap().vid = 1;
    m.publish_native();
    assert_ne!(m.revision, revision);
}

proptest! {
    #[test]
    fn histories_and_varied_synthetic_full(ops in operations()) {
        let initial = fixture(); let mut m = Model::new(initial.clone(),Fault::None); let mut tokens = Vec::new();
        for op in ops {
            let before = m.views.clone();
            match op {
                Op::Edit(d,v) => m.working.docs.entry(d).or_insert(Document { identity:d,vid:0 }).vid = v,
                Op::Add(mask,outcome) => add_document(&mut m,mask,outcome),
                Op::Delete(d) => { m.working.docs.remove(&d); },
                Op::Rename(d,v) => { if let Some(doc) = m.working.docs.get_mut(&d) { doc.identity = v; } },
                Op::Restore(d) => { m.working.docs.insert(d,initial.docs[&d].clone()); },
                Op::Context(p,s,v) => m.working.contexts.get_mut(&(p,s)).unwrap()[0] = v,
                Op::Set(k,r) => { let capture=m.views[&m.native].capture.clone(); let mut rows=m.semantic_inputs.rows.clone(); rows.insert(k,r); m.publish_scopes(m.native,capture,rows,&[(k.1,k.0 % 2)]); },
                Op::Publish => { m.publish_native(); },
                Op::Start => { if let Some(token) = m.start() { tokens.push(token); } },
                Op::Unit => { if let Some(&t) = tokens.last() { m.unit(t); } },
                Op::Commit => { if let Some(t) = tokens.pop() { let selected = m.stages.get(&t).map(|s| (s.selected.clone(),s.semantic_inputs.clone(),s.selections.clone())); if m.commit(t) == Commit::Published { let (capture, inputs, selections)=selected.unwrap(); prop_assert_eq!(&m.semantics[&m.semantic].rows, &reference_selected(&capture,&inputs,&selections)); } } },
                Op::CancelOld => { if !tokens.is_empty() { m.cancel(tokens.remove(0)); } },
                Op::Import(b) => { m.import(b); },
                Op::Read(k) => { let _ = m.read(m.head,k); },
                Op::Traverse => { let _ = m.traverse(m.head,&[(0,0),(1,0),(2,0)]); },
            }
            for (pin, view) in before { prop_assert_eq!(m.views.get(&pin),Some(&view)); }
            let view = &m.views[&m.head];
            let pending = reference_pending(&m, view);
            // The fixed-point comparator does not model explicitly captured
            // cross-basis consumed-result identity; never pretend it does.
            let comparable = (0..2).all(|p| (0..3).all(|d| m.base_for(view,(d,p)).unwrap().rows.values().all(|r| r.edges.iter().all(|e| e.kind != Kind::Consumes || e.external))));
            for d in 0..3 { for p in 0..2 {
                if comparable { prop_assert_eq!(matches!(m.read(m.head,(d,p)),Read::Pending { .. }), pending.contains(&(d,p)), "key {:?}", (d,p)); }
            }}
        }
        // Force a commit after arbitrary generated evidence/outcome interactions.
        m.working.docs.entry(0).or_insert(Document {identity:0,vid:0}).vid ^= 128;
        m.publish_native(); let selected = m.working.clone(); let t = m.start().unwrap(); let inputs=m.stages[&t].semantic_inputs.clone(); m.finish(t);
        m.working.docs.entry(2).or_insert(Document { identity:2,vid:0 }).vid ^= 1; // unpublished edit
        let incomplete=selected.required.iter().any(|key| selected.docs.contains_key(&key.0) && !inputs.rows.contains_key(key));
        if incomplete { prop_assert_eq!(m.commit(t), Commit::Incomplete); }
        else { prop_assert_eq!(m.commit(t), Commit::Published); prop_assert_eq!(&m.semantics[&m.semantic].rows, &reference_full(&selected,&inputs)); }
        let pin = m.head; let pinned = m.read(pin,(0,0));
        m.publish_native(); let stale = m.start().unwrap(); m.finish(stale); m.working.docs.get_mut(&0).unwrap().vid ^= 1; m.publish_native(); let successor = m.head;
        prop_assert_eq!(m.commit(stale),Commit::Conflict);
        prop_assert_eq!(m.head,successor);
        prop_assert_eq!(m.read(pin,(0,0)),pinned);
    }
}

#[test]
fn generated_event_and_edge_counts() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::default();
    let mut operations_count = [0usize; 15];
    let mut kinds = [0usize; 11];
    let mut outcomes = [0usize; 8];
    let mut variants = [0usize; 4]; // multi-edge rows, scoped edges, external targets, mapped lookups
    let mut added_outcomes = [0usize; 5];
    let mut added_masks = [0usize; 3];
    for _ in 0..256 {
        for op in operations().new_tree(&mut runner).unwrap().current() {
            let n = match &op {
                Op::Edit(..) => 0,
                Op::Add(mask, outcome) => {
                    added_masks[usize::from(*mask - 1)] += 1;
                    added_outcomes[usize::from(*outcome)] += 1;
                    14
                }
                Op::Delete(..) => 11,
                Op::Rename(..) => 12,
                Op::Restore(..) => 1,
                Op::Context(..) => 2,
                Op::Set(_, r) => {
                    if r.edges.len() > 1 {
                        variants[0] += 1;
                    }
                    let o = match r.outcome {
                        Outcome::Empty => 0,
                        Outcome::NotRequested => 1,
                        Outcome::Failed => 2,
                        Outcome::Evidence(Claim::Unknown) => 3,
                        Outcome::Evidence(Claim::Unresolved) => 4,
                        Outcome::Evidence(Claim::Ambiguous) => 5,
                        Outcome::Evidence(Claim::External) => 6,
                        Outcome::Evidence(Claim::Resolved) => 7,
                    };
                    outcomes[o] += 1;
                    for e in &r.edges {
                        if e.context.is_some() {
                            variants[1] += 1;
                        }
                        if e.external {
                            variants[2] += 1;
                        }
                        if e.kind == Kind::Lookup && e.mapped {
                            variants[3] += 1;
                        }
                        let k = match e.kind {
                            Kind::Symbol => 0,
                            Kind::DeclCandidate => 1,
                            Kind::RefDeclared => 2,
                            Kind::RefCandidate => 3,
                            Kind::CallDeclared => 4,
                            Kind::CallCandidate => 5,
                            Kind::Dispatch => 6,
                            Kind::TypeFrom => 7,
                            Kind::TypeTo => 8,
                            Kind::Lookup => 9,
                            Kind::Consumes => 10,
                        };
                        kinds[k] += 1;
                    }
                    3
                }
                Op::Publish => 4,
                Op::Start => 5,
                Op::Unit => 13,
                Op::Commit => 6,
                Op::CancelOld => 7,
                Op::Import(_) => 8,
                Op::Read(_) => 9,
                Op::Traverse => 10,
            };
            operations_count[n] += 1;
        }
    }
    eprintln!(
        "256 generated histories operations edit/restore/context/set/publish/start/commit/cancel/import/read/traverse/delete/rename/unit/add={operations_count:?}"
    );
    eprintln!(
        "generated edge kinds symbol/decl-candidate/ref-declared/ref-candidate/call-declared/call-candidate/dispatch/type-from/type-to/lookup/consumes={kinds:?}; outcomes empty/not-requested/failed/unknown/unresolved/ambiguous/external/resolved={outcomes:?}"
    );
    eprintln!("multi-edge rows/scoped edges/external targets/mapped lookups={variants:?}");
    eprintln!(
        "new-document outcome missing/empty/not-requested/failed/present={added_outcomes:?}; producer masks 01/10/11={added_masks:?}"
    );
    assert!(
        operations_count
            .iter()
            .chain(kinds.iter())
            .chain(outcomes.iter())
            .chain(variants.iter())
            .chain(added_outcomes.iter())
            .chain(added_masks.iter())
            .all(|n| *n > 0)
    );
}

#[test]
fn generated_bound_graph_frequencies() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::default();
    let mut counts = [0usize; 4];
    for _ in 0..128 {
        for (d, p, t, tp, consumes) in bound_graph_edges().new_tree(&mut runner).unwrap().current()
        {
            if consumes {
                counts[0] += 1;
                if p != tp {
                    counts[2] += 1;
                }
                if (d, p) == (t, tp) {
                    counts[3] += 1;
                }
            } else {
                counts[1] += 1;
            }
        }
    }
    eprintln!(
        "128 bound graph fixtures: consumed/candidate/cross-producer/self-consumption={counts:?}"
    );
    assert!(counts.iter().all(|&count| count > 0));
}
