#![cfg(feature = "test-support")]
//! #543: trivial accessors (by their target's measured body, only with fresh exact evidence)
//! and logging calls are incidental in every Sequence language: hidden by default with a
//! labelled reason, never expanded, and revealed by Show all.
use trellis::behavior::{IncidentalReason, SequenceStep, SequenceView};
use trellis::semantic_evidence as s;

use crate::graph_fixtures::{Publisher, Spec, bind, syntax_id};

fn flatten(steps: &[SequenceStep]) -> Vec<&SequenceStep> {
    steps
        .iter()
        .flat_map(|step| {
            std::iter::once(step)
                .chain(flatten(&step.children))
                .chain(flatten(&step.alternate))
        })
        .collect()
}

/// The seed's own step whose source text starts with `text`.
fn step<'v>(view: &'v SequenceView, source: &str, text: &str) -> &'v SequenceStep {
    view.steps
        .iter()
        .find(|step| source[step.range.start_byte..step.range.end_byte].starts_with(text))
        .unwrap_or_else(|| panic!("no step for {text}: {view:#?}"))
}

struct Case {
    language: s::Language,
    path: &'static str,
    source: &'static str,
    getter: &'static str,
    setter: &'static str,
    compute: &'static str,
    /// The call texts, in source order, of the getter, setter, non-trivial getter,
    /// unresolved `getY()`-style call and each logging form.
    getter_call: &'static str,
    setter_call: &'static str,
    compute_call: &'static str,
    unresolved_call: &'static str,
    logging: &'static [&'static str],
}

fn check(case: Case) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[(case.path, case.source)]);
    spec.languages = vec![case.language];
    spec.bindings = vec![
        bind("run", 0, case.getter),
        bind("run", 1, case.setter),
        bind("run", 2, case.compute),
    ];
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "run");
    let compute = syntax_id(publisher.native(), case.compute);
    let lang = case.path;
    for show_all in [false, true] {
        let view = publisher
            .store
            .sequence_at(&seed, pin, show_all)
            .unwrap()
            .unwrap();
        for text in [case.getter_call, case.setter_call] {
            let accessor = step(&view, case.source, text);
            assert_eq!(
                accessor.incidental,
                Some(IncidentalReason::Accessor),
                "{lang} {text}"
            );
            assert_eq!(accessor.hidden, !show_all, "{lang} {text}");
            assert!(
                accessor.children.is_empty(),
                "{lang} {text}: never expanded"
            );
        }
        let computed = step(&view, case.source, case.compute_call);
        assert_eq!(computed.target.as_deref(), Some(compute.as_str()), "{lang}");
        assert!(!computed.hidden && computed.incidental.is_none(), "{lang}");
        assert!(
            !computed.children.is_empty(),
            "{lang}: a non-trivial getter expands"
        );
        let unresolved = step(&view, case.source, case.unresolved_call);
        assert!(
            !unresolved.hidden && unresolved.incidental.is_none(),
            "{lang}: name alone never qualifies"
        );
        for text in case.logging {
            let logging = step(&view, case.source, text);
            assert_eq!(
                logging.incidental,
                Some(IncidentalReason::Logging),
                "{lang} {text}"
            );
            assert_eq!(logging.hidden, !show_all, "{lang} {text}");
        }
        // Receiver and argument steps (Python attribute lookups) count with their call.
        let incidental = flatten(&view.steps)
            .iter()
            .filter(|step| step.incidental.is_some())
            .count();
        assert!(incidental >= 2 + case.logging.len(), "{lang}");
        assert_eq!(view.incidental_steps, incidental, "{lang}");
        assert_eq!(
            view.hidden_steps,
            if show_all { 0 } else { incidental },
            "{lang}"
        );
        assert_eq!(
            flatten(&view.steps)
                .iter()
                .filter(|step| step.hidden)
                .count(),
            view.hidden_steps,
            "{lang}"
        );
    }
}

#[test]
fn java_accessors_and_logging_are_incidental() {
    check(Case {
        language: s::Language::Java,
        path: "S.java",
        source: "class S {\n  private int x;\n  int getX() { return this.x; }\n  void setX(int v) { this.x = v; }\n  int compute() { return helper(); }\n  int helper() { return 1; }\n  void run() { getX(); setX(1); compute(); getY(); LOG.info(\"x\"); logger.debug(\"y\"); }\n}\n",
        getter: "getX",
        setter: "setX",
        compute: "compute",
        getter_call: "getX()",
        setter_call: "setX(1)",
        compute_call: "compute()",
        unresolved_call: "getY()",
        logging: &["LOG.info", "logger.debug"],
    });
}

#[test]
fn rust_accessors_and_logging_are_incidental() {
    check(Case {
        language: s::Language::Rust,
        path: "a.rs",
        source: "struct S { x: u8 }\nimpl S {\n    fn get_x(&self) -> &u8 { &self.x }\n    fn set_x(&mut self, v: u8) { self.x = v; }\n    fn compute(&self) -> u8 { helper() }\n    fn run(&mut self) { self.get_x(); self.set_x(1); self.compute(); get_y(); log::info!(\"x\"); debug!(\"y\"); self.log.warn(\"z\"); }\n}\nfn helper() -> u8 { 1 }\n",
        getter: "get_x",
        setter: "set_x",
        compute: "compute",
        getter_call: "self.get_x()",
        setter_call: "self.set_x(1)",
        compute_call: "self.compute()",
        unresolved_call: "get_y()",
        logging: &["log::info!", "debug!", "self.log.warn"],
    });
}

#[test]
fn python_accessors_and_logging_are_incidental() {
    check(Case {
        language: s::Language::Python,
        path: "a.py",
        source: "def helper():\n    return 1\n\nclass S:\n    def get_x(self):\n        \"\"\"The x.\"\"\"\n        return self.x\n\n    def set_x(self, v):\n        self.x = v\n\n    def compute(self):\n        return helper()\n\n    def run(self):\n        self.get_x()\n        self.set_x(1)\n        self.compute()\n        get_y()\n        self.logger.warning(\"x\")\n        log.info(\"y\")\n",
        getter: "get_x",
        setter: "set_x",
        compute: "compute",
        getter_call: "self.get_x()",
        setter_call: "self.set_x(1)",
        compute_call: "self.compute()",
        unresolved_call: "get_y()",
        logging: &["self.logger.warning", "log.info"],
    });
}

#[test]
fn javascript_accessors_and_logging_are_incidental() {
    check(Case {
        language: s::Language::Javascript,
        path: "a.js",
        source: "function helper() { return 1; }\nclass S {\n  getX() { return this.x; }\n  setX(v) { this.x = v; }\n  compute() { return helper(); }\n  run() { this.getX(); this.setX(1); this.compute(); getY(); console.info(\"x\"); this.logger.error(\"y\"); }\n}\n",
        getter: "getX",
        setter: "setX",
        compute: "compute",
        getter_call: "this.getX()",
        setter_call: "this.setX(1)",
        compute_call: "this.compute()",
        unresolved_call: "getY()",
        logging: &["console.info", "this.logger.error"],
    });
}

/// Review follow-ups: more logging spellings match (constructors and compound assignment
/// are unit-tested in `behavior_incidental`), and an operand container is hidden whole.
#[test]
fn more_logging_spellings_and_operand_containers() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let source = "class S {\n  void run() { LOGGER.warn(\"a\"); this.logger.error(\"b\"); }\n}\n";
    let mut spec = Spec::new(&[("S.java", source)]);
    spec.languages = vec![s::Language::Java];
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "run");
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    for text in ["LOGGER.warn", "this.logger.error"] {
        assert_eq!(
            step(&view, source, text).incidental,
            Some(IncidentalReason::Logging),
            "{text}"
        );
    }

    // A JavaScript ternary in a logging call's arguments is hidden whole and counted.
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let source = "function f() {}\nfunction g() {}\nfunction run(c) { console.log(c ? f() : g()); console?.warn(1); work(); }\n";
    let mut spec = Spec::new(&[("a.js", source)]);
    spec.languages = vec![s::Language::Javascript];
    spec.bindings = vec![bind("run", 1, "f"), bind("run", 2, "g")];
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "run");
    for show_all in [false, true] {
        let view = publisher
            .store
            .sequence_at(&seed, pin, show_all)
            .unwrap()
            .unwrap();
        let logged: Vec<_> = flatten(&view.steps)
            .into_iter()
            .filter(|s| s.label != "work")
            .collect();
        let labels: Vec<_> = logged
            .iter()
            .map(|s| (s.kind.as_str(), s.label.as_str()))
            .collect();
        assert!(
            logged.iter().any(|s| s.label == "f") && logged.iter().any(|s| s.label == "g"),
            "{labels:?}"
        );
        assert!(
            logged
                .iter()
                .all(|s| s.incidental == Some(IncidentalReason::Logging) && s.hidden == !show_all),
            "{view:#?}"
        );
        assert!(
            logged.iter().all(|s| s.lane.is_none()),
            "nothing is expanded beneath a logging call"
        );
        assert_eq!(view.incidental_steps, logged.len());
        assert_eq!(view.hidden_steps, if show_all { 0 } else { logged.len() });
    }
}

/// #547: hidden incidental steps take no lane, so accessor-heavy code stays under the
/// 20-lane cap and its real callee still gets a lane and expands. Show all, which shows the
/// accessors, gives them lanes up to the cap.
#[test]
fn hidden_accessors_take_no_lanes() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let getters: String = (0..25)
        .map(|n| format!("  int getF{n}() {{ return this.f{n}; }}\n"))
        .collect();
    let calls: String = (0..25).map(|n| format!("getF{n}(); ")).collect();
    let source = format!(
        "class S {{\n{getters}  int compute() {{ return helper(); }}\n  int helper() {{ return 1; }}\n  void run() {{ {calls}compute(); }}\n}}\n"
    );
    let mut spec = Spec::new(&[("S.java", &source)]);
    spec.languages = vec![s::Language::Java];
    spec.bindings = (0..25)
        .map(|n| bind("run", n, &format!("getF{n}")))
        .chain([bind("run", 25, "compute")])
        .collect();
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "run");
    let compute = syntax_id(publisher.native(), "compute");
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    assert_eq!(view.participants.len(), 2, "the seed and compute only");
    assert!(!view.truncated, "{:?}", view.warnings);
    let hidden: Vec<_> = flatten(&view.steps)
        .into_iter()
        .filter(|step| step.hidden)
        .collect();
    assert_eq!(hidden.len(), 25);
    assert!(hidden.iter().all(|step| {
        step.target.is_none()
            && step.incidental == Some(IncidentalReason::Accessor)
            && step
                .call_evidence
                .as_ref()
                .is_some_and(|e| e.boundary_reason == trellis::graph_query::BoundaryReason::None)
    }));
    let computed = step(&view, &source, "compute()");
    assert_eq!(computed.target.as_deref(), Some(compute.as_str()));
    assert!(!computed.children.is_empty());

    let all = publisher
        .store
        .sequence_at(&seed, pin, true)
        .unwrap()
        .unwrap();
    // Shown accessors are targets like any other; all of them are in class S's column (#558).
    assert_eq!(all.participants.len(), 27);
    assert!(all.truncated_by.is_empty(), "{:?}", all.truncated_by);
}

/// #547: Java's bare-name getter must return a field of its class (native declarations),
/// never a parameter: an identity method is not an accessor.
#[test]
fn java_bare_name_getter_must_return_a_field() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let source = "class S {\n  private int x;\n  int getX() { return x; }\n  int id(int v) { return v; }\n  void run() { getX(); id(1); }\n}\n";
    let mut spec = Spec::new(&[("S.java", source)]);
    spec.languages = vec![s::Language::Java];
    spec.bindings = vec![bind("run", 0, "getX"), bind("run", 1, "id")];
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "run");
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    assert_eq!(
        step(&view, source, "getX()").incidental,
        Some(IncidentalReason::Accessor)
    );
    let id = step(&view, source, "id(1)");
    assert!(id.incidental.is_none() && !id.hidden);
}

/// Publishes `source` at `path` in `language` with `bindings` and returns the view of
/// `seed` in both show-all modes.
fn publish_views(
    path: &str,
    source: &str,
    language: s::Language,
    bindings: Vec<crate::graph_fixtures::Bind>,
    seed: &str,
) -> [SequenceView; 2] {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[(path, source)]);
    spec.languages = vec![language];
    spec.bindings = bindings;
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), seed);
    let view = |show_all| {
        publisher
            .store
            .sequence_at(&seed, pin, show_all)
            .unwrap()
            .unwrap()
    };
    [view(false), view(true)]
}

/// #547 review: an f-string executes, so it is no docstring: a getter that starts with one
/// is not trivial, stays visible and expands; a varargs parameter shadows a same-named field.
#[test]
fn python_fstring_and_java_varargs_are_not_accessors() {
    let source = "def effect():\n    return 1\n\nclass S:\n    def get_x(self):\n        f\"{effect()}\"\n        return self.x\n\n    def run(self):\n        self.get_x()\n";
    let views = publish_views(
        "a.py",
        source,
        s::Language::Python,
        vec![bind("run", 0, "get_x"), bind("get_x", 0, "effect")],
        "run",
    );
    for view in &views {
        let call = step(view, source, "self.get_x()");
        assert!(call.incidental.is_none() && !call.hidden, "{call:?}");
        assert!(!call.children.is_empty(), "the getter expands");
    }
    let source =
        "class S {\n  int[] x;\n  int[] id(int... x) { return x; }\n  void run() { id(1); }\n}\n";
    let views = publish_views(
        "S.java",
        source,
        s::Language::Java,
        vec![bind("run", 0, "id")],
        "run",
    );
    for view in &views {
        let call = step(view, source, "id(1)");
        assert!(call.incidental.is_none() && !call.hidden, "{call:?}");
    }
}

/// #547 review: an accessor's expandable operand stays shown and expands inside an expanded
/// body too, as it does when that body is the seed: entry → wrapper → set_x(compute()).
#[test]
fn expandable_accessor_operand_expands_in_a_callee_body() {
    let source = "struct S { x: u8 }\nimpl S {\n    fn set_x(&mut self, v: u8) { self.x = v; }\n    fn compute(&self) -> u8 { work() }\n    fn wrapper(&mut self) { self.set_x(self.compute()); }\n    fn entry(&mut self) { self.wrapper(); }\n}\nfn work() -> u8 { 1 }\n";
    let bindings = vec![
        bind("entry", 0, "wrapper"),
        bind("wrapper", 0, "set_x"),
        bind("wrapper", 1, "compute"),
        bind("compute", 0, "work"),
    ];
    let views = publish_views("a.rs", source, s::Language::Rust, bindings, "entry");
    for (show_all, view) in [false, true].into_iter().zip(&views) {
        let wrapper = &view.steps[0];
        assert!(!wrapper.children.is_empty(), "wrapper expands");
        let compute = wrapper
            .children
            .iter()
            .find(|s| source[s.range.start_byte..s.range.end_byte] == *"self.compute()")
            .unwrap();
        assert!(
            compute.incidental.is_none() && !compute.hidden,
            "show_all={show_all}"
        );
        assert!(
            !compute.children.is_empty(),
            "compute expands, show_all={show_all}"
        );
        let set_x = wrapper
            .children
            .iter()
            .find(|s| source[s.range.start_byte..s.range.end_byte].starts_with("self.set_x"))
            .unwrap();
        assert_eq!(set_x.incidental, Some(IncidentalReason::Accessor));
        assert_eq!(set_x.hidden, !show_all);
    }
}
