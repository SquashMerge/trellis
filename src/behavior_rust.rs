//! Rust-only adapter. No execution, filesystem reads, or model calls.
//! A conservative structured walk models evaluation order, not lexical call order.
use crate::behavior::{
    CONTINUES_LABEL, Participant, SequenceStep, SequenceView, TRY_LABEL, fold_guard, measured_call,
};
use crate::model::{CallSite, IndexPin, SourceFile, SourceRange, Symbol, SymbolKind};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use tree_sitter::Node;

// The work bound (#536): the visible bound is applied to the view this builds.
const MAX_STEPS: usize = crate::behavior::MAX_WORK_STEPS;
const MAX_PARTICIPANTS: usize = 20;
const MAX_DEPTH: usize = 24;
/// Bounds the recursion over nested Rust syntax. The step tree it produces is bounded to
/// `MAX_DEPTH` levels separately by `behavior::cap_depth` (#522): counting syntax levels
/// against 24 dropped the main loop of a long function long before its steps nested.
const MAX_AST_DEPTH: usize = 96;
fn bounded(s: &str) -> String {
    let mut out: String = s.chars().take(180).collect();
    if s.chars().count() > 180 {
        out.push('…');
    }
    out
}
fn range(n: Node<'_>) -> SourceRange {
    SourceRange {
        start_byte: n.start_byte(),
        end_byte: n.end_byte(),
        start_line: n.start_position().row + 1,
        start_column: n.start_position().column + 1,
        end_line: n.end_position().row + 1,
        end_column: n.end_position().column + 1,
    }
}
fn named(n: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = n.walk();
    n.named_children(&mut cursor).collect()
}
fn callable(n: Node<'_>) -> bool {
    matches!(n.kind(), "function_item" | "closure_expression")
}
// continue=false means this path cannot reach the next statement. exits=true means
// some paths may leave, so any suffix must be explicitly guarded.
#[derive(Default)]
struct Flow {
    steps: Vec<SequenceStep>,
    continues: bool,
    exits: bool,
}
impl Flow {
    fn empty() -> Self {
        Self {
            continues: true,
            ..Self::default()
        }
    }
}
struct Builder<'a> {
    file: &'a SourceFile,
    seed: &'a Symbol,
    calls: &'a [CallSite],
    view: SequenceView,
    count: usize,
}
impl Builder<'_> {
    fn text(&self, n: Node<'_>) -> &str {
        &self.file.text[n.byte_range()]
    }
    fn warn(&mut self, msg: &str) {
        if self.view.warnings.len() < 16 && !self.view.warnings.iter().any(|s| s == msg) {
            self.view.warnings.push(msg.into());
        }
    }
    fn limit(&mut self) {
        crate::behavior::adapter_limit(&mut self.view, self.count);
        self.warn("Sequence bounds reached (2000 steps of work, 20 participants, depth 24); omitted behavior is unknown.");
    }
    fn step(&mut self, n: Node<'_>, kind: &str, label: impl AsRef<str>) -> Option<SequenceStep> {
        if self.count >= MAX_STEPS {
            self.limit();
            return None;
        }
        self.count += 1;
        Some(SequenceStep {
            id: format!("step:{}:{}", n.start_byte(), self.count),
            kind: kind.into(),
            label: bounded(label.as_ref()),
            path: self.file.path.clone(),
            range: range(n),
            call_id: None,
            target: None,
            resolution: None,
            call_evidence: None,
            children: vec![],
            alternate: vec![],
            hidden: false,
            lane: None,
            folded: vec![],
            incidental: None,
        })
    }
    fn boundary(&mut self, n: Node<'_>, message: &str) -> Flow {
        self.warn(message);
        Flow {
            steps: self.step(n, "boundary", message).into_iter().collect(),
            ..Flow::empty()
        }
    }
    fn field(&mut self, n: Node<'_>, field: &str, depth: usize) -> Flow {
        n.child_by_field_name(field)
            .map(|c| self.walk(c, depth + 1))
            .unwrap_or_else(Flow::empty)
    }
    fn block(&mut self, nodes: &[Node<'_>], depth: usize) -> Flow {
        let mut out = Flow::empty();
        // The statements after one that may exit are the children of a guard, unless the
        // guard folds into the `?` call it guards against (#542). Unfolded guards nest one
        // level per such statement: each level holds its steps and the guard after them.
        let mut levels: Vec<(Vec<SequenceStep>, SequenceStep)> = vec![];
        let mut current: Vec<SequenceStep> = vec![];
        for (i, &n) in nodes.iter().enumerate() {
            if self.count >= MAX_STEPS {
                self.limit();
                break;
            }
            let f = self.walk(n, depth + 1);
            current.extend(f.steps);
            out.exits |= f.exits;
            if !f.continues {
                out.continues = false;
                if i + 1 < nodes.len() {
                    self.warn("Statements after an unconditional exit are omitted as unreachable on this path.");
                }
                break;
            }
            if f.exits && i + 1 < nodes.len() {
                // Do not draw the suffix after a return/throw alternative as unconditional.
                if depth + levels.len() >= MAX_AST_DEPTH {
                    self.limit();
                    break;
                }
                let Some(guard) = self.step(nodes[i + 1], "branch", CONTINUES_LABEL) else {
                    break;
                };
                // After a `?` call the suffix stays at this level (#542).
                if let Some(guard) = fold_guard(&mut current, guard) {
                    levels.push((std::mem::take(&mut current), guard));
                }
            }
        }
        while let Some((mut steps, mut guard)) = levels.pop() {
            guard.children = current;
            steps.push(guard);
            current = steps;
        }
        out.steps = current;
        out
    }

    // Compose evaluation without making a suffix unconditional after a possible exit.
    fn then(&mut self, mut first: Flow, next: Flow, n: Node<'_>) -> Flow {
        if !first.continues {
            return first;
        }
        if first.exits && !next.steps.is_empty() {
            if let Some(guard) = self.step(n, "branch", CONTINUES_LABEL) {
                match fold_guard(&mut first.steps, guard) {
                    // After a `?` call the rest of the evaluation follows it (#542).
                    None => first.steps.extend(next.steps),
                    Some(mut guard) => {
                        guard.children = next.steps;
                        first.steps.push(guard);
                    }
                }
            }
        } else {
            first.steps.extend(next.steps);
        }
        first.continues = next.continues;
        first.exits |= next.exits;
        first
    }
    fn children(&mut self, n: Node<'_>, depth: usize) -> Flow {
        self.block(&named(n), depth)
    }
    fn event(&mut self, n: Node<'_>, kind: &str, label: &str) -> Flow {
        Flow {
            steps: self.step(n, kind, label).into_iter().collect(),
            ..Flow::empty()
        }
    }
    fn unknown(&mut self, n: Node<'_>, label: &str) -> Flow {
        let mut f = self.boundary(n, label);
        f.exits = true;
        f
    }
    fn walk(&mut self, n: Node<'_>, depth: usize) -> Flow {
        if self.count >= MAX_STEPS {
            self.limit();
            return Flow::empty();
        }
        if depth > MAX_AST_DEPTH {
            self.limit();
            return self.unknown(n, "Depth limit: behavior omitted.");
        }
        if n.has_error() || n.is_missing() {
            return self.unknown(n, "Malformed Rust: behavior unknown.");
        }
        match n.kind() {
            "function_item" | "closure_expression" => self.boundary(n, "Nested callable boundary: body does not execute at definition; captures are not expanded."),
            "async_block" => self.boundary(n, "Async block boundary: future construction does not execute its body; captures are not expanded."),
            // A logging macro (#543) is a boundary that continues: it guards nothing after it.
            "macro_invocation" if crate::behavior_incidental::logging_call(self.text(n)) => self.event(n, "boundary", "Logging macro: expansion and argument effects not expanded."),
            "macro_invocation" | "unsafe_block" | "const_block" => self.unknown(n, "Opaque macro/unsafe/const boundary: expansion, effects and transfers unknown."),
            "break_expression" | "continue_expression" | "yield_expression" => {
                let mut f = self.boundary(n, "Unsupported control transfer: remainder of this path omitted.");
                f.continues = false; f.exits = true; f
            }
            "block" if named(n).iter().any(|c| c.kind() == "label") => self.unknown(n, "Labeled block: control transfers not expanded."),
            "array_expression" if n.child_by_field_name("length").is_some() => self.unknown(n, "Array repeat boundary: const length and element evaluation are not expanded."),
            "block" | "expression_statement" | "else_clause" | "parenthesized_expression" | "arguments"
            | "tuple_expression" | "array_expression" | "field_expression" | "index_expression"
            | "reference_expression" | "unary_expression" | "range_expression" => self.children(n, depth),
            "type_cast_expression" => self.field(n, "value", depth),
            "let_declaration" => {
                let f = self.field(n, "value", depth);
                if n.child_by_field_name("alternative").is_some() {
                    let b = self.unknown(n, "let-else pattern/transfer boundary: alternatives not expanded.");
                    self.then(f, b, n)
                } else { f }
            }
            "let_condition" => self.field(n, "value", depth),
            "if_expression" => {
                let condition = self.field(n, "condition", depth);
                let mut f = Flow::empty();
                if let Some(mut s) = self.step(n, "branch", format!("if {}", n.child_by_field_name("condition").map(|c| bounded(self.text(c))).unwrap_or_default())) {
                    let yes = self.field(n, "consequence", depth);
                    let no = self.field(n, "alternative", depth);
                    f.continues = yes.continues || no.continues; f.exits = yes.exits || no.exits;
                    s.children = yes.steps; s.alternate = no.steps; f.steps.push(s);
                }
                self.then(condition, f, n)
            }
            "match_expression" => self.match_step(n, depth),
            "while_expression" | "for_expression" | "loop_expression" => self.loop_step(n, depth),
            "binary_expression" => {
                let op = n.child_by_field_name("operator").map(|o| self.text(o)).unwrap_or("");
                if matches!(op, "&&" | "||") {
                    let label = if op == "&&" { "RHS only if left is true" } else { "RHS only if left is false" };
                    let left = self.field(n, "left", depth);
                    let mut f = Flow::empty();
                    if let Some(mut s) = self.step(n, "branch", label) {
                        let right = self.field(n, "right", depth); f.exits = right.exits;
                        s.children = right.steps; f.steps.push(s);
                    }
                    self.then(left, f, n)
                } else { self.children(n, depth) }
            }
            "call_expression" => self.call(n, depth),
            "return_expression" => {
                let value = self.children(n, depth);
                let mut f = self.event(n, "return", &bounded(self.text(n)));
                f.exits = true; f.continues = false; self.then(value, f, n)
            }
            "try_expression" => {
                let value = self.children(n, depth);
                let mut f = Flow::empty();
                let mut value = value;
                if let Some(mut s) = self.step(n, "branch", TRY_LABEL) {
                    s.alternate.extend(self.step(n, "return", "?: early return on residual; conversion not expanded"));
                    // The call it guards may return early (#542); `x?` keeps its branch.
                    f.steps.extend(fold_try(&mut value.steps, s, try_operand(n)));
                }
                f.exits = true; self.then(value, f, n)
            }
            "await_expression" => {
                let value = self.children(n, depth);
                let f = self.event(n, "await", "await: possible suspension/resumption when polled; timing unknown");
                self.then(value, f, n)
            }
            "assignment_expression" => {
                // Rust assignment evaluates RHS before the assignee place (unlike JS).
                let right = self.field(n, "right", depth);
                let left = self.field(n, "left", depth);
                let values = self.then(right, left, n);
                let effect = self.event(n, "effect", &format!("write: {}", bounded(self.text(n))));
                self.then(values, effect, n)
            }
            "compound_assignment_expr" => self.unknown(n, "Compound assignment: operand order depends on primitive versus overloaded operation; write/evaluation not expanded."),
            "generic_function" => self.field(n, "function", depth),
            "struct_expression" => self.field(n, "body", depth),
            "field_initializer_list" => self.children(n, depth),
            "field_initializer" => self.field(n, "value", depth),
            "base_field_initializer" => self.children(n, depth),
            "identifier" | "field_identifier" | "shorthand_field_initializer" | "scoped_identifier" | "self"
            | "integer_literal" | "float_literal" | "boolean_literal" | "char_literal" | "string_literal"
            | "raw_string_literal" | "unit_expression" | "line_comment" | "block_comment" | "empty_statement"
            | "use_declaration" => Flow::empty(),
            _ => self.unknown(n, &format!("Unsupported Rust form '{}': behavior unknown.", n.kind())),
        }
    }
    fn call(&mut self, n: Node<'_>, depth: usize) -> Flow {
        let evaluation = self.children(n, depth);
        if !evaluation.continues {
            return evaluation;
        }
        let Some(c) = self
            .calls
            .iter()
            .find(|c| {
                c.caller == self.seed.id
                    && c.path == self.file.path
                    && c.range.start_byte == n.start_byte()
                    && c.range.end_byte == n.end_byte()
            })
            .cloned()
        else {
            let f = self.unknown(
                n,
                "Call has no matching measured call site; target unknown.",
            );
            return self.then(evaluation, f, n);
        };
        let mut participant = unresolved_participant(n, self.file);
        if participant.kind != "internal"
            && self.view.participants.len() >= MAX_PARTICIPANTS - 1
            && !self
                .view
                .participants
                .iter()
                .any(|p| p.id == participant.id)
        {
            // Reserve the last lane for excess source hints. Keep measured call
            // steps (and chain grouping) intact; no behavior has been omitted.
            participant = unresolved_boundary();
            self.warn("Source-hint participant limit reached; additional hints share Unresolved calls. Measured call evidence is retained.");
        }
        let mut target = Some(participant.id.clone());
        if !self
            .view
            .participants
            .iter()
            .any(|p| p.id == participant.id)
        {
            if self.view.participants.len() >= MAX_PARTICIPANTS {
                self.limit();
                target = None;
            } else {
                self.view.participants.push(participant);
            }
        }
        let mut f = Flow::empty();
        if let Some(mut s) = self.step(
            n,
            if target.is_some() { "call" } else { "boundary" },
            call_label(n, self.file),
        ) {
            s.call_id = Some(c.id);
            s.range = c.range;
            f.steps.push(s);
        }
        self.then(evaluation, f, n)
    }
    fn match_step(&mut self, n: Node<'_>, depth: usize) -> Flow {
        let value = self.field(n, "value", depth);
        // A match can complete normally only through an arm that completes normally.
        // Exhaustiveness is not type-checked here; an unmatched value is not a normal path.
        let mut f = Flow {
            continues: false,
            ..Flow::empty()
        };
        if let Some(mut s) = self.step(
            n,
            "branch",
            "match: mutually exclusive arms; first matching pattern with true guard",
        ) {
            if let Some(body) = n.child_by_field_name("body") {
                for arm in named(body).into_iter().filter(|a| a.kind() == "match_arm") {
                    if self.count >= MAX_STEPS {
                        self.limit();
                        break;
                    }
                    if let Some(mut a) = self.step(
                        arm,
                        "branch",
                        format!(
                            "only if no earlier arm selected and pattern matches: {}",
                            arm.child_by_field_name("pattern")
                                .map(|p| bounded(self.text(p)))
                                .unwrap_or_default()
                        ),
                    ) {
                        let pattern = arm.child_by_field_name("pattern");
                        let guard = pattern.and_then(|p| p.child_by_field_name("condition"));
                        let result = self.field(arm, "value", depth);
                        f.exits |= result.exits;
                        if let Some(g) = guard {
                            let evaluated = self.walk(g, depth + 1);
                            f.exits |= evaluated.exits;
                            f.continues |= evaluated.continues && result.continues;
                            if let Some(mut selected) =
                                self.step(arm, "branch", "arm body only if guard is true")
                            {
                                selected.children = result.steps;
                                let guarded = Flow {
                                    steps: vec![selected],
                                    ..Flow::empty()
                                };
                                a.children = self.then(evaluated, guarded, arm).steps;
                            }
                        } else {
                            f.continues |= result.continues;
                            a.children = result.steps;
                        }
                        s.alternate.push(a);
                    }
                }
            }
            f.steps.push(s);
        }
        self.then(value, f, n)
    }
    fn loop_step(&mut self, n: Node<'_>, depth: usize) -> Flow {
        if named(n).iter().any(|c| c.kind() == "label") {
            return self.unknown(n, "Labeled loop: transfers not expanded.");
        }
        let init = if n.kind() == "for_expression" {
            self.field(n, "value", depth)
        } else {
            Flow::empty()
        };
        let mut f = Flow::empty();
        f.exits = true;
        if let Some(mut s) = self.step(
            n,
            "loop",
            format!(
                "{}: possible iterations, not unrolled; exit/termination unknown",
                n.kind()
            ),
        ) {
            // Reserve the body guard before the body: a body that reaches the step bound
            // keeps its first steps instead of losing them all to a missing guard (#522).
            let guard = self.step(
                n,
                "branch",
                "body only if condition is true / next item exists (loop: each iteration)",
            );
            let cond = self.field(n, "condition", depth);
            let body = self.field(n, "body", depth);
            // `loop` always enters its body. An unconditional return cannot reach
            // its suffix. Keep boundary-containing bodies conservative: an opaque
            // macro/transfer may break the loop rather than return from the callable.
            fn has_boundary(steps: &[SequenceStep]) -> bool {
                steps.iter().any(|s| {
                    s.kind == "boundary" || has_boundary(&s.children) || has_boundary(&s.alternate)
                })
            }
            if n.kind() == "loop_expression" && !body.continues && !has_boundary(&body.steps) {
                f.continues = false;
            }
            if let Some(mut g) = guard {
                g.children = body.steps;
                s.children = self
                    .then(
                        cond,
                        Flow {
                            steps: vec![g],
                            ..Flow::empty()
                        },
                        n,
                    )
                    .steps;
            }
            f.steps.push(s);
        }
        self.then(init, f, n)
    }
}
// A method label names only the immediate callee. Source ranges and measured
// CallSite evidence still cover the original expression, including its receiver.
fn call_function(mut n: Node<'_>) -> Option<Node<'_>> {
    n = n.child_by_field_name("function")?;
    while n.kind() == "generic_function" {
        n = n.child_by_field_name("function")?;
    }
    Some(n)
}
fn call_label<'a>(n: Node<'_>, file: &'a SourceFile) -> &'a str {
    let function = call_function(n).unwrap_or(n);
    let callee = if function.kind() == "field_expression" {
        function.child_by_field_name("field").unwrap_or(function)
    } else {
        function
    };
    &file.text[callee.byte_range()]
}
fn unresolved_boundary() -> Participant {
    Participant { declaration: None, column: None,
        id: "boundary:rust-unknown".into(),
        label: "Unresolved calls".into(),
        kind: "boundary".into(),
        identification: "Source hints only; type and dispatch unresolved. Visual group, not runtime object identity.".into(),
    }
}
fn source_hint(kind: &str, expression: &str) -> Participant {
    // The full source spelling, not its shortened label, identifies a visual
    // group. Stable across call sites; deliberately not a binding/object ID.
    Participant { declaration: None, column: None,
        id: format!("rust:source:{kind}:{}", hex::encode(Sha256::digest(expression.as_bytes()))),
        label: bounded(expression),
        kind: kind.into(),
        identification: "Source expression only; type and dispatch unresolved. Repeated names are visual groups, not runtime object identity.".into(),
    }
}
fn simple_receiver(mut n: Node<'_>) -> bool {
    for _ in 0..MAX_DEPTH {
        match n.kind() {
            "identifier" | "self" | "scoped_identifier" => return true,
            "field_expression" => match n.child_by_field_name("value") {
                Some(value) => n = value,
                None => return false,
            },
            _ => return false,
        }
    }
    false
}
fn chain_result(mut n: Node<'_>) -> bool {
    for _ in 0..MAX_DEPTH {
        match n.kind() {
            "call_expression" => return true,
            "try_expression" | "await_expression" | "parenthesized_expression" => {
                match n.named_child(0) {
                    Some(value) => n = value,
                    None => return false,
                }
            }
            _ => return false,
        }
    }
    false
}
fn unresolved_participant(n: Node<'_>, file: &SourceFile) -> Participant {
    let Some(function) = call_function(n) else {
        return unresolved_boundary();
    };
    match function.kind() {
        "identifier" | "scoped_identifier" => {
            source_hint("unresolvedCallee", &file.text[function.byte_range()])
        }
        "field_expression" => {
            let Some(receiver) = function.child_by_field_name("value") else {
                return unresolved_boundary();
            };
            if simple_receiver(receiver) {
                source_hint("unresolvedReceiver", &file.text[receiver.byte_range()])
            } else if chain_result(receiver) {
                Participant { declaration: None, column: None,
                    id: "rust:syntax:chain-results".into(),
                    label: "Chain results".into(),
                    kind: "unresolvedReceiver".into(),
                    identification: "Syntactic call/try-chain results only; type and dispatch unresolved. Shared visual group, not runtime object identity or a claimed return type.".into(),
                }
            } else {
                unresolved_boundary()
            }
        }
        _ => unresolved_boundary(),
    }
}
fn receiver_call(n: Node<'_>) -> Option<Node<'_>> {
    let function = call_function(n)?;
    if function.kind() != "field_expression" {
        return None;
    }
    let receiver = function.child_by_field_name("value")?;
    (receiver.kind() == "call_expression").then_some(receiver)
}
// Presentation only: no claim about purity, builder types, or object identity.
// Run after measurement so grouping cannot change step IDs, limits, or guards.
// Each group is a step: grouping stops, losing nothing, before the view's steps would
// pass the work bound (#536).
fn group_chains(n: Node<'_>, file: &SourceFile, steps: &mut Vec<SequenceStep>) -> bool {
    let mut room = MAX_STEPS.saturating_sub(crate::behavior::step_count(steps));
    let mut cursor = n.walk();
    let mut depth = 0;
    let mut budget = MAX_STEPS * MAX_DEPTH;
    let mut limited = false;
    loop {
        if budget == 0 {
            return true;
        }
        budget -= 1;
        let node = cursor.node();
        let opaque = (depth > 0 && callable(node))
            || matches!(
                node.kind(),
                "macro_invocation" | "unsafe_block" | "const_block" | "async_block"
            );
        if !opaque {
            if room == 0 {
                return true;
            }
            if wrap_syntax_chain(node, file, steps) {
                room -= 1;
            }
            if depth < MAX_DEPTH {
                if cursor.goto_first_child() {
                    depth += 1;
                    continue;
                }
            } else if node.child_count() > 0 {
                limited = true;
            }
        }
        loop {
            if depth == 0 {
                return limited;
            }
            if cursor.goto_next_sibling() {
                break;
            }
            cursor.goto_parent();
            depth -= 1;
        }
    }
}
/// Wraps the call chain at `n` into one group step; true when it did.
fn wrap_syntax_chain(n: Node<'_>, file: &SourceFile, steps: &mut Vec<SequenceStep>) -> bool {
    if n.kind() == "call_expression" {
        let mut chain = vec![n];
        let mut receiver = n;
        while let Some(next) = receiver_call(receiver) {
            if chain.len() >= MAX_DEPTH {
                return false;
            }
            chain.push(next);
            receiver = next;
        }
        if chain.len() >= 3 {
            chain.reverse();
            let labels: Vec<_> = chain
                .iter()
                .map(|&c| {
                    let name = call_label(c, file);
                    let args = c.child_by_field_name("arguments");
                    // Include simple source configuration, never summarize nested effects.
                    let simple = args.filter(|a| {
                        a.end_byte() - a.start_byte() <= 48
                            && !named(*a).is_empty()
                            && named(*a).iter().all(|v| {
                                matches!(
                                    v.kind(),
                                    "identifier"
                                        | "scoped_identifier"
                                        | "boolean_literal"
                                        | "integer_literal"
                                        | "float_literal"
                                        | "string_literal"
                                        | "char_literal"
                                        | "raw_string_literal"
                                )
                            })
                    });
                    simple
                        .map(|a| format!("{name}{}", &file.text[a.byte_range()]))
                        .unwrap_or_else(|| name.to_owned())
                })
                .collect();
            let label = format!("{} ({} calls)", labels.join(" → "), labels.len());
            return wrap_chain(n, &chain, &label, steps);
        }
    }
    false
}

fn wrap_chain(n: Node<'_>, chain: &[Node<'_>], label: &str, steps: &mut Vec<SequenceStep>) -> bool {
    // Only a flat, fully measured evaluation is eligible. Branches, opaque
    // boundaries, writes, suspension, and transfers conservatively prevent it.
    let first = steps
        .iter()
        .position(|s| s.range.start_byte >= n.start_byte() && s.range.end_byte <= n.end_byte());
    if let Some(first) = first {
        let end = first
            + steps[first..]
                .iter()
                .take_while(|s| {
                    s.range.start_byte >= n.start_byte() && s.range.end_byte <= n.end_byte()
                })
                .count();
        let slice = &steps[first..end];
        if slice
            .iter()
            .all(|s| s.kind == "call" && s.call_id.is_some())
            // Only the chain's last call may return early (`chain()?`, #542); an early
            // return inside an argument stays visible.
            && slice[..slice.len() - 1].iter().all(|s| s.folded.is_empty())
            && chain.iter().all(|c| {
                slice.iter().any(|s| {
                    s.range.start_byte == c.start_byte() && s.range.end_byte == c.end_byte()
                })
            })
        {
            let mut group = slice.last().unwrap().clone();
            group.id = format!("group:{}:{}", n.start_byte(), n.end_byte());
            group.kind = "group".into();
            group.label = bounded(label);
            group.range = range(n);
            group.call_id = None;
            // A member that may return early keeps its own folded steps (#542).
            group.folded.clear();
            group.children = steps.drain(first..end).collect();
            steps.insert(first, group);
            return true;
        }
    }
    // Do not wrap shorter subchains inside an already collapsed chain.
    steps
        .iter_mut()
        .filter(|step| step.kind != "group")
        .any(|step| {
            wrap_chain(n, chain, label, &mut step.children)
                || wrap_chain(n, chain, label, &mut step.alternate)
        })
}
/// The byte range of a `?`'s direct operand, through transparent parentheses, when that
/// operand is a call (#542). A field, tuple index, await or other value is not.
fn try_operand(n: Node<'_>) -> Option<(usize, usize)> {
    let mut operand = n.named_child(0)?;
    while operand.kind() == "parenthesized_expression" {
        operand = operand.named_child(0)?;
    }
    (operand.kind() == "call_expression").then(|| (operand.start_byte(), operand.end_byte()))
}

/// Folds a `?` early-return branch into the measured call that is its direct operand
/// (#542). That call is evaluated last: the last of `steps`, or the last under trailing
/// "continues normally" guards (`outer(x?, y())?`). A `?` whose operand is not a call keeps
/// its branch. Lossless: the branch, with its early return as `alternate`, is kept on the
/// call's `folded`. Returns the branch when it could not be folded.
fn fold_try(
    steps: &mut [SequenceStep],
    branch: SequenceStep,
    operand: Option<(usize, usize)>,
) -> Option<SequenceStep> {
    fn find(steps: &mut [SequenceStep], start: usize, end: usize) -> Option<&mut SequenceStep> {
        let step = steps.last_mut()?;
        let exact =
            measured_call(step) && step.range.start_byte == start && step.range.end_byte == end;
        let guard =
            step.kind == "branch" && step.label == CONTINUES_LABEL && step.alternate.is_empty();
        if exact {
            Some(step)
        } else if guard {
            find(&mut step.children, start, end)
        } else {
            None
        }
    }
    match operand.and_then(|(start, end)| find(steps, start, end)) {
        Some(call) => {
            call.folded.push(branch);
            None
        }
        None => Some(branch),
    }
}

pub fn build(
    revision: IndexPin,
    seed: &Symbol,
    file: &SourceFile,
    calls: &[CallSite],
    show_all: bool,
) -> Result<SequenceView> {
    ensure!(seed.path == file.path, "symbol/source path mismatch");
    ensure!(
        matches!(seed.kind, SymbolKind::Function | SymbolKind::Method),
        "sequence seed must be a function or method"
    );
    ensure!(
        seed.range.start_byte < seed.range.end_byte && seed.range.end_byte <= file.text.len(),
        "invalid symbol source range"
    );
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let tree = parser
        .parse(&file.text, None)
        .context("Rust parse unavailable")?;
    let mut expansions = crate::native_rust_macros::Expansions::new(&file.text);
    // A callable measured inside a macro has syntax only in that macro's expansion (#551).
    let mut node =
        expansions.root_for(tree.root_node(), seed.range.start_byte, seed.range.end_byte);
    // Locate the exact indexed function, not a nested body or an approximate span.
    loop {
        if node.start_byte() == seed.range.start_byte
            && node.end_byte() == seed.range.end_byte
            && callable(node)
        {
            break;
        }
        let child = named(node).into_iter().find(|c| {
            c.start_byte() <= seed.range.start_byte && c.end_byte() >= seed.range.end_byte
        });
        match child {
            Some(c) => node = c,
            None => break,
        }
    }
    let mut b = Builder { file, seed, calls, count: 0, view: SequenceView { semantic_warnings: vec![], semantic: Default::default(), incidental_steps: 0,
        revision, seed: seed.clone(), participants: vec![Participant { declaration: None, column: None, id: seed.id.clone(), label: bounded(&seed.name), kind: "method".into(), identification: "Selected source symbol.".into() }],
        steps: vec![], warnings: vec!["Static possible paths, not a runtime trace. Rust types, traits, dispatch, cfg/attributes, implicit drops, coercions and iterator protocols are not resolved. Async bodies describe possible execution when polled, not future construction. Calls may panic or diverge; those implicit paths are not expanded.".into()], hidden_steps: 0, truncated: false, truncated_by: vec![],
    }};
    let f = if !callable(node)
        || node.start_byte() != seed.range.start_byte
        || node.end_byte() != seed.range.end_byte
    {
        b.boundary(
            node,
            "Indexed function does not match cached syntax; behavior unknown.",
        )
    } else if node.has_error() {
        b.boundary(node, "Malformed Rust: behavior cannot be established.")
    } else {
        b.field(node, "body", 0)
    };
    b.view.steps = f.steps;
    if !show_all && group_chains(node, file, &mut b.view.steps) {
        b.warn("Chain grouping traversal truncated at its depth/work bound; original measured steps are retained.");
    }
    Ok(b.view)
}
