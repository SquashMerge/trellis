#![cfg(feature = "test-support")]
//! #558: one drawn column per type or module, in every Sequence language. Each exact target
//! stays a participant: the column groups the drawing, not the evidence.
use trellis::behavior::{SequenceStep, SequenceView};
use trellis::graph_query::BoundaryReason;
use trellis::semantic_evidence as s;

use crate::graph_fixtures::{Bind, Publisher, Spec, bind, syntax_id};

fn publish(files: &[(&str, &str)], language: s::Language, bindings: Vec<Bind>) -> SequenceView {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(files);
    spec.languages = vec![language];
    spec.bindings = bindings;
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "run");
    publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap()
}

/// The column label of each participant, by the participant's own name.
fn columns(view: &SequenceView) -> Vec<(&str, &str)> {
    view.participants
        .iter()
        .map(|p| {
            (
                p.label.as_str(),
                p.column.as_ref().expect("a column").label.as_str(),
            )
        })
        .collect()
}

fn drawn(view: &SequenceView) -> usize {
    let mut ids: Vec<_> = view
        .participants
        .iter()
        .map(|p| p.column.as_ref().unwrap().id.as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids.len()
}

#[test]
fn rust_methods_share_their_impl_type_and_free_functions_their_module() {
    let source = "struct S;\nimpl S {\n    fn run(&self) { self.one(); self.two(); free(); }\n    fn one(&self) {}\n}\ntrait T { fn two(&self) {} }\nimpl T for S {}\nfn free() {}\n";
    let view = publish(
        &[("src/store/graph_read.rs", source)],
        s::Language::Rust,
        vec![
            bind("run", 0, "one"),
            bind("run", 1, "two"),
            bind("run", 2, "free"),
        ],
    );
    assert_eq!(
        columns(&view),
        [
            ("run", "S"),
            ("one", "S"),
            ("two", "T"),
            ("free", "graph_read")
        ]
    );
    assert_eq!(drawn(&view), 3);
}

#[test]
fn rust_impl_blocks_join_across_files_and_a_trait_impl_belongs_to_its_type() {
    let view = publish(
        &[
            (
                "a.rs",
                "struct S;\nimpl S {\n    fn run(&self) { self.one(); self.fmt(); }\n}\n",
            ),
            (
                "b.rs",
                "impl S {\n    fn one(&self) {}\n}\nimpl Show for crate::a::S {\n    fn fmt(&self) {}\n}\n",
            ),
        ],
        s::Language::Rust,
        vec![bind("run", 0, "one"), bind("run", 1, "fmt")],
    );
    assert_eq!(columns(&view), [("run", "S"), ("one", "S"), ("fmt", "S")]);
    assert_eq!(drawn(&view), 1);
}

/// Two types of one name stay apart: nothing here says which an `impl` implements.
#[test]
fn rust_types_that_share_a_name_are_never_conflated() {
    let view = publish(
        &[
            (
                "a.rs",
                "struct S;\nimpl S {\n    fn run(&self) { self.one(); }\n}\n",
            ),
            ("b.rs", "struct S;\nimpl S {\n    fn one(&self) {}\n}\n"),
        ],
        s::Language::Rust,
        vec![bind("run", 0, "one")],
    );
    assert_eq!(drawn(&view), 2);
    assert_eq!(columns(&view), [("run", "S (a.rs)"), ("one", "S (b.rs)")]);
}

#[test]
fn javascript_methods_share_their_class_and_functions_their_file() {
    let source = "class A {\n  run() { this.one(); free(); }\n  one() {}\n}\nfunction free() {}\n";
    let view = publish(
        &[("web/a.js", source)],
        s::Language::Javascript,
        vec![bind("run", 0, "one"), bind("run", 1, "free")],
    );
    assert_eq!(
        columns(&view),
        [("run", "A"), ("one", "A"), ("free", "a.js")]
    );
}

#[test]
fn java_methods_share_their_class_and_a_nested_class_is_its_own() {
    let source = "class A {\n  void run() { one(); new B().two(); }\n  void one() {}\n  static class B { void two() {} }\n}\n";
    let view = publish(
        &[("A.java", source)],
        s::Language::Java,
        vec![bind("run", 0, "one"), bind("run", 2, "two")],
    );
    assert_eq!(columns(&view), [("run", "A"), ("one", "A"), ("two", "B")]);
}

#[test]
fn python_methods_share_their_class_and_functions_their_module() {
    let source = "class A:\n    def run(self):\n        self.one()\n        free()\n\n    def one(self):\n        pass\n\ndef free():\n    pass\n";
    let view = publish(
        &[("pkg/util.py", source)],
        s::Language::Python,
        vec![bind("run", 0, "one"), bind("run", 1, "free")],
    );
    assert_eq!(
        columns(&view),
        [("run", "A"), ("one", "A"), ("free", "util")]
    );
}

fn calls(steps: &[SequenceStep]) -> Vec<&SequenceStep> {
    steps
        .iter()
        .flat_map(|step| std::iter::once(step).chain(calls(&step.children)))
        .collect()
}

/// #558: the 20-lane bound counts columns, so a type with more methods than that fits and
/// each call keeps its exact target; the 21st column is refused as before.
#[test]
fn the_participant_bound_counts_columns_not_methods() {
    let methods: String = (0..30).map(|n| format!("fn m{n}(&self) {{}}\n")).collect();
    let calling: String = (0..30).map(|n| format!("self.m{n}(); ")).collect();
    let source = format!("struct S;\nimpl S {{\nfn run(&self) {{ {calling} }}\n{methods}}}\n");
    let view = publish(
        &[("a.rs", &source)],
        s::Language::Rust,
        (0..30).map(|n| bind("run", n, &format!("m{n}"))).collect(),
    );
    assert_eq!(view.participants.len(), 31);
    assert_eq!(drawn(&view), 1);
    assert!(!view.truncated, "{:?}", view.warnings);
    assert!(
        calls(&view.steps)
            .iter()
            .filter(|step| step.kind == "call")
            .all(|step| step.target.is_some())
    );

    // The seed's module and 20 types: the 20th type is one column too many.
    let types: String = (0..20)
        .map(|n| format!("struct T{n};\nimpl T{n} {{ fn f{n}(&self) {{}} }}\n"))
        .collect();
    let calling: String = (0..20).map(|n| format!("T{n}.f{n}(); ")).collect();
    let source = format!("fn run() {{ {calling} }}\n{types}");
    let view = publish(
        &[("a.rs", &source)],
        s::Language::Rust,
        (0..20).map(|n| bind("run", n, &format!("f{n}"))).collect(),
    );
    assert_eq!(drawn(&view), 20);
    assert_eq!(view.participants.len(), 20);
    assert!(view.truncated);
    let refused: Vec<_> = calls(&view.steps)
        .into_iter()
        .filter(|step| {
            step.call_evidence
                .as_ref()
                .is_some_and(|evidence| evidence.boundary_reason == BoundaryReason::NodeLimit)
        })
        .collect();
    assert_eq!(refused.len(), 1);
    assert!(refused[0].target.is_none());
    let evidence = refused[0].call_evidence.as_ref().unwrap();
    assert_eq!(evidence.resolution, Some(s::Resolution::Resolved));
}

/// The syntax tier keeps its source-hint lanes: a hint is not a declaration of any type.
#[test]
fn a_syntax_view_has_no_columns() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let source = "fn run(x: X) { x.one(); }\n";
    let mut spec = Spec::new(&[("a.rs", source)]);
    spec.bindings = vec![];
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "run");
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    assert!(view.participants.len() > 1);
    assert!(view.participants.iter().all(|p| p.column.is_none()));
}
