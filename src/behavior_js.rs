//! JavaScript-only adapter. No execution, filesystem reads, or model calls.
//! A conservative structured walk models evaluation order, not lexical call order.
use crate::behavior::{Participant, SequenceStep, SequenceView};
use crate::model::{CallSite, IndexPin, SourceFile, SourceRange, Symbol, SymbolKind};
use anyhow::{Context, Result, ensure};
use tree_sitter::Node;

const MAX_STEPS: usize = 200;
const MAX_PARTICIPANTS: usize = 20;
const MAX_DEPTH: usize = 24;
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
    matches!(
        n.kind(),
        "function_declaration"
            | "function_expression"
            | "generator_function_declaration"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
    )
}
fn optional_chain(n: Node<'_>) -> bool {
    // Follow only the callee/receiver chain, never argument strings or subexpressions.
    let mut current = Some(n);
    while let Some(node) = current {
        if node.child_by_field_name("optional_chain").is_some() {
            return true;
        }
        current = match node.kind() {
            "call_expression" => node.child_by_field_name("function"),
            "member_expression" | "subscript_expression" => node.child_by_field_name("object"),
            _ => None,
        };
    }
    false
}
fn loop_transfer(n: Node<'_>) -> bool {
    // Bounded iterative scan; nested-function bodies are not part of this loop.
    let mut todo = vec![n];
    while let Some(node) = todo.pop() {
        if matches!(node.kind(), "break_statement" | "continue_statement") {
            return true;
        }
        if !callable(node) && !matches!(node.kind(), "class" | "class_declaration") {
            todo.extend(named(node));
        }
    }
    false
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
    bindings: crate::behavior_bindings::Bindings,
    view: SequenceView,
    count: usize,
    show_all: bool,
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
        self.view.truncated = true;
        self.warn("Sequence bounds reached (200 steps, 20 participants, depth 24); omitted behavior is unknown.");
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
        for (i, &n) in nodes.iter().enumerate() {
            if self.count >= MAX_STEPS {
                self.limit();
                break;
            }
            let f = self.walk(n, depth + 1);
            out.steps.extend(f.steps);
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
                if depth >= MAX_DEPTH {
                    self.limit();
                    break;
                }
                if let Some(mut guard) = self.step(
                    nodes[i + 1],
                    "branch",
                    "only if the preceding path continues normally",
                ) {
                    let rest = self.block(&nodes[i + 1..], depth + 1);
                    guard.children = rest.steps;
                    out.continues = rest.continues;
                    out.exits |= rest.exits;
                    out.steps.push(guard);
                }
                break;
            }
        }
        out
    }
    fn walk(&mut self, n: Node<'_>, depth: usize) -> Flow {
        if self.count >= MAX_STEPS {
            self.limit();
            return Flow::empty();
        }
        if depth > MAX_DEPTH {
            self.limit();
            return self.boundary(n, "Depth limit: behavior omitted.");
        }
        if n.has_error() || n.is_missing() {
            return self.boundary(n, "Malformed JavaScript: behavior cannot be established.");
        }
        if callable(n) || matches!(n.kind(), "class" | "class_declaration") {
            return self.boundary(n, "Nested function/callback/class boundary: bodies are not executed here; class definition effects (base, computed keys, static initialization) are not expanded.");
        }
        match n.kind() {
            "statement_block" | "program" => self.block(&named(n), depth),
            "if_statement" | "ternary_expression" => {
                let mut out = self.field(n, "condition", depth);
                let cond = n
                    .child_by_field_name("condition")
                    .map(|x| bounded(self.text(x)))
                    .unwrap_or_default();
                if let Some(mut s) = self.step(n, "branch", format!("if {cond}")) {
                    let yes = self.field(n, "consequence", depth);
                    let no = self.field(n, "alternative", depth);
                    s.children = yes.steps;
                    s.alternate = no.steps;
                    out.continues = yes.continues || no.continues;
                    out.exits |= yes.exits || no.exits;
                    out.steps.push(s);
                }
                out
            }
            "binary_expression" | "augmented_assignment_expression" => {
                let op = n
                    .child_by_field_name("operator")
                    .map(|x| self.text(x).to_owned())
                    .unwrap_or_default();
                if matches!(op.as_str(), "&&" | "||" | "??" | "&&=" | "||=" | "??=") {
                    let mut out = self.field(n, "left", depth);
                    let guard = match op.as_str() {
                        "&&" | "&&=" => "RHS only if left is truthy",
                        "||" | "||=" => "RHS only if left is falsy",
                        _ => "RHS only if left is nullish",
                    };
                    if let Some(mut s) = self.step(n, "branch", guard) {
                        s.children = self.field(n, "right", depth).steps;
                        if n.kind() == "augmented_assignment_expression" {
                            let label = format!("write: {}", bounded(self.text(n)));
                            s.children.extend(self.step(n, "effect", label));
                        }
                        out.steps.push(s);
                    }
                    out
                } else {
                    let mut out = self.children(n, depth);
                    if n.kind() == "augmented_assignment_expression" {
                        let label = format!("write: {}", bounded(self.text(n)));
                        out.steps.extend(self.step(n, "effect", label));
                    }
                    out
                }
            }
            "call_expression" | "new_expression" => self.call(n, depth),
            "member_expression" | "subscript_expression" if optional_chain(n) => self.boundary(
                n,
                "Optional-chain access: guarded property evaluation not expanded.",
            ),
            "return_statement" | "throw_statement" => {
                let mut out = self.children(n, depth);
                let kind = if n.kind() == "return_statement" {
                    "return"
                } else {
                    "throw"
                };
                let label = bounded(self.text(n));
                out.steps.extend(self.step(n, kind, label));
                out.continues = false;
                out.exits = true;
                out
            }
            "await_expression" => {
                let mut out = self.children(n, depth);
                out.steps.extend(self.step(
                    n,
                    "await",
                    "await: suspension/resumption boundary, timing unknown",
                ));
                out
            }
            "while_statement" | "do_statement" | "for_statement" | "for_in_statement" => {
                self.loop_step(n, depth)
            }
            "try_statement" => self.try_step(n, depth),
            "switch_statement" | "with_statement" | "labeled_statement" | "yield_expression" => {
                let mut out = self.boundary(n, "Unsupported complex control: possible behavior remains unknown; body not expanded.");
                out.exits = true;
                out
            }
            "break_statement" | "continue_statement" => {
                let mut out = self.boundary(
                    n,
                    "Loop control transfer: remainder of this path is not executed.",
                );
                out.continues = false;
                out.exits = true;
                out
            }
            "variable_declarator"
                if n.child_by_field_name("name")
                    .is_some_and(|p| matches!(p.kind(), "object_pattern" | "array_pattern")) =>
            {
                let mut out = self.field(n, "value", depth);
                out.steps.extend(self.boundary(n, "Destructuring binding: computed keys/defaults/implicit reads are not expanded (RHS evaluated first).").steps);
                out
            }
            "assignment_expression"
                if n.child_by_field_name("left")
                    .is_some_and(|p| matches!(p.kind(), "object_pattern" | "array_pattern")) =>
            {
                let mut out = self.field(n, "right", depth);
                out.steps.extend(self.boundary(n, "Destructuring assignment: computed keys/defaults/writes are not expanded (RHS evaluated first).").steps);
                out
            }
            "assignment_expression" | "update_expression" => {
                let mut out = self.children(n, depth);
                let label = format!("write: {}", bounded(self.text(n)));
                out.steps.extend(self.step(n, "effect", label));
                out
            }
            "unary_expression" => {
                let mut out = self.children(n, depth);
                if self.text(n).trim_start().starts_with("delete ") {
                    let label = format!("delete: {}", bounded(self.text(n)));
                    out.steps.extend(self.step(n, "effect", label));
                }
                out
            }
            // Explicit safe structural/expression forms. Unknown grammar nodes fail closed.
            "expression_statement"
            | "else_clause"
            | "parenthesized_expression"
            | "arguments"
            | "lexical_declaration"
            | "variable_declaration"
            | "variable_declarator"
            | "sequence_expression"
            | "member_expression"
            | "subscript_expression"
            | "array"
            | "object"
            | "pair"
            | "computed_property_name"
            | "spread_element"
            | "template_string"
            | "template_substitution"
            | "array_pattern"
            | "object_pattern"
            | "pair_pattern"
            | "rest_pattern" => self.children(n, depth),
            "identifier"
            | "property_identifier"
            | "private_property_identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern"
            | "number"
            | "string"
            | "string_fragment"
            | "escape_sequence"
            | "true"
            | "false"
            | "null"
            | "undefined"
            | "this"
            | "super"
            | "regex"
            | "comment"
            | "empty_statement" => Flow::empty(),
            _ => self.boundary(
                n,
                &format!(
                    "Unsupported JavaScript form '{}': behavior unknown.",
                    n.kind()
                ),
            ),
        }
    }
    fn children(&mut self, n: Node<'_>, depth: usize) -> Flow {
        self.block(&named(n), depth)
    }
    fn call(&mut self, n: Node<'_>, depth: usize) -> Flow {
        // Optional chaining is not flattened: nullish receivers can skip argument evaluation.
        if optional_chain(n) {
            let mut out = self.boundary(
                n,
                "Optional-chain call: guarded invocation/evaluation not expanded.",
            );
            if let Some(c) = self.calls.iter().find(|c| {
                c.caller == self.seed.id
                    && c.path == self.file.path
                    && c.range.start_byte == n.start_byte()
                    && c.range.end_byte == n.end_byte()
            }) && let Some(s) = out.steps.first_mut()
            {
                s.call_id = Some(c.id.clone());
                s.range = c.range.clone();
            }
            return out;
        }
        let mut out = self.children(n, depth);
        let measured = self
            .calls
            .iter()
            .find(|c| {
                c.caller == self.seed.id
                    && c.path == self.file.path
                    && c.range.start_byte == n.start_byte()
                    && c.range.end_byte == n.end_byte()
            })
            .cloned();
        let Some(c) = measured else {
            out.steps.extend(
                self.boundary(
                    n,
                    "Call has no matching measured call site; target unknown.",
                )
                .steps,
            );
            return out;
        };
        // The full callee expression is source display text only; the native
        // measured CallSite still uses its independently verified token.
        let label = n
            .child_by_field_name("function")
            .or_else(|| n.child_by_field_name("constructor"))
            .and_then(|node| self.file.text.get(node.byte_range()))
            .unwrap_or(c.callee_text.as_deref().unwrap_or(""));
        // Binding and name matching can label a terminal visual group, not a
        // callee identity or a cross-participant sequence arrow.
        let participant = self
            .bindings
            .identify(n, &self.file.text, &self.file.path)
            .unwrap_or_else(|| Participant {
                declaration: None,
                id: "boundary:unknown".into(),
                label: "Unknown/external operations (visual group, not object identity)".into(),
                kind: "boundary".into(),
                identification: "Target unknown; visual group only.".into(),
            });
        let target = participant.id.clone();
        // A boundary lane groups operations visually; it does not identify a shared object.
        if !self.view.participants.iter().any(|p| p.id == target) {
            if self.view.participants.len() >= MAX_PARTICIPANTS {
                self.limit();
                // Keep evidence even when no additional lifeline can be allocated.
                if let Some(mut s) = self.step(
                    n,
                    "boundary",
                    format!("Participant limit: {}", bounded(label)),
                ) {
                    s.call_id = Some(c.id);
                    s.range = c.range;
                    out.steps.push(s);
                }
                return out;
            }
            self.view.participants.push(Participant {
                label: bounded(&participant.label),
                ..participant
            });
        }
        if let Some(mut s) = self.step(n, "call", label) {
            // Only the standalone wrapper is hidden, never receiver/argument effects,
            // test expressions, returned values, warn/error, or arbitrary logger names.
            if !self.show_all
                && matches!(label, "console.log" | "console.debug")
                && n.parent()
                    .is_some_and(|p| p.kind() == "expression_statement")
            {
                s.hidden = true;
                self.view.hidden_steps += 1;
                self.warn("Standalone console.log/debug wrappers are hidden by a naming heuristic, not a proof of purity; argument effects remain. Use Show all to restore them.");
            }
            s.call_id = Some(c.id);
            s.range = c.range;
            out.steps.push(s);
        }
        out
    }
    fn loop_step(&mut self, n: Node<'_>, depth: usize) -> Flow {
        let mut out = Flow::empty();
        if n.kind() == "for_statement" {
            out = self.field(n, "initializer", depth);
        }
        if n.kind() == "for_in_statement" {
            out = self.field(n, "right", depth);
        }
        if loop_transfer(n) {
            out.steps.extend(self.boundary(n, "Loop containing break/continue: control-transfer paths and iteration tails are not expanded.").steps);
            out.exits = true;
            return out;
        }
        if let Some(mut s) = self.step(
            n,
            "loop",
            format!("{}: possible iterations, not unrolled", n.kind()),
        ) {
            if n.kind() != "do_statement" {
                s.children.extend(self.field(n, "condition", depth).steps);
            }
            if n.kind() == "for_in_statement"
                && let Some(left) = n.child_by_field_name("left")
            {
                if matches!(left.kind(), "object_pattern" | "array_pattern") {
                    s.children.extend(self.boundary(left, "Loop destructuring binding: per-iteration reads/defaults are not expanded.").steps);
                } else {
                    s.children.extend(self.walk(left, depth + 1).steps);
                }
            }
            let body = self.field(n, "body", depth);
            s.children.extend(body.steps);
            if body.continues {
                let mut tail = self.field(n, "increment", depth).steps;
                if n.kind() == "do_statement" {
                    tail.extend(self.field(n, "condition", depth).steps);
                }
                if body.exits && !tail.is_empty() {
                    if let Some(mut g) =
                        self.step(n, "branch", "only if iteration continues normally")
                    {
                        g.children = tail;
                        s.children.push(g);
                    }
                } else {
                    s.children.extend(tail);
                }
            }
            out.exits = body.exits;
            // Unlike other loops, do/while executes the body at least once.
            if n.kind() == "do_statement" {
                out.continues = body.continues;
            }
            out.steps.push(s);
        }
        out
    }
    fn try_step(&mut self, n: Node<'_>, depth: usize) -> Flow {
        let mut out = Flow::empty();
        self.warn("Exception paths are possible, not proven; finally runs on normal and abrupt completion.");
        if let Some(mut s) = self.step(n, "try", "try; exception behavior unknown") {
            let body = self.field(n, "body", depth);
            out.exits = body.exits;
            out.continues = body.continues;
            s.children = body.steps;
            if let Some(catch) = n.child_by_field_name("handler")
                && let Some(mut c) = self.step(
                    catch,
                    "branch",
                    "catch: only if an exception reaches this handler",
                )
            {
                let mut f = self.field(catch, "body", depth);
                if let Some(param) = catch.child_by_field_name("parameter")
                    && param.kind() != "identifier"
                {
                    let mut prefix = self
                        .boundary(
                            param,
                            "Catch destructuring binding: computed keys/defaults are not expanded.",
                        )
                        .steps;
                    prefix.extend(f.steps);
                    f.steps = prefix;
                }
                out.exits |= f.exits;
                out.continues |= f.continues;
                c.children = f.steps;
                s.alternate.push(c);
            }
            if let Some(finally) = n.child_by_field_name("finalizer")
                && let Some(mut c) = self.step(
                    finally,
                    "note",
                    "finally: on both normal and abrupt completion",
                )
            {
                let f = self.field(finally, "body", depth);
                out.exits |= f.exits;
                out.continues &= f.continues;
                c.children = f.steps;
                s.alternate.push(c);
            }
            out.steps.push(s);
        }
        out
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
    parser.set_language(&tree_sitter_javascript::LANGUAGE.into())?;
    let tree = parser
        .parse(&file.text, None)
        .context("JavaScript parse unavailable")?;
    let mut node = tree.root_node();
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
    let mut b = Builder { file, seed, calls, bindings: crate::behavior_bindings::Bindings::new(tree.root_node(), &file.text), count: 0, show_all, view: SequenceView { semantic_warnings: vec![], semantic: Default::default(), incidental_steps: 0,
        revision, seed: seed.clone(), participants: vec![Participant { declaration: None, id: seed.id.clone(), label: bounded(&seed.name), kind: "method".into(), identification: "Selected source symbol.".into() }],
        steps: vec![], warnings: vec!["Static possible paths, not observed runtime order. Selectivity is an uncalibrated conservative heuristic. Implicit getters, coercions, iteration protocols, and other runtime effects are not expanded.".into()],
        hidden_steps: 0, truncated: false,
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
        b.boundary(
            node,
            "Malformed JavaScript: behavior cannot be established.",
        )
    } else {
        let mut f = b.field(node, "body", 0);
        if let Some(params) = node.child_by_field_name("parameters")
            && named(params).iter().any(|p| p.kind() != "identifier")
        {
            let mut boundary = b.boundary(params, "Parameter defaults/destructuring may execute before the body; behavior not expanded.").steps;
            boundary.extend(f.steps);
            f.steps = boundary;
        }
        f
    };
    b.view.steps = f.steps;
    Ok(b.view)
}
