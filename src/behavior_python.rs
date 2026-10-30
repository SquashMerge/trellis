//! Python-only adapter. No execution, filesystem reads, or model calls.
//! A conservative structured walk models evaluation order, not lexical call order.
use crate::behavior::{Participant, SequenceStep, SequenceView};
use crate::model::{CallSite, IndexPin, SourceFile, SourceRange, Symbol, SymbolKind};
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use tree_sitter::Node;

const MAX_STEPS: usize = 200;
const MAX_VISITS: usize = 4800;
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
    n.named_children(&mut cursor).take(MAX_VISITS + 1).collect()
}
fn callable(n: Node<'_>) -> bool {
    matches!(n.kind(), "function_definition" | "lambda")
}
// Only a syntactically proven module header enables postponed annotations.
// Do not search source text, nested scopes, or statements after the header.
fn future_annotations(module: Node<'_>, source: &str) -> bool {
    if module.kind() != "module" || module.has_error() {
        return false;
    }
    let mut cursor = module.walk();
    let mut visits = 0;
    let mut first_statement = true;
    let mut found = false;
    for statement in module.named_children(&mut cursor) {
        visits += 1;
        if visits > MAX_VISITS {
            return false;
        }
        if statement.kind() == "comment" {
            continue;
        }
        if first_statement
            && statement.kind() == "expression_statement"
            && statement.named_child_count() == 1
            && let Some(value) = statement.named_child(0)
            && value.kind() == "string"
            && source[value.byte_range()].starts_with(['\'', '"'])
            && !named(value).iter().any(|c| c.kind() == "interpolation")
        {
            first_statement = false;
            continue;
        }
        first_statement = false;
        if statement.kind() != "future_import_statement" {
            return found;
        }
        let mut names_cursor = statement.walk();
        let mut names = 0;
        for name in statement.named_children(&mut names_cursor) {
            visits += 1;
            if visits > MAX_VISITS {
                return false;
            }
            if name.kind() == "comment" {
                continue;
            }
            // Aliases and unfamiliar shapes conservatively disable this shortcut.
            if name.kind() != "dotted_name" || name.named_child_count() != 1 {
                return false;
            }
            let Some(identifier) = name.named_child(0) else {
                return false;
            };
            if identifier.kind() != "identifier" {
                return false;
            }
            let feature = &source[identifier.byte_range()];
            if !matches!(
                feature,
                "nested_scopes"
                    | "generators"
                    | "division"
                    | "absolute_import"
                    | "with_statement"
                    | "print_function"
                    | "unicode_literals"
                    | "barry_as_FLUFL"
                    | "generator_stop"
                    | "annotations"
            ) {
                return false;
            }
            found |= feature == "annotations";
            names += 1;
        }
        if names == 0 {
            return false;
        }
    }
    found
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
    visits: usize,
    future_annotations: bool,
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
            if self.count >= MAX_STEPS || self.visits >= MAX_VISITS {
                self.limit();
                // Omitted nodes may transfer control or fail argument evaluation.
                // Propagate uncertainty so an enclosing invocation is not unconditional.
                let cutoff = self.unknown(n, "Traversal limit: remaining evaluation unknown.");
                out.steps.extend(cutoff.steps);
                out.exits = true;
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

    // Compose evaluation without making a suffix unconditional after a possible exit.
    fn then(&mut self, mut first: Flow, next: Flow, n: Node<'_>) -> Flow {
        if !first.continues {
            return first;
        }
        if first.exits && !next.steps.is_empty() {
            if let Some(mut guard) =
                self.step(n, "branch", "only if the preceding path continues normally")
            {
                guard.children = next.steps;
                first.steps.push(guard);
            }
        } else {
            first.steps.extend(next.steps);
        }
        first.continues = next.continues;
        first.exits |= next.exits;
        first
    }
    fn children(&mut self, n: Node<'_>, depth: usize) -> Flow {
        if n.named_child_count() > MAX_VISITS {
            self.limit();
            return self.unknown(n, "AST width limit: behavior omitted.");
        }
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
    // Recognize only definitions whose header has no omitted eager behavior.
    // Annotation subtrees and the deferred function body are never traversed here.
    fn plain_definition(&mut self, n: Node<'_>) -> bool {
        if n.child_by_field_name("type_parameters").is_some()
            || (!self.future_annotations && n.child_by_field_name("return_type").is_some())
        {
            return false;
        }
        let Some(name) = n.child_by_field_name("name") else {
            return false;
        };
        let Some(parameters) = n.child_by_field_name("parameters") else {
            return false;
        };
        if name.kind() != "identifier" || parameters.kind() != "parameters" {
            return false;
        }
        let mut header_cursor = n.walk();
        for child in n.named_children(&mut header_cursor) {
            self.visits += 1;
            if self.visits >= MAX_VISITS {
                self.limit();
                return false;
            }
            if !matches!(
                child.kind(),
                "identifier" | "parameters" | "type" | "block" | "comment"
            ) {
                return false;
            }
        }
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            self.visits += 1;
            if self.visits >= MAX_VISITS {
                self.limit();
                return false;
            }
            let target = match parameter.kind() {
                "identifier" | "list_splat_pattern" | "dictionary_splat_pattern" => parameter,
                "positional_separator" | "keyword_separator" | "comment" => continue,
                "typed_parameter" if self.future_annotations => {
                    if parameter.named_child_count() != 2 {
                        return false;
                    }
                    let Some(annotation) = parameter.child_by_field_name("type") else {
                        return false;
                    };
                    let Some(target) = parameter.named_child(0) else {
                        return false;
                    };
                    if annotation.kind() != "type" || target == annotation {
                        return false;
                    }
                    target
                }
                // Defaults, destructuring and unknown parameter forms remain opaque.
                _ => return false,
            };
            match target.kind() {
                "identifier" if target.named_child_count() == 0 => {}
                "list_splat_pattern" | "dictionary_splat_pattern"
                    if target.named_child_count() == 1
                        && target
                            .named_child(0)
                            .is_some_and(|c| c.kind() == "identifier") => {}
                _ => return false,
            }
        }
        true
    }
    fn walk(&mut self, n: Node<'_>, depth: usize) -> Flow {
        if self.count >= MAX_STEPS || self.visits >= MAX_VISITS || depth > MAX_DEPTH {
            self.limit();
            return self.unknown(n, "Traversal limit: behavior omitted.");
        }
        self.visits += 1;
        if n.has_error() || n.is_missing() {
            return self.unknown(n, "Malformed Python: behavior unknown.");
        }
        match n.kind() {
            "function_definition" if self.plain_definition(n) => {
                let name = n.child_by_field_name("name").map(|c| self.text(c)).unwrap_or("?");
                let deferred = if n.child(0).is_some_and(|c| c.kind() == "async") {
                    "async body deferred"
                } else {
                    "body deferred"
                };
                self.event(n, "definition", &format!("define {name} · {deferred}"))
            }
            "function_definition" | "decorated_definition" | "lambda" => self.unknown(n,
                "Definition boundary: decorators/defaults/annotations and binding effects not expanded; callable body does not execute here."),
            "class_definition" => self.unknown(n,
                "Class definition boundary: bases, decorators, metaclass and class-body execution not expanded; method bodies do not execute here."),
            "list_comprehension" | "set_comprehension" | "dictionary_comprehension" | "generator_expression" => self.unknown(n,
                "Comprehension/generator boundary: iteration, filters and deferred execution not expanded."),
            "try_statement" => self.unknown(n,
                "Try boundary: exception selection, else and finally execution/transfers not expanded."),
            "with_statement" | "match_statement" => self.unknown(n,
                "Context-manager/match boundary: implicit protocol and conditional execution not expanded."),
            "for_statement" if n.child(0).is_some_and(|c| c.kind() == "async") => self.unknown(n,
                "Async iteration boundary: protocol and suspension not expanded."),
            "block" | "expression_statement" | "parenthesized_expression" | "argument_list"
            | "list" | "tuple" | "set" | "dictionary" | "pair" | "expression_list"
            | "binary_operator" | "unary_operator" | "not_operator" | "slice" => self.children(n, depth),
            "keyword_argument" | "named_expression" => self.field(n, "value", depth),
            "attribute" => {
                let value = self.field(n, "object", depth);
                let effect = self.boundary(n, "Attribute lookup: descriptor/__getattribute__ effects and type unresolved.");
                self.then(value, effect, n)
            }
            "subscript" => {
                let value = self.children(n, depth);
                let effect = self.boundary(n, "Subscription: implicit __getitem__ effects unresolved.");
                self.then(value, effect, n)
            }
            "call" => self.call(n, depth),
            "assignment" => {
                // This adapter walks only a selected callable body. Local variable
                // annotations are not evaluated, including attribute/subscript targets.
                // Nested class/function definitions are already opaque boundaries.
                if n.child_by_field_name("type").is_some() && n.child_by_field_name("right").is_none() {
                    if n.child_by_field_name("left").is_some_and(|c| c.kind() == "identifier") {
                        return Flow::empty();
                    }
                    return self.unknown(n, "Annotation-only target boundary: target address effects not expanded; local annotation is not evaluated.");
                }
                // Chained assignment is right-associative in the grammar but writes
                // left-to-right in Python. Bound it rather than reverse target effects.
                if n.child_by_field_name("right").is_some_and(|c| c.kind() == "assignment") {
                    return self.unknown(n, "Chained assignment boundary: target write sequence not expanded.");
                }
                let value = self.field(n, "right", depth);
                let target = n.child_by_field_name("left").map(|c| self.assignment_target(c, depth + 1)).unwrap_or_else(Flow::empty);
                let values = self.then(value, target, n);
                let label = if let Some(left) = n.child_by_field_name("left").filter(|c| c.kind() == "identifier") {
                    format!("bind name: {}", self.text(left))
                } else {
                    format!("write: {} (implicit setters/unpacking unresolved)", bounded(self.text(n)))
                };
                let write = self.event(n, "effect", &label);
                self.then(values, write, n)
            }
            "augmented_assignment" => self.unknown(n, "Augmented assignment boundary: target read/write and overloaded operator effects not expanded."),
            "if_statement" | "elif_clause" => self.if_step(n, depth),
            "else_clause" => self.field(n, "body", depth),
            "boolean_operator" => {
                let left = self.field(n, "left", depth);
                let op = n.child_by_field_name("operator").map(|c| self.text(c)).unwrap_or("?");
                let label = if op == "and" { "RHS only if left is truthy" } else { "RHS only if left is falsy" };
                let mut result = Flow::empty();
                if let Some(mut guard) = self.step(n, "branch", label) {
                    let rhs = self.field(n, "right", depth); result.exits = rhs.exits;
                    guard.children = rhs.steps; result.steps.push(guard);
                }
                self.then(left, result, n)
            }
            "comparison_operator" if n.named_child_count() > 2 => self.unknown(n,
                "Chained comparison boundary: later operands are conditional; not expanded."),
            "comparison_operator" => self.children(n, depth),
            "conditional_expression" => self.conditional(n, depth),
            "for_statement" | "while_statement" => self.loop_step(n, depth),
            "return_statement" | "raise_statement" | "break_statement" | "continue_statement" => {
                let value = self.children(n, depth);
                let kind = match n.kind() { "return_statement" => "return", "raise_statement" => "throw", _ => "exit" };
                let mut transfer = self.event(n, kind, &bounded(self.text(n)));
                transfer.exits = true; transfer.continues = false;
                self.then(value, transfer, n)
            }
            "await" => {
                let value = self.children(n, depth);
                let suspend = self.event(n, "await", "await: possible suspension/resumption; await protocol unresolved");
                self.then(value, suspend, n)
            }
            "yield" => self.unknown(n, "Yield boundary: generator suspension, send/throw/close and yield-from protocol not expanded."),
            "list_splat" | "dictionary_splat" => self.unknown(n, "Argument unpacking boundary: iteration/mapping protocol not expanded."),
            "concatenated_string" => self.children(n, depth),
            "string" if named(n).iter().any(|c| c.kind() == "interpolation") => self.unknown(n,
                "Formatted string boundary: interpolation and formatting protocol not expanded."),
            "identifier" | "integer" | "float" | "true" | "false" | "none" | "ellipsis"
            | "string" | "comment" | "pass_statement" | "global_statement" | "nonlocal_statement" => Flow::empty(),
            _ => self.unknown(n, &format!("Unsupported Python form '{}': behavior unknown.", n.kind())),
        }
    }
    fn if_step(&mut self, n: Node<'_>, depth: usize) -> Flow {
        let condition = self.field(n, "condition", depth);
        let mut result = Flow::empty();
        if let Some(mut branch) = self.step(
            n,
            "branch",
            format!(
                "if {}",
                n.child_by_field_name("condition")
                    .map(|c| bounded(self.text(c)))
                    .unwrap_or_default()
            ),
        ) {
            let yes = self.field(n, "consequence", depth);
            // Python exposes multiple alternative fields (elif*, else?). Build a
            // nested alternate chain, never execute all elif conditions linearly.
            let mut cursor = n.walk();
            let alternatives: Vec<_> = n
                .children_by_field_name("alternative", &mut cursor)
                .take(MAX_VISITS + 1)
                .collect();
            let no = self.alternatives(&alternatives, depth + 1);
            result.continues = yes.continues || no.continues;
            result.exits = yes.exits || no.exits;
            branch.children = yes.steps;
            branch.alternate = no.steps;
            result.steps.push(branch);
        }
        self.then(condition, result, n)
    }
    fn alternatives(&mut self, nodes: &[Node<'_>], depth: usize) -> Flow {
        let Some((&first, rest)) = nodes.split_first() else {
            return Flow::empty();
        };
        if depth > MAX_DEPTH || self.count >= MAX_STEPS {
            self.limit();
            return self.unknown(first, "Alternative limit: behavior omitted.");
        }
        if first.kind() != "elif_clause" {
            return self.walk(first, depth + 1);
        }
        let condition = self.field(first, "condition", depth);
        let yes = self.field(first, "consequence", depth);
        let no = self.alternatives(rest, depth + 1);
        let mut result = Flow {
            continues: yes.continues || no.continues,
            exits: yes.exits || no.exits,
            steps: vec![],
        };
        if let Some(mut branch) = self.step(
            first,
            "branch",
            format!(
                "elif {}",
                first
                    .child_by_field_name("condition")
                    .map(|c| bounded(self.text(c)))
                    .unwrap_or_default()
            ),
        ) {
            branch.children = yes.steps;
            branch.alternate = no.steps;
            result.steps.push(branch);
        }
        self.then(condition, result, first)
    }
    fn conditional(&mut self, n: Node<'_>, depth: usize) -> Flow {
        let children = named(n);
        if children.len() != 3 {
            return self.unknown(n, "Conditional expression shape unknown.");
        }
        let condition = self.walk(children[1], depth + 1);
        let yes = self.walk(children[0], depth + 1);
        let no = self.walk(children[2], depth + 1);
        let mut result = Flow {
            continues: yes.continues || no.continues,
            exits: yes.exits || no.exits,
            steps: vec![],
        };
        if let Some(mut branch) = self.step(
            n,
            "branch",
            "conditional expression: truthy / falsy alternative",
        ) {
            branch.children = yes.steps;
            branch.alternate = no.steps;
            result.steps.push(branch);
        }
        self.then(condition, result, n)
    }
    fn loop_step(&mut self, n: Node<'_>, depth: usize) -> Flow {
        if n.child_by_field_name("alternative").is_some() {
            return self.unknown(
                n,
                "Loop-else boundary: break versus exhaustion paths not expanded.",
            );
        }
        let init = if n.kind() == "for_statement" {
            self.field(n, "right", depth)
        } else {
            Flow::empty()
        };
        let mut result = Flow::empty();
        result.exits = true;
        if let Some(mut step) = self.step(
            n,
            "loop",
            "possible iterations, not unrolled; protocol and termination unresolved",
        ) {
            let condition = self.field(n, "condition", depth);
            let target = if n.kind() == "for_statement" {
                if let Some(left) = n.child_by_field_name("left") {
                    let address = self.assignment_target(left, depth + 1);
                    let write = self.event(
                        left,
                        "effect",
                        "iteration target write: implicit setters/unpacking unresolved",
                    );
                    self.then(address, write, left)
                } else {
                    Flow::empty()
                }
            } else {
                Flow::empty()
            };
            let body = self.field(n, "body", depth);
            let iteration = self.then(target, body, n);
            if let Some(mut guard) = self.step(
                n,
                "branch",
                "body only if condition is truthy / next item exists",
            ) {
                guard.children = iteration.steps;
                step.children = self
                    .then(
                        condition,
                        Flow {
                            steps: vec![guard],
                            ..Flow::empty()
                        },
                        n,
                    )
                    .steps;
            }
            result.steps.push(step);
        }
        self.then(init, result, n)
    }
    fn assignment_target(&mut self, n: Node<'_>, depth: usize) -> Flow {
        match n.kind() {
            "identifier" => Flow::empty(),
            "attribute" => self.field(n, "object", depth),
            "subscript" => self.children(n, depth),
            _ => self.unknown(
                n,
                "Assignment target boundary: unpacking/setter effects not expanded.",
            ),
        }
    }
    fn call(&mut self, n: Node<'_>, depth: usize) -> Flow {
        if n.child_by_field_name("arguments").is_some_and(|args| {
            named(args)
                .iter()
                .any(|c| matches!(c.kind(), "list_splat" | "dictionary_splat"))
        }) {
            return self.unknown(n, "Call unpacking boundary: positional/keyword evaluation and iteration effects not expanded.");
        }
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
        // Python indexing provides no semantic target. Never infer constructors,
        // types, dispatch or internal bindings from names (even capitalized ones).
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
}
fn call_label<'a>(n: Node<'_>, file: &'a SourceFile) -> &'a str {
    let function = n.child_by_field_name("function").unwrap_or(n);
    let callee = if function.kind() == "attribute" {
        function
            .child_by_field_name("attribute")
            .unwrap_or(function)
    } else {
        function
    };
    &file.text[callee.byte_range()]
}
fn unresolved_boundary() -> Participant {
    Participant {
        declaration: None,
        id: "boundary:python-unknown".into(),
        label: "Unresolved calls".into(),
        kind: "boundary".into(),
        identification:
            "Source hints only; types and dispatch unresolved. Not runtime object identity.".into(),
    }
}
fn unresolved_participant(n: Node<'_>, file: &SourceFile) -> Participant {
    let Some(function) = n.child_by_field_name("function") else {
        return unresolved_boundary();
    };
    let (kind, hint) = if function.kind() == "attribute" {
        (
            "unresolvedReceiver",
            function.child_by_field_name("object").unwrap_or(function),
        )
    } else {
        ("unresolvedCallee", function)
    };
    let text = &file.text[hint.byte_range()];
    Participant { declaration: None, id: format!("python:source:{kind}:{}", hex::encode(Sha256::digest(text.as_bytes()))), label: bounded(text), kind: kind.into(),
        identification: "Source expression only; type, descriptors and dispatch unresolved. Visual group, not object identity or a constructor claim.".into() }
}
pub fn build(
    revision: IndexPin,
    seed: &Symbol,
    file: &SourceFile,
    calls: &[CallSite],
    _show_all: bool,
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
    let mut b = Builder { file, seed, calls, count: 0, visits: 0, future_annotations: false, view: SequenceView { semantic_warnings: vec![],
        revision, seed: seed.clone(), participants: vec![Participant { declaration: None, id: seed.id.clone(), label: bounded(&seed.name), kind: "method".into(), identification: "Selected source symbol.".into() }],
        steps: vec![], warnings: vec!["Static possible paths, not a runtime trace. Python types, imports, dispatch, descriptors, operator/truthiness protocols and implicit exceptions are not resolved. Async/generator bodies show possible execution on await/iteration, not call-time execution. Unsupported definition-time effects remain opaque; function bodies are deferred.".into()], hidden_steps: 0, truncated: false,
    }};
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_python::LANGUAGE.into())?;
    // Bound parser input and time before walking cached syntax.
    let started = std::time::Instant::now();
    let mut budget = |_: &tree_sitter::ParseState| {
        if started.elapsed() > std::time::Duration::from_millis(250) {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let tree = if file.text.len() <= 4 * 1024 * 1024 {
        parser.parse_with_options(
            &mut |offset, _| &file.text.as_bytes()[offset..],
            None,
            Some(tree_sitter::ParseOptions::new().progress_callback(&mut budget)),
        )
    } else {
        None
    };
    let Some(tree) = tree else {
        b.limit();
        b.view.steps.push(SequenceStep {
            id: "python:parse-boundary".into(),
            kind: "boundary".into(),
            label: "Python parse budget exceeded (4 MiB / 250 ms); behavior unknown.".into(),
            path: file.path.clone(),
            range: seed.range.clone(),
            call_id: None,
            target: None,
            resolution: None,
            call_evidence: None,
            children: vec![],
            alternate: vec![],
            hidden: false,
        });
        return Ok(b.view);
    };
    b.future_annotations = future_annotations(tree.root_node(), &file.text);
    let mut node = tree.root_node();
    // Find the containing child with a cursor; do not allocate all siblings.
    let mut found = false;
    for _ in 0..512 {
        if node.start_byte() == seed.range.start_byte
            && node.end_byte() == seed.range.end_byte
            && callable(node)
        {
            found = true;
            break;
        }
        let mut cursor = node.walk();
        let child = node.named_children(&mut cursor).take(MAX_VISITS).find(|c| {
            c.start_byte() <= seed.range.start_byte && c.end_byte() >= seed.range.end_byte
        });
        match child {
            Some(c) => node = c,
            None => break,
        }
    }
    let flow = if !found {
        b.unknown(
            node,
            "Indexed callable not located within syntax search bound; behavior unknown.",
        )
    } else if node.has_error() {
        b.unknown(node, "Malformed Python callable: behavior unknown.")
    } else if let Some(body) = node.child_by_field_name("body") {
        b.walk(body, 0)
    } else {
        b.boundary(node, "Callable has no body; behavior unavailable.")
    };
    b.view.steps = flow.steps;
    Ok(b.view)
}
