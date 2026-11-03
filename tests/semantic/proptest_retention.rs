//! Decision 0006: independent, head-rooted, two-producer retention model.
use super::*;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use trellis::store::MaintenanceOutcome;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Js,
    Py,
}
impl Slot {
    fn producer(self) -> &'static str {
        match self {
            Self::Js => "scip-typescript",
            Self::Py => "scip-python",
        }
    }
    fn index(self) -> usize {
        match self {
            Self::Js => 0,
            Self::Py => 1,
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Import(Slot),
    Edit,
    Reselect(Slot, u8),
    Advance(u16),
    Step,
    Drain,
    Interrupted(Slot, bool),
}
fn slot() -> impl Strategy<Value = Slot> {
    prop_oneof![Just(Slot::Js), Just(Slot::Py)]
}
fn operation() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => slot().prop_map(Op::Import), 3 => Just(Op::Edit),
        4 => (slot(), any::<u8>()).prop_map(|(s, n)| Op::Reselect(s, n)),
        4 => prop_oneof![Just(1), Just(899), Just(900), Just(901)].prop_map(Op::Advance),
        3 => Just(Op::Step), 2 => Just(Op::Drain),
        2 => (slot(), any::<bool>()).prop_map(|(s, p)| Op::Interrupted(s, p)),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Head,
    Retained,
    PendingWhole,
    PendingPartial,
    Released,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    batch: usize,
    capture: usize,
}
#[derive(Clone, Debug)]
struct Revision {
    pin: IndexPin,
    state: State,
    superseded: Option<i64>,
    selected: [Option<Selection>; 2],
    epoch: usize,
}
struct Batch {
    slot: Slot,
    descriptor: (PathBuf, String),
    epoch: usize,
    id: i64,
    raw_counts: (i64, i64),
    proofs: Vec<(String, String, String, Vec<u8>)>,
}
type Rows = BTreeMap<String, (i64, String)>; // producer -> (batch, captured pin)
type Observation = BTreeMap<String, (String, Rows)>;

struct Machine {
    world: World,
    revisions: Vec<Revision>,
    batches: Vec<Batch>,
    now: i64,
    trace: Vec<String>,
}
impl Machine {
    fn new() -> Self {
        let world = world(|root| {
            write_files(
                root,
                &[
                    ("src/A.js", DEFINITIONS_A),
                    ("src/B.js", DEFINITIONS_B),
                    ("src/Q.py", "def q(): return 1\n"),
                ],
            )
        });
        world.store.set_retention_clock_for_tests(1_000, 0);
        world
            .store
            .set_maintenance_unit_budget_for_tests(std::time::Duration::ZERO);
        let pin = world.store.index_baseline().unwrap();
        Self {
            world,
            revisions: vec![Revision {
                pin,
                state: State::Head,
                superseded: None,
                selected: [None; 2],
                epoch: 0,
            }],
            batches: vec![],
            now: 1_000,
            trace: vec![],
        }
    }
    fn head(&self) -> usize {
        self.revisions.len() - 1
    }
    fn key(&self, i: usize) -> String {
        let p = self.revisions[i].pin;
        format!("pin:v1:{}:{}", p.index_generation, p.index_revision)
    }
    fn publish(&mut self, pin: IndexPin, selected: [Option<Selection>; 2], edited: bool) {
        let h = self.head();
        assert_eq!(pin.index_revision, self.revisions[h].pin.index_revision + 1);
        self.revisions[h].state = State::Retained;
        self.revisions[h].superseded = Some(self.now);
        self.revisions.push(Revision {
            pin,
            state: State::Head,
            superseded: None,
            selected,
            epoch: self.revisions[h].epoch + usize::from(edited),
        });
    }
    // Only head selections are roots. Earlier *still selected* rows are import witnesses.
    fn holds(&self) -> BTreeSet<usize> {
        let mut roots = BTreeSet::new();
        for selection in self.revisions[self.head()].selected.into_iter().flatten() {
            roots.insert(selection.capture);
            if let Some(first) = self.revisions.iter().position(|r| {
                r.selected
                    .into_iter()
                    .flatten()
                    .any(|s| s.batch == selection.batch)
            }) {
                roots.insert(first);
            }
        }
        roots
    }
    fn rows_for(&self, selected: [Option<Selection>; 2]) -> Rows {
        selected
            .into_iter()
            .enumerate()
            .filter_map(|(slot, s)| {
                s.map(|s| {
                    (
                        [Slot::Js, Slot::Py][slot].producer().to_owned(),
                        (self.batches[s.batch].id, self.key(s.capture)),
                    )
                })
            })
            .collect()
    }
    fn observation(&self) -> Observation {
        // All observations are outside production transactions: closing another SQLite
        // descriptor during one would drop this process's POSIX locks.
        let db = self.world.db();
        let mut result = BTreeMap::new();
        {
            let mut q = db.prepare("SELECT r.id,COALESCE(s.state,'head') FROM native_revisions r LEFT JOIN native_revision_supersessions s ON s.revision_id=r.id").unwrap();
            for row in q
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
            {
                let (id, state) = row.unwrap();
                result.insert(id, (state, Rows::new()));
            }
        }
        {
            let mut q = db.prepare("SELECT revision_id,producer_id,batch_id,captured_pin FROM semantic_revision_raw_selections").unwrap();
            for row in q
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })
                .unwrap()
            {
                let (revision, producer, batch, capture) = row.unwrap();
                assert!(
                    result
                        .get_mut(&revision)
                        .unwrap()
                        .1
                        .insert(producer, (batch, capture))
                        .is_none()
                );
            }
        }
        result
    }
    fn state_name(state: State) -> &'static str {
        match state {
            State::Head => "head",
            State::Retained => "retained",
            State::PendingWhole | State::PendingPartial => "pending",
            State::Released => "released",
        }
    }
    fn model_observation(&self) -> Observation {
        self.revisions
            .iter()
            .enumerate()
            .map(|(i, r)| {
                (
                    self.key(i),
                    (
                        Self::state_name(r.state).to_owned(),
                        self.rows_for(r.selected),
                    ),
                )
            })
            .collect()
    }
    fn raw_counts(&self, id: i64) -> (i64, i64) {
        self.world.db().query_row("SELECT (SELECT count(*) FROM semantic_raw_captures WHERE batch_id=?1),(SELECT count(*) FROM semantic_raw_proofs p JOIN semantic_raw_captures c ON c.capture_id=p.capture_id WHERE c.batch_id=?1)", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
    }
    fn raw_proofs(&self, id: i64) -> Vec<(String, String, String, Vec<u8>)> {
        let db = self.world.db();
        let mut q = db.prepare("SELECT p.proof_id,p.record_kind,p.fact_digest,p.canonical_bytes FROM semantic_raw_proofs p JOIN semantic_raw_captures c ON c.capture_id=p.capture_id WHERE c.batch_id=?1 ORDER BY c.ordinal,p.proof_id").unwrap();
        q.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
    fn check(&self) {
        assert_eq!(
            self.observation(),
            self.model_observation(),
            "trace {:?}",
            self.trace
        );
        let db = self.world.db();
        for (i, r) in self.revisions.iter().enumerate() {
            if let Some(stamp) = r.superseded {
                let (actual, debt): (i64, i64) = db.query_row("SELECT superseded_at,(SELECT count(*) FROM native_revision_release_debt d WHERE d.revision_id=s.revision_id) FROM native_revision_supersessions s WHERE revision_id=?1", [self.key(i)], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
                assert_eq!(actual, stamp, "{:?}", self.trace);
                assert_eq!(
                    debt,
                    i64::from(matches!(
                        r.state,
                        State::PendingWhole | State::PendingPartial
                    )),
                    "{:?}",
                    self.trace
                );
            }
            if matches!(r.state, State::Head | State::Retained) {
                assert!(
                    self.world.store.semantic_at(r.pin).is_ok(),
                    "revision {i}: {:?}",
                    self.trace
                );
            } else if r.state == State::Released {
                assert!(
                    format!("{:#}", self.world.store.semantic_at(r.pin).unwrap_err())
                        .starts_with("pin_expired"),
                    "{:?}",
                    self.trace
                );
            }
        }
        drop(db);
        for s in self
            .revisions
            .iter()
            .flat_map(|r| r.selected.into_iter().flatten())
        {
            let b = &self.batches[s.batch];
            let exists: bool = self
                .world
                .db()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM semantic_raw_batches WHERE batch_id=?1)",
                    [b.id],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "selected raw batch missing: {:?}", self.trace);
            assert_eq!(
                self.raw_counts(b.id),
                b.raw_counts,
                "selected raw evidence changed: {:?}",
                self.trace
            );
            assert_eq!(
                self.raw_proofs(b.id),
                b.proofs,
                "selected raw proofs changed: {:?}",
                self.trace
            );
        }
    }
    fn step(&mut self) -> MaintenanceOutcome {
        let holds = self.holds();
        let before = self.model_observation();
        let leader = self.world.store.publication_for_tests().unwrap();
        let permit = self.world.store.maintenance_try_enter().unwrap();
        let probe = self.world.store.open_maintenance_queue_probe().unwrap();
        let outcome = self
            .world
            .store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap();
        drop((probe, permit, leader));
        let actual = self.observation();
        // Enumerate permitted transitions from the model, then compare the entire database
        // observation. Progress may do invisible cleanup; deferred may do nothing.
        let mut candidates = vec![(None, before.clone())];
        if outcome == MaintenanceOutcome::Progress {
            for (i, r) in self.revisions.iter().enumerate() {
                let choices: Vec<(State, [Option<Selection>; 2])> = match r.state {
                    State::Retained
                        if !holds.contains(&i) && self.now - r.superseded.unwrap() >= 900 =>
                    {
                        vec![(
                            if r.selected == [None; 2] {
                                State::PendingPartial
                            } else {
                                State::PendingWhole
                            },
                            r.selected,
                        )]
                    }
                    State::PendingWhole if holds.contains(&i) => {
                        vec![(State::Retained, r.selected)]
                    }
                    State::PendingWhole | State::PendingPartial => vec![
                        (State::PendingPartial, [None; 2]),
                        (State::Released, [None; 2]),
                    ],
                    _ => vec![],
                };
                for (state, selections) in choices {
                    let mut next = before.clone();
                    next.insert(
                        self.key(i),
                        (
                            Self::state_name(state).to_owned(),
                            self.rows_for(selections),
                        ),
                    );
                    candidates.push((Some((i, state, selections)), next));
                }
            }
        }
        let transition = candidates.into_iter().find(|(_, candidate)| *candidate == actual)
            .unwrap_or_else(|| panic!("invalid maintenance transition {outcome:?}, holds {holds:?}, before {before:?}, actual {actual:?}, trace {:?}", self.trace)).0;
        if let Some((i, state, selected)) = transition {
            self.revisions[i].state = state;
            self.revisions[i].selected = selected;
        }
        if outcome == MaintenanceOutcome::Idle {
            assert!(
                self.revisions
                    .iter()
                    .enumerate()
                    .all(|(i, r)| match r.state {
                        State::PendingWhole | State::PendingPartial => false,
                        State::Retained =>
                            holds.contains(&i) || self.now - r.superseded.unwrap() < 900,
                        _ => true,
                    }),
                "idle with due work: {:?}",
                self.trace
            );
        }
        outcome
    }
    fn drain(&mut self) {
        for _ in 0..1000 {
            if self.step() == MaintenanceOutcome::Idle {
                return;
            }
        }
        panic!("maintenance did not drain: {:?}", self.trace);
    }
    fn import(&mut self, slot: Slot) {
        let h = self.head();
        let mut fixture = self.world.fixture().reference(match slot {
            Slot::Js => "scip-typescript",
            Slot::Py => "scip-python",
        });
        match slot {
            Slot::Js => {
                let (a, b) = definitions_docs();
                let a = a.occurrence(
                    &[0, 0, 1],
                    &[],
                    &format!("pkg generated{}.", self.batches.len()),
                    0,
                );
                fixture.artifact("javascript", vec![a, b]);
            }
            Slot::Py => {
                let q = DocSpec::new("src/Q.py").occurrence(
                    &[0, 0, 1],
                    &[],
                    &format!("pkg generated{}.", self.batches.len()),
                    0,
                );
                fixture.artifact("python", vec![q]);
            }
        }
        let descriptor = self.world.write(&fixture);
        self.select(slot, descriptor, h, None);
    }
    fn select(
        &mut self,
        slot: Slot,
        descriptor: (PathBuf, String),
        capture: usize,
        previous: Option<usize>,
    ) {
        let h = self.head();
        let pin = self.world.publish(&descriptor).unwrap().0;
        let key = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
        // Database IDs are opaque handles only: the logical selection and capture come from
        // the operation. A still-selected incarnation must retain the same raw identity.
        let rows = self.observation();
        let actual = &rows[&key].1;
        let id = actual[slot.producer()].0;
        let live_previous = previous.and_then(|b| {
            self.revisions
                .iter()
                .flat_map(|r| r.selected.into_iter().flatten())
                .map(|s| s.batch)
                .find(|&live| self.batches[live].descriptor == self.batches[b].descriptor)
        });
        let batch = if let Some(b) = live_previous {
            assert_eq!(
                id, self.batches[b].id,
                "raw identity changed while selected: {:?}",
                self.trace
            );
            b
        } else {
            // A collected descriptor can get a new row ID, even one SQLite has reused.
            let b = self.batches.len();
            self.batches.push(Batch {
                slot,
                descriptor,
                epoch: self.revisions[capture].epoch,
                id,
                raw_counts: self.raw_counts(id),
                proofs: self.raw_proofs(id),
            });
            b
        };
        let mut selected = self.revisions[h].selected;
        selected[slot.index()] = Some(Selection { batch, capture });
        self.publish(pin, selected, false);
    }
    fn apply(&mut self, op: Op) {
        self.trace.push(format!("{op:?}"));
        match op {
            Op::Import(s) => self.import(s),
            Op::Edit => {
                let selected = self.revisions[self.head()].selected;
                let content = format!("function other(){{ return {}; }}\n", self.revisions.len());
                write_files(&self.world.root, &[("src/C.js", &content)]);
                self.world.republish_with_rejoin();
                let pin = self.world.store.index_baseline().unwrap();
                self.publish(pin, selected, true);
            }
            Op::Reselect(slot, n) => {
                let pool: Vec<_> = self
                    .batches
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.slot == slot)
                    .map(|(i, _)| i)
                    .collect();
                if !pool.is_empty() {
                    let b = pool[usize::from(n) % pool.len()];
                    if let Some(capture) = self.revisions.iter().rposition(|r| {
                        r.epoch == self.batches[b].epoch
                            && matches!(r.state, State::Head | State::Retained)
                    }) {
                        self.select(slot, self.batches[b].descriptor.clone(), capture, Some(b));
                    }
                }
            }
            Op::Advance(n) => {
                self.now += i64::from(n);
                self.world
                    .store
                    .set_retention_clock_for_tests(self.now, (self.now - 1_000) as u64);
            }
            Op::Step => {
                self.step();
            }
            Op::Drain => self.drain(),
            Op::Interrupted(slot, partial) => {
                // Keep a second slot selected and a same-epoch capture alive at the boundary.
                let other = if slot == Slot::Js { Slot::Py } else { Slot::Js };
                self.apply(Op::Import(other));
                let pool_index = self.batches.iter().filter(|b| b.slot == slot).count();
                self.apply(Op::Import(slot));
                let import = self.head();
                self.apply(Op::Reselect(slot, pool_index as u8));
                self.apply(Op::Advance(1));
                self.apply(Op::Edit);
                self.apply(Op::Import(slot));
                self.apply(Op::Advance(899));
                for _ in 0..1000 {
                    if self.revisions[import].state == State::PendingWhole {
                        break;
                    }
                    assert_ne!(self.step(), MaintenanceOutcome::Idle, "{:?}", self.trace);
                }
                assert_eq!(
                    self.revisions[import].state,
                    State::PendingWhole,
                    "{:?}",
                    self.trace
                );
                if partial {
                    self.step();
                    assert_eq!(
                        self.revisions[import].state,
                        State::PendingPartial,
                        "{:?}",
                        self.trace
                    );
                }
                self.apply(Op::Reselect(slot, pool_index as u8));
                self.drain();
                assert_eq!(
                    self.revisions[import].state,
                    if partial {
                        State::Released
                    } else {
                        State::Retained
                    },
                    "{:?}",
                    self.trace
                );
            }
        }
        self.check();
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_RETENTION_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(8), ..ProptestConfig::default() })]
    #[test]
    fn decision_0006_multi_slot_histories(ops in prop::collection::vec(operation(), 1..15)) {
        let mut m = Machine::new();
        for op in ops { m.apply(op); }
    }
}

#[test]
fn two_slots_keep_only_head_roots_and_replacement_preserves_other() {
    let mut m = Machine::new();
    for op in [
        Op::Import(Slot::Js),
        Op::Edit,
        Op::Import(Slot::Py),
        Op::Edit,
        Op::Import(Slot::Js),
        Op::Edit,
        Op::Advance(899),
        Op::Drain,
    ] {
        m.apply(op);
    }
    assert!(
        m.revisions[..m.head()]
            .iter()
            .all(|r| r.state == State::Retained)
    );
    m.apply(Op::Advance(1));
    m.apply(Op::Drain);
    assert_eq!(m.holds(), BTreeSet::from([2, 3, 4, 5]));
    assert_eq!(
        m.revisions.iter().map(|r| r.state).collect::<Vec<_>>(),
        vec![
            State::Released,
            State::Released,
            State::Retained,
            State::Retained,
            State::Retained,
            State::Retained,
            State::Head
        ]
    );
    assert!(m.revisions[m.head()].selected.iter().all(Option::is_some));
    for selection in m.revisions[m.head()].selected.into_iter().flatten() {
        assert!(
            !m.batches[selection.batch].proofs.is_empty(),
            "both slots must exercise raw proofs"
        );
    }
    assert_eq!(m.revisions[3].selected[0].unwrap().batch, 0); // incidental old JS
}

#[test]
fn whole_pending_reselection_two_slots() {
    Machine::new().apply(Op::Interrupted(Slot::Js, false));
}
#[test]
fn partial_pending_reselection_two_slots() {
    Machine::new().apply(Op::Interrupted(Slot::Py, true));
}

#[test]
fn reimport_after_collection_then_reselect_uses_live_incarnation() {
    let mut m = Machine::new();
    for op in [
        Op::Import(Slot::Py),
        Op::Import(Slot::Js),
        Op::Edit,
        Op::Import(Slot::Js),
        Op::Edit,
        Op::Import(Slot::Js),
        Op::Advance(900),
        Op::Drain,
    ] {
        m.apply(op);
    }
    let old = 1;
    assert!(
        !m.revisions
            .iter()
            .any(|r| r.selected.into_iter().flatten().any(|s| s.batch == old))
    );
    m.apply(Op::Reselect(Slot::Js, 0));
    let reincarnation = m.revisions[m.head()].selected[0].unwrap().batch;
    assert_ne!(old, reincarnation);
    m.apply(Op::Import(Slot::Js));
    m.apply(Op::Reselect(Slot::Js, 0));
    assert_eq!(
        m.revisions[m.head()].selected[0].unwrap().batch,
        reincarnation
    );
    assert!(m.revisions[m.head()].selected[1].is_some());
}
