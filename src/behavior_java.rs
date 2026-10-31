//! Java syntax-only possible paths. No execution, class loading or provider calls.
//! A conservative structured walk models evaluation order, not lexical call order.
use crate::behavior::{Participant, SequenceStep, SequenceView};
use crate::model::{CallSite, IndexPin, SourceFile, SourceRange, Symbol, SymbolKind};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use tree_sitter::Node;

// The work bound (#536): the visible bound is applied to the view this builds.
const MAX_STEPS: usize = crate::behavior::MAX_WORK_STEPS;
const MAX_PARTICIPANTS: usize = 20;
const MAX_DEPTH: usize = 24;
const MAX_VISITS: usize = 10_000;
fn bounded(s: &str) -> String {
    let mut out: String = s.chars().take(180).collect();
    if s.chars().nth(180).is_some() {
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
fn callable(n: Node<'_>) -> bool {
    matches!(
        n.kind(),
        "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration"
            | "lambda_expression"
            | "annotation_type_element_declaration"
    )
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
        self.warn("Sequence bounds reached (2000 steps of work, 20 participants, depth 24, 10000 AST visits); omitted behavior is unknown.");
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
                    self.warn("Remaining statements omitted: this path exits or its evaluation is incomplete.");
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
        let mut cursor = n.walk();
        // A very wide block must not allocate an unbounded node vector.
        let nodes: Vec<_> = n.named_children(&mut cursor).take(MAX_VISITS).collect();
        let omitted = n.named_child_count() > nodes.len();
        let f = self.block(&nodes, depth);
        if omitted {
            self.limit();
            let boundary = self.unknown(
                n,
                "Child traversal budget exceeded; remaining behavior unknown.",
            );
            self.then(f, boundary, n)
        } else {
            f
        }
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
        if self.count >= MAX_STEPS || self.visits >= MAX_VISITS {
            self.limit();
            let mut flow = self.unknown(
                n,
                "Traversal budget exhausted: remaining evaluation and enclosing operation omitted.",
            );
            // Missing argument/receiver work must never count as normal completion.
            // Stop this displayed path rather than invent the enclosing invocation.
            flow.continues = false;
            return flow;
        }
        self.visits += 1;
        if depth > MAX_DEPTH {
            self.limit();
            return self.unknown(n, "Depth limit: behavior omitted.");
        }
        if n.has_error() || n.is_missing() {
            return self.unknown(n, "Malformed Java: behavior unknown.");
        }
        match n.kind() {
            "lambda_expression" => self.boundary(n, "Lambda boundary: body does not execute at definition; captures are not expanded."),
            "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration" | "class_body"
            | "method_declaration" | "constructor_declaration" | "compact_constructor_declaration" =>
                self.boundary(n, "Nested declaration boundary: bodies are not executed here; implicit initialization is not expanded."),
            "switch_expression" | "switch_statement" | "try_statement" | "try_with_resources_statement"
            | "synchronized_statement" | "do_statement"
            | "labeled_statement" | "assert_statement" | "method_reference" | "string_template" =>
                self.unknown(n, &format!("Opaque Java {} boundary: evaluation and control transfers are not expanded.", n.kind())),
            "break_statement" | "continue_statement" | "yield_statement" => {
                let mut f = self.boundary(n, "Unsupported control transfer: remainder of this path omitted.");
                f.continues = false; f.exits = true; f
            }
            "block" | "constructor_body" | "expression_statement" | "parenthesized_expression"
            | "argument_list" | "array_initializer" | "unary_expression" | "dimensions_expr" => self.children(n, depth),
            "local_variable_declaration" => {
                // Skip modifiers, annotations and types; only declarator initializers execute.
                let mut cursor = n.walk();
                let nodes: Vec<_> = n.named_children(&mut cursor).filter(|c| c.kind() == "variable_declarator").take(MAX_VISITS).collect();
                if n.named_child_count() > MAX_VISITS { self.limit(); }
                self.block(&nodes, depth)
            }
            "variable_declarator" => self.field(n, "value", depth),
            "cast_expression" => self.field(n, "value", depth),
            "field_access" => self.field(n, "object", depth),
            "array_access" => {
                let array = self.field(n, "array", depth);
                let index = self.field(n, "index", depth);
                self.then(array, index, n)
            }
            "array_creation_expression" => self.unknown(n, "Array creation boundary: dimensions, allocation and initialization not expanded."),
            "instanceof_expression" => {
                if n.child_by_field_name("pattern").is_some() {
                    self.unknown(n, "Pattern matching boundary: deconstruction and implicit accessors not expanded.")
                } else { self.field(n, "left", depth) }
            }
            "if_statement" | "ternary_expression" => {
                let condition = self.field(n, "condition", depth);
                let mut f = Flow::empty();
                let label = format!("if {}", n.child_by_field_name("condition").map(|c| bounded(self.text(c))).unwrap_or_default());
                if let Some(mut s) = self.step(n, "branch", label) {
                    let yes = self.field(n, "consequence", depth);
                    let no = self.field(n, "alternative", depth);
                    f.continues = yes.continues || no.continues;
                    f.exits = yes.exits || no.exits;
                    s.children = yes.steps; s.alternate = no.steps; f.steps.push(s);
                }
                self.then(condition, f, n)
            }
            "while_statement" | "for_statement" | "enhanced_for_statement" => self.loop_step(n, depth),
            "binary_expression" => {
                let op = n.child_by_field_name("operator").map(|o| self.text(o)).unwrap_or("").to_owned();
                let left = self.field(n, "left", depth);
                if matches!(op.as_str(), "&&" | "||") {
                    let label = if op == "&&" { "RHS only if left is true" } else { "RHS only if left is false" };
                    let mut f = Flow::empty();
                    if let Some(mut s) = self.step(n, "branch", label) {
                        let right = self.field(n, "right", depth); f.exits = right.exits;
                        s.children = right.steps; f.steps.push(s);
                    }
                    self.then(left, f, n)
                } else {
                    let right = self.field(n, "right", depth);
                    self.then(left, right, n)
                }
            }
            "method_invocation" | "object_creation_expression" | "explicit_constructor_invocation" => self.call(n, depth),
            "return_statement" | "throw_statement" => {
                let value = self.children(n, depth);
                let mut f = self.event(n, if n.kind() == "return_statement" { "return" } else { "throw" }, &bounded(self.text(n)));
                f.exits = true; f.continues = false; self.then(value, f, n)
            }
            "assignment_expression" => {
                // Java evaluates the destination receiver/index before the RHS, also for compound assignments.
                let left = self.field(n, "left", depth);
                let right = self.field(n, "right", depth);
                let values = self.then(left, right, n);
                let effect = self.event(n, "effect", &format!("write: {}", bounded(self.text(n))));
                self.then(values, effect, n)
            }
            "update_expression" => {
                let values = self.children(n, depth);
                let effect = self.event(n, "effect", &format!("update: {}", bounded(self.text(n))));
                self.then(values, effect, n)
            }
            "identifier" | "this" | "super" | "null_literal" | "true" | "false"
            | "decimal_integer_literal" | "hex_integer_literal" | "octal_integer_literal" | "binary_integer_literal"
            | "decimal_floating_point_literal" | "hex_floating_point_literal" | "character_literal" | "string_literal"
            | "line_comment" | "block_comment" | "empty_statement" | "class_literal" => Flow::empty(),
            _ => self.unknown(n, &format!("Unsupported Java form '{}': behavior unknown.", n.kind())),
        }
    }
    fn fields(&mut self, n: Node<'_>, field: &str, depth: usize) -> Flow {
        let mut cursor = n.walk();
        let nodes: Vec<_> = n
            .children_by_field_name(field, &mut cursor)
            .take(MAX_VISITS)
            .collect();
        if nodes.len() == MAX_VISITS {
            self.limit();
        }
        self.block(&nodes, depth)
    }
    fn loop_step(&mut self, n: Node<'_>, depth: usize) -> Flow {
        // A for-continue runs updates, unlike break/return. Keep that whole loop opaque
        // until transfers can be represented with destinations rather than a boolean Flow.
        if n.kind() == "for_statement" {
            let mut cursor = n.walk();
            let mut nesting = 0;
            let mut visits = 0;
            loop {
                visits += 1;
                if visits >= MAX_VISITS {
                    self.limit();
                    return self
                        .unknown(n, "Loop transfer scan budget exceeded; behavior unknown.");
                }
                let node = cursor.node();
                if node.kind() == "continue_statement" {
                    return self.unknown(n, "For loop with continue: update/transfer paths are opaque; loop not expanded.");
                }
                let nested_definition = nesting > 0
                    && (callable(node)
                        || matches!(
                            node.kind(),
                            "class_declaration" | "class_body" | "record_declaration"
                        ));
                if !nested_definition && cursor.goto_first_child() {
                    nesting += 1;
                    continue;
                }
                loop {
                    if nesting == 0 {
                        break;
                    }
                    if cursor.goto_next_sibling() {
                        break;
                    }
                    cursor.goto_parent();
                    nesting -= 1;
                }
                if nesting == 0 {
                    break;
                }
            }
        }
        let initialization = match n.kind() {
            "for_statement" => self.fields(n, "init", depth),
            "enhanced_for_statement" => self.field(n, "value", depth),
            _ => Flow::empty(),
        };
        let mut f = Flow::empty();
        // The loop can be skipped, terminate, diverge, return or throw. Its suffix
        // is possible only on normal exit, never an unconditional next event.
        f.exits = true;
        let label = if n.kind() == "enhanced_for_statement" {
            "enhanced for: possible iterations; iterable evaluated once; implicit iterator/array protocol not expanded"
        } else {
            "loop: possible iterations, not unrolled; termination and transfers unknown"
        };
        if let Some(mut step) = self.step(n, "loop", label) {
            let condition = self.field(n, "condition", depth);
            let label = if n.kind() == "enhanced_for_statement" {
                "body only if next item exists (each iteration)"
            } else {
                "body only if condition is true (absent condition: each iteration)"
            };
            if let Some(mut branch) = self.step(n, "branch", label) {
                let body = self.field(n, "body", depth);
                let body = if n.kind() == "for_statement" && body.continues {
                    let update = self.fields(n, "update", depth);
                    self.then(body, update, n)
                } else {
                    body
                };
                branch.children = body.steps;
                let guarded = Flow {
                    steps: vec![branch],
                    ..Flow::empty()
                };
                step.children = self.then(condition, guarded, n).steps;
            }
            f.steps.push(step);
        }
        self.then(initialization, f, n)
    }
    fn call(&mut self, n: Node<'_>, depth: usize) -> Flow {
        let receiver = if n.kind() == "object_creation_expression" {
            // Qualified construction uses an unfielded primary expression (outer().new Inner()).
            let mut cursor = n.walk();
            let qualifier = n.named_children(&mut cursor).find(|c| {
                Some(*c) != n.child_by_field_name("type")
                    && Some(*c) != n.child_by_field_name("type_arguments")
                    && Some(*c) != n.child_by_field_name("arguments")
                    && !matches!(c.kind(), "class_body" | "annotation" | "marker_annotation")
            });
            qualifier
                .map(|c| self.walk(c, depth + 1))
                .unwrap_or_else(Flow::empty)
        } else {
            self.field(n, "object", depth)
        };
        let arguments = self.field(n, "arguments", depth);
        let evaluation = self.then(receiver, arguments, n);
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
        // Even Show all never resolves or descends into an unknown/third-party target.
        let mut participant = unresolved_participant(n, self.file);
        if self.view.participants.len() >= MAX_PARTICIPANTS - 1
            && !self
                .view
                .participants
                .iter()
                .any(|p| p.id == participant.id)
        {
            participant = Participant {
                declaration: None,
                id: "java:unresolved-overflow".into(),
                label: "Unresolved calls".into(),
                kind: "boundary".into(),
                identification: "Additional source hints only; no type or dispatch resolution."
                    .into(),
            };
            self.warn("Source-hint participant limit reached; additional hints share Unresolved calls. Measured call evidence is retained.");
        }
        let target = participant.id.clone();
        if !self.view.participants.iter().any(|p| p.id == target) {
            self.view.participants.push(participant);
        }
        let mut f = Flow::empty();
        if let Some(mut step) = self.step(n, "call", call_label(n, self.file)) {
            step.call_id = Some(c.id);
            step.range = c.range;
            f.steps.push(step);
        }
        self.then(evaluation, f, n)
    }
}
fn call_label(n: Node<'_>, file: &SourceFile) -> String {
    if n.kind() == "object_creation_expression" {
        format!(
            "new {}",
            n.child_by_field_name("type")
                .map(|c| bounded(&file.text[c.byte_range()]))
                .unwrap_or_else(|| "?".into())
        )
    } else {
        n.child_by_field_name("name")
            .or_else(|| n.child_by_field_name("constructor"))
            .map(|c| bounded(&file.text[c.byte_range()]))
            .unwrap_or_else(|| "?".into())
    }
}
fn unresolved_participant(n: Node<'_>, file: &SourceFile) -> Participant {
    let (kind, expression) = if let Some(receiver) = n.child_by_field_name("object") {
        ("unresolvedReceiver", &file.text[receiver.byte_range()])
    } else if let Some(callee) = n
        .child_by_field_name("name")
        .or_else(|| n.child_by_field_name("type"))
        .or_else(|| n.child_by_field_name("constructor"))
    {
        ("unresolvedCallee", &file.text[callee.byte_range()])
    } else {
        ("unresolvedCallee", "unknown")
    };
    Participant { declaration: None, id: format!("java:source:{kind}:{}", hex::encode(Sha256::digest(expression.as_bytes()))), label: bounded(expression), kind: kind.into(),
        identification: "Source spelling only; types, overloads and virtual dispatch unresolved. Visual group, not runtime object identity or a claimed return type.".into() }
}
pub(crate) fn build(
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
    ensure!(
        file.text.len() <= 8 * 1024 * 1024,
        "Java sequence parse budget exceeded (8 MiB source)"
    );
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_java::LANGUAGE.into())?;
    let tree = parser
        .parse(&file.text, None)
        .context("Java parse unavailable")?;
    let mut node = tree.root_node();
    let mut remaining = MAX_VISITS;
    // Descend only the containing range. Cursor iteration avoids collecting a wide AST to find a seed.
    while remaining > 0 {
        remaining -= 1;
        if node.start_byte() == seed.range.start_byte
            && node.end_byte() == seed.range.end_byte
            && callable(node)
        {
            break;
        }
        let mut cursor = node.walk();
        let mut found = None;
        for child in node.named_children(&mut cursor) {
            if remaining == 0 {
                break;
            }
            remaining -= 1;
            if child.start_byte() <= seed.range.start_byte
                && child.end_byte() >= seed.range.end_byte
            {
                found = Some(child);
                break;
            }
        }
        match found {
            Some(child) => node = child,
            None => break,
        }
    }
    let mut b = Builder { file, seed, calls, count: 0, visits: 0, view: SequenceView { semantic_warnings: vec![], semantic: Default::default(), incidental_steps: 0,
        revision, seed: seed.clone(), participants: vec![Participant { declaration: None, id: seed.id.clone(), label: bounded(&seed.name), kind: "method".into(), identification: "Selected source symbol.".into() }],
        steps: vec![], warnings: vec!["Static possible paths, not a runtime trace. Java types, overloads, virtual dispatch, annotations/Lombok, reflection, generated methods and implicit initialization are not resolved. Calls/operations may throw; implicit exception paths are not expanded.".into()], hidden_steps: 0, truncated: false, truncated_by: vec![],
    }};
    let f = if remaining == 0 {
        b.limit();
        b.boundary(node, "Seed lookup budget exceeded; behavior unknown.")
    } else if !callable(node)
        || node.start_byte() != seed.range.start_byte
        || node.end_byte() != seed.range.end_byte
    {
        b.boundary(
            node,
            "Indexed callable does not match cached syntax; behavior unknown.",
        )
    } else if node.has_error() {
        b.boundary(node, "Malformed Java: behavior cannot be established.")
    } else if node.child_by_field_name("body").is_none() {
        b.boundary(
            node,
            "No callable body: signature or annotation element only; behavior unavailable.",
        )
    } else {
        b.field(node, "body", 0)
    };
    b.view.steps = f.steps;
    Ok(b.view)
}
