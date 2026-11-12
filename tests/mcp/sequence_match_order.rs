#![cfg(feature = "test-support")]
//! #907: the shape of `DaemonState::enqueue_generated_semantic_import`. A same-column helper
//! (`lease`, like `admission_lease`) expands a body whose `match` wakes the inbox in one arm;
//! then the seed matches on a store call's `Result` and wakes the inbox in its `Err` arm. The
//! DTO keeps evaluation order: the scrutinee's argument, then the scrutinee, then the match
//! with its arms as alternates. The Interactions frames are drawn by the browser from it.
use trellis::behavior::{SequenceStep, SequenceView};
use trellis::semantic_evidence as s;

use crate::graph_fixtures::{Publisher, Spec, bind, syntax_id};

const RUN: &str = "use crate::store::Store;\nuse crate::inbox::Inbox;\n\
pub struct Daemon { store: Store, inbox: Inbox }\nimpl Daemon {\n\
    fn lease(&self, retiring: bool) -> Result<u8, ()> {\n        match retiring {\n            true => self.inbox.wake(),\n            false => {}\n        }\n        Ok(0)\n    }\n\
    fn run(&self) -> Result<u8, ()> {\n        let lease = self.lease(true)?;\n\
        let row = match self.store.enqueue(self.store.root()) {\n            Ok(row) => row,\n\
            Err(error) => {\n                drop(lease);\n                self.inbox.wake();\n                return Err(error);\n            }\n        };\n\
        drop(lease);\n        self.inbox.wake();\n        Ok(row)\n    }\n}\n";
const STORE: &str = "pub struct Store;\nimpl Store {\n    pub fn root(&self) -> u8 { 0 }\n    pub fn enqueue(&self, root: u8) -> Result<u8, ()> { Ok(root) }\n}\n";
const INBOX: &str = "pub struct Inbox;\nimpl Inbox {\n    pub fn wake(&self) {}\n}\n";

fn interactions() -> SequenceView {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[
        ("src/lib.rs", RUN),
        ("src/store.rs", STORE),
        ("src/inbox.rs", INBOX),
    ]);
    spec.languages = vec![s::Language::Rust];
    // `run`'s calls in source order: lease, enqueue, root, drop, wake, Err, drop, wake, Ok.
    spec.bindings = vec![
        bind("lease", 0, "wake"),
        bind("run", 0, "lease"),
        bind("run", 1, "enqueue"),
        bind("run", 2, "root"),
        bind("run", 4, "wake"),
        bind("run", 7, "wake"),
    ];
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "run");
    publisher
        .store
        .sequence_interactions_at(&seed, pin)
        .unwrap()
        .unwrap()
}

/// Each step in pre-order (children, then alternate), as `kind label`, indented by depth.
fn outline(steps: &[SequenceStep], depth: usize, out: &mut Vec<String>) {
    for step in steps {
        let label = step.label.split(':').next().unwrap();
        out.push(format!("{}{} {label}", "  ".repeat(depth), step.kind));
        outline(&step.children, depth + 1, out);
        outline(&step.alternate, depth + 1, out);
    }
}

#[test]
fn a_match_on_a_calls_result_follows_the_call_and_holds_its_arms() {
    let view = interactions();
    let mut steps = vec![];
    outline(&view.steps, 0, &mut steps);
    assert_eq!(
        steps,
        [
            "call lease",
            "  branch match",
            "    branch only if no earlier arm selected and pattern matches",
            "      call wake",
            "    branch only if no earlier arm selected and pattern matches",
            "  call Ok",
            "call root",
            "call enqueue",
            "  call Ok",
            "branch match",
            "  branch only if no earlier arm selected and pattern matches",
            "  branch only if no earlier arm selected and pattern matches",
            "    call drop",
            "    call wake",
            "    call Err",
            "    return return Err(error)",
            "call drop",
            "call wake",
            "call Ok",
        ],
        "{:#?}",
        view.steps
    );
    // The helper's wake runs on the helper's lane, inside its expanded body; the seed's two
    // wakes are its own. All three target the same declaration.
    let lease = &view.steps[0];
    let helper_wake = &lease.children[0].alternate[0].children[0];
    assert_eq!(helper_wake.lane, lease.target);
    let err_wake = &view.steps[3].alternate[1].children[1];
    assert_eq!(err_wake.lane, None);
    assert!(helper_wake.target.is_some());
    assert_eq!(helper_wake.target, err_wake.target);
    assert_eq!(view.steps[5].target, err_wake.target);
}
