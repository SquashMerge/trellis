//! Decision 0006 discovery model. Expected holds come from publication history, not the store.
use super::*;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use trellis::store::MaintenanceOutcome;

#[derive(Clone, Debug)]
enum Op {
    Import,
    Edit,
    Reselect(u8),
    Advance(u16),
    Step,
    Drain,
    InterruptedReselect(bool),
}

fn operation() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => Just(Op::Import), 3 => Just(Op::Edit),
        3 => any::<u8>().prop_map(Op::Reselect),
        3 => prop_oneof![Just(1), Just(899), Just(900), Just(901)].prop_map(Op::Advance),
        3 => Just(Op::Step), 2 => Just(Op::Drain),
        2 => any::<bool>().prop_map(Op::InterruptedReselect),
    ]
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Head,
    Retained,
    PendingWhole,
    PendingPartial,
    Released,
}

#[derive(Clone, Debug)]
struct Revision {
    pin: IndexPin,
    state: State,
    superseded: Option<i64>,
    selected: Option<usize>,
    captured: Option<usize>,
    epoch: usize,
}

struct Batch {
    descriptor: (PathBuf, String),
    epoch: usize,
    db_id: i64,
    raw_counts: (i64, i64),
}

struct Machine {
    world: World,
    revisions: Vec<Revision>,
    batches: Vec<Batch>,
    now: i64,
    trace: Vec<String>,
}

impl Machine {
    fn new() -> Self {
        let world = definitions_world();
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
                selected: None,
                captured: None,
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

    fn publish(
        &mut self,
        pin: IndexPin,
        selected: Option<usize>,
        captured: Option<usize>,
        edited: bool,
    ) {
        let last = self.head();
        assert_eq!(
            pin.index_revision,
            self.revisions[last].pin.index_revision + 1
        );
        self.revisions[last].state = State::Retained;
        self.revisions[last].superseded = Some(self.now);
        self.revisions.push(Revision {
            pin,
            state: State::Head,
            superseded: None,
            selected,
            captured,
            epoch: self.revisions[last].epoch + usize::from(edited),
        });
    }

    // Calculate the head-rooted holds directly from recorded logical selections.
    fn holds(&self) -> BTreeSet<usize> {
        let mut holds = BTreeSet::new();
        if let Some(batch) = self.revisions[self.head()].selected {
            holds.insert(self.revisions[self.head()].captured.unwrap());
            if let Some(first) = self
                .revisions
                .iter()
                .position(|r| r.selected == Some(batch))
            {
                holds.insert(first);
            }
        }
        holds
    }

    fn step(&mut self) -> MaintenanceOutcome {
        let before = self.revisions.clone();
        let holds = self.holds();
        let leader = self.world.store.publication_for_tests().unwrap();
        let permit = self.world.store.maintenance_try_enter().unwrap();
        let probe = self.world.store.open_maintenance_queue_probe().unwrap();
        let outcome = self
            .world
            .store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap();
        drop((probe, permit, leader));
        let due = before.iter().enumerate().any(|(i, r)| match r.state {
            State::PendingWhole | State::PendingPartial => true,
            State::Retained => !holds.contains(&i) && self.now - r.superseded.unwrap() >= 900,
            _ => false,
        });
        if outcome == MaintenanceOutcome::Progress {
            assert!(due, "progress without eligible work: {:?}", self.trace);
        }
        let observed = self.observed();
        let changed: Vec<_> = (0..self.head())
            .filter(|&i| {
                let actual = &observed[&self.key(i)];
                let state = match before[i].state {
                    State::Retained => "retained",
                    State::PendingWhole | State::PendingPartial => "pending",
                    State::Released => "released",
                    State::Head => unreachable!(),
                };
                actual.0 != state || actual.1 != before[i].selected.map(|b| self.batches[b].db_id)
            })
            .collect();
        if matches!(
            outcome,
            MaintenanceOutcome::Deferred | MaintenanceOutcome::Idle
        ) {
            assert!(
                changed.is_empty(),
                "non-progress step changed state: {:?}",
                self.trace
            );
        }
        assert!(
            changed.len() <= 1,
            "step changed multiple revisions: {:?}",
            self.trace
        );
        if let Some(&i) = changed.first() {
            let (state, selected) = &observed[&self.key(i)];
            match before[i].state {
                State::Retained => {
                    assert_eq!(state, "pending", "{:?}", self.trace);
                    assert!(
                        !holds.contains(&i) && self.now - before[i].superseded.unwrap() >= 900,
                        "released held or unexpired revision {i}: {:?}",
                        self.trace
                    );
                    assert_eq!(*selected, before[i].selected.map(|b| self.batches[b].db_id));
                    self.revisions[i].state = State::PendingWhole;
                }
                State::PendingWhole if state == "retained" => {
                    assert!(
                        holds.contains(&i) && before[i].selected.is_some(),
                        "unexpected rehold: {:?}",
                        self.trace
                    );
                    self.revisions[i].state = State::Retained;
                }
                State::PendingWhole | State::PendingPartial => {
                    assert!(
                        !(holds.contains(&i)
                            && before[i].state == State::PendingWhole
                            && before[i].selected.is_some()),
                        "whole reheld revision released: {:?}",
                        self.trace
                    );
                    assert!(selected.is_none());
                    self.revisions[i].selected = None;
                    self.revisions[i].captured = None;
                    self.revisions[i].state = if state == "released" {
                        State::Released
                    } else {
                        State::PendingPartial
                    };
                }
                _ => panic!("invalid transition: {:?}", self.trace),
            }
        }
        if matches!(outcome, MaintenanceOutcome::Idle) {
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
                "idle with due work: {:?}, {:?}",
                self.trace,
                self.revisions
            );
        }
        outcome
    }

    fn key(&self, i: usize) -> String {
        let pin = self.revisions[i].pin;
        format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision)
    }

    fn observed(&self) -> BTreeMap<String, (String, Option<i64>)> {
        let db = self.world.db();
        let mut q = db.prepare("SELECT r.id,COALESCE(s.state,'head'),(SELECT batch_id FROM semantic_revision_raw_selections WHERE revision_id=r.id) FROM native_revisions r LEFT JOIN native_revision_supersessions s ON s.revision_id=r.id").unwrap();
        q.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn check(&self) {
        let observed = self.observed();
        assert_eq!(observed.len(), self.revisions.len(), "{:?}", self.trace);
        for (i, r) in self.revisions.iter().enumerate() {
            let state = match r.state {
                State::Head => "head",
                State::Retained => "retained",
                State::PendingWhole | State::PendingPartial => "pending",
                State::Released => "released",
            };
            assert_eq!(
                observed[&self.key(i)],
                (state.to_owned(), r.selected.map(|b| self.batches[b].db_id)),
                "revision {i}, model {:?}, trace {:?}",
                self.revisions,
                self.trace
            );
            if let Some(capture) = r.captured {
                let actual: String = self.world.db().query_row("SELECT captured_pin FROM semantic_revision_raw_selections WHERE revision_id=?1", [self.key(i)], |row| row.get(0)).unwrap();
                assert_eq!(
                    actual,
                    self.key(capture),
                    "captured pin, trace {:?}",
                    self.trace
                );
            }
            if let Some(stamp) = r.superseded {
                let (actual_stamp, debt): (i64, i64) = self.world.db().query_row(
                    "SELECT superseded_at,(SELECT count(*) FROM native_revision_release_debt d WHERE d.revision_id=s.revision_id) FROM native_revision_supersessions s WHERE revision_id=?1",
                    [self.key(i)], |row| Ok((row.get(0)?, row.get(1)?)),
                ).unwrap();
                assert_eq!(actual_stamp, stamp, "trace {:?}", self.trace);
                assert_eq!(
                    debt,
                    i64::from(matches!(
                        r.state,
                        State::PendingWhole | State::PendingPartial
                    )),
                    "trace {:?}",
                    self.trace
                );
            }
            if matches!(r.state, State::Head | State::Retained) {
                assert!(
                    self.world.store.semantic_at(r.pin).is_ok(),
                    "read revision {i}: {:?}",
                    self.trace
                );
            }
            if r.state == State::Released {
                assert!(
                    format!("{:#}", self.world.store.semantic_at(r.pin).unwrap_err())
                        .starts_with("pin_expired"),
                    "{:?}",
                    self.trace
                );
            }
        }
        let db = self.world.db();
        for r in &self.revisions {
            if let Some(b) = r.selected {
                let exists: bool = db
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM semantic_raw_batches WHERE batch_id=?1)",
                        [self.batches[b].db_id],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(exists, "selected raw batch lost: {:?}", self.trace);
                assert_eq!(
                    self.raw_counts(self.batches[b].db_id),
                    self.batches[b].raw_counts,
                    "selected raw inventory changed: {:?}",
                    self.trace
                );
            }
        }
    }

    fn raw_counts(&self, id: i64) -> (i64, i64) {
        self.world.db().query_row("SELECT (SELECT count(*) FROM semantic_raw_captures WHERE batch_id=?1),(SELECT count(*) FROM semantic_raw_proofs p JOIN semantic_raw_captures c ON c.capture_id=p.capture_id WHERE c.batch_id=?1)", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
    }

    fn drain(&mut self) {
        for _ in 0..1_000 {
            match self.step() {
                MaintenanceOutcome::Idle => return,
                MaintenanceOutcome::Progress | MaintenanceOutcome::Deferred => {}
            }
        }
        panic!("maintenance did not reach idle: {:?}", self.trace);
    }

    fn apply(&mut self, op: Op) {
        self.trace.push(format!("{op:?}"));
        match op {
            Op::Import => {
                let captured = self.head();
                let (a, b) = definitions_docs();
                let a = a.occurrence(
                    &[0, 0, 1],
                    &[],
                    &format!("pkg generated{}.", self.batches.len()),
                    0,
                );
                let mut fixture = self.world.fixture().reference("scip-typescript");
                fixture.artifact("javascript", vec![a, b]);
                let descriptor = self.world.write(&fixture);
                let pin = self.world.publish(&descriptor).unwrap().0;
                let id: i64 = self.world.db().query_row("SELECT batch_id FROM semantic_revision_raw_selections WHERE revision_id=?1", [format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision)], |r| r.get(0)).unwrap();
                let index = self.batches.len();
                self.batches.push(Batch {
                    descriptor,
                    epoch: self.revisions[captured].epoch,
                    db_id: id,
                    raw_counts: self.raw_counts(id),
                });
                self.publish(pin, Some(index), Some(captured), false);
            }
            Op::Edit => {
                let selected = self.revisions[self.head()].selected;
                let captured = self.revisions[self.head()].captured;
                let content = format!("function other(){{ return {}; }}\n", self.revisions.len());
                write_files(&self.world.root, &[("src/C.js", &content)]);
                self.world.republish_with_rejoin();
                let pin = self.world.store.index_baseline().unwrap();
                self.publish(pin, selected, captured, true);
            }
            Op::Reselect(n) if !self.batches.is_empty() => {
                let b = n as usize % self.batches.len();
                let capture = self.revisions.iter().rposition(|r| {
                    r.epoch == self.batches[b].epoch
                        && matches!(r.state, State::Head | State::Retained)
                });
                if let Some(capture) = capture {
                    let pin = self.world.publish(&self.batches[b].descriptor).unwrap().0;
                    // The same descriptor can be imported again after its raw rows were collected.
                    let id = self.world.db().query_row("SELECT batch_id FROM semantic_revision_raw_selections WHERE revision_id=?1", [format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision)], |r| r.get(0)).unwrap();
                    let selected = if id == self.batches[b].db_id {
                        b
                    } else {
                        let new = self.batches.len();
                        self.batches.push(Batch {
                            descriptor: self.batches[b].descriptor.clone(),
                            epoch: self.batches[b].epoch,
                            db_id: id,
                            raw_counts: self.raw_counts(id),
                        });
                        new
                    };
                    self.publish(pin, Some(selected), Some(capture), false);
                }
            }
            Op::Advance(seconds) => {
                self.now += i64::from(seconds);
                self.world
                    .store
                    .set_retention_clock_for_tests(self.now, (self.now - 1_000) as u64);
            }
            Op::Step => {
                self.step();
            }
            Op::Drain => self.drain(),
            Op::InterruptedReselect(partial) => {
                // Construct the interaction inside arbitrary generated prefixes: an extra
                // selection keeps a compatible capture available while the import is pending.
                let batch = self.batches.len();
                self.apply(Op::Import);
                let import = self.head();
                self.apply(Op::Reselect(batch as u8));
                assert_eq!(
                    self.revisions[self.head()].selected,
                    Some(batch),
                    "initial reselect: {:?}",
                    self.trace
                );
                // Let the import expire while the later compatible capture still has grace.
                self.apply(Op::Advance(1));
                self.apply(Op::Edit);
                self.apply(Op::Import);
                self.apply(Op::Advance(899));
                for _ in 0..1_000 {
                    if self.revisions[import].state == State::PendingWhole {
                        break;
                    }
                    assert_ne!(
                        self.step(),
                        MaintenanceOutcome::Idle,
                        "import never became pending: {:?}",
                        self.trace
                    );
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
                self.apply(Op::Reselect(batch as u8));
                assert_eq!(
                    self.revisions[self.head()].selected,
                    Some(batch),
                    "{:?}",
                    self.trace
                );
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
            Op::Reselect(_) => {}
        }
        self.check();
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_RETENTION_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(8), ..ProptestConfig::default() })]
    #[test]
    fn decision_0006_histories(ops in prop::collection::vec(operation(), 1..15)) {
        let mut machine = Machine::new();
        for op in ops { machine.apply(op); }
    }
}

#[test]
fn head_holds_and_no_chain_witness() {
    let mut m = Machine::new();
    for op in [
        Op::Import,
        Op::Edit,
        Op::Import,
        Op::Edit,
        Op::Advance(901),
        Op::Drain,
    ] {
        m.apply(op);
    }
    assert_eq!(m.holds(), BTreeSet::from([2, 3]));
    assert_eq!(
        m.revisions.iter().map(|r| &r.state).collect::<Vec<_>>(),
        vec![
            &State::Released,
            &State::Released,
            &State::Retained,
            &State::Retained,
            &State::Head
        ]
    );
}

// Re-selecting the first batch before its release deletes selections must cancel the
// pending debt; after that first chunk it must finish instead. The second selection
// of batch zero provides a still-retained capture of the same native revision.
fn pending_reselection_witness(partial: bool) {
    let mut m = Machine::new();
    for op in [
        Op::Import,
        Op::Reselect(0),
        Op::Edit,
        Op::Import,
        Op::Advance(901),
    ] {
        m.apply(op);
    }
    for _ in 0..200 {
        if m.revisions[1].state == State::PendingWhole {
            break;
        }
        m.apply(Op::Step);
    }
    assert_eq!(m.revisions[1].state, State::PendingWhole, "{:?}", m.trace);
    if partial {
        m.apply(Op::Step);
        assert_eq!(m.revisions[1].state, State::PendingPartial, "{:?}", m.trace);
    }
    m.apply(Op::Reselect(0));
    assert_eq!(m.revisions[m.head()].selected, Some(0));
    m.apply(Op::Drain);
    assert_eq!(
        m.revisions[1].state,
        if partial {
            State::Released
        } else {
            State::Retained
        },
        "{:?}",
        m.trace
    );
}

#[test]
fn whole_pending_reselection_witness() {
    pending_reselection_witness(false);
}

#[test]
fn partial_pending_reselection_witness() {
    pending_reselection_witness(true);
}
