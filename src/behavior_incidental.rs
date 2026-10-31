//! Incidental-call heuristics shared by every sequence language (#543): logging calls by
//! their spelling, and trivial accessors by their target's measured body. Both are hidden
//! under the same "incidental calls" toggle as external calls, with a labelled reason.
use crate::behavior::{IncidentalReason, SequenceStep, SequenceView};
use crate::model::{SourceFile, Symbol};
use std::collections::BTreeSet;
use tree_sitter::Node;

pub(crate) const LOGGING_WARNING: &str = "Logging calls are hidden by a naming rule (log/logger/console methods and Rust logging macros), not a proof of purity; their arguments are hidden with them. Use Show all to restore them.";

/// Whether `text`, the source of a call or macro step, is a logging call (#543): a
/// `log`/`logger`/`console`-style receiver with a logging method, or a Rust `trace!`,
/// `debug!`, `info!`, `warn!` or `error!` macro (bare or under `log::`/`tracing::`).
pub(crate) fn logging_call(text: &str) -> bool {
    let head: String = text
        .chars()
        .take_while(|c| !matches!(c, '(' | '[' | '{'))
        .take(64)
        .filter(|c| !c.is_whitespace())
        .collect();
    // `console?.log`, `this.logger?.info`: optional chaining spells the same call.
    let head = head.replace("?.", ".");
    if let Some(name) = head.strip_suffix('!') {
        let name = name.strip_prefix("::").unwrap_or(name);
        let name = name
            .strip_prefix("log::")
            .or_else(|| name.strip_prefix("tracing::"))
            .unwrap_or(name);
        return matches!(name, "trace" | "debug" | "info" | "warn" | "error");
    }
    let Some((receiver, method)) = head.rsplit_once('.') else {
        return false;
    };
    matches!(
        receiver,
        "log"
            | "logger"
            | "LOG"
            | "LOGGER"
            | "_log"
            | "_logger"
            | "self.log"
            | "self.logger"
            | "this.log"
            | "this.logger"
            | "console"
    ) && matches!(
        method,
        "trace" | "debug" | "info" | "warn" | "warning" | "error" | "fatal" | "critical" | "log"
    )
}

/// Marks the view's logging calls incidental, with the steps that evaluate their receiver
/// and arguments, and hides them unless `show_all` (see [`mark_with_operands`]).
pub(crate) fn mark_logging(view: &mut SequenceView, file: &SourceFile, show_all: bool) {
    fn mark(steps: &mut [SequenceStep], file: &SourceFile, show_all: bool) -> bool {
        let mut found = false;
        for at in 0..steps.len() {
            found |= mark(&mut steps[at].children, file, show_all);
            found |= mark(&mut steps[at].alternate, file, show_all);
            let step = &steps[at];
            if !matches!(step.kind.as_str(), "call" | "boundary") || !step.folded.is_empty() {
                continue;
            }
            let Some(text) = file.text.get(step.range.start_byte..step.range.end_byte) else {
                continue;
            };
            if logging_call(text) {
                found = true;
                mark_with_operands(steps, at, IncidentalReason::Logging, show_all, &|_| false);
            }
        }
        found
    }
    if mark(&mut view.steps, file, show_all) && !view.warnings.iter().any(|w| w == LOGGING_WARNING)
    {
        view.warnings.push(LOGGING_WARNING.into());
    }
}

/// Marks `steps[at]` incidental for `reason` and hides it unless `show_all`, with its
/// operands: the steps just before it inside its source range (its receiver and argument
/// evaluation) and everything they nest. An operand subtree that holds a call that may
/// return early (#542), or one `keep` names (a call that can expand), stays as it is.
pub(crate) fn mark_with_operands(
    steps: &mut [SequenceStep],
    at: usize,
    reason: IncidentalReason,
    show_all: bool,
    keep: &dyn Fn(&SequenceStep) -> bool,
) {
    fn kept(step: &SequenceStep, keep: &dyn Fn(&SequenceStep) -> bool) -> bool {
        !step.folded.is_empty()
            || keep(step)
            || step
                .children
                .iter()
                .chain(&step.alternate)
                .any(|s| kept(s, keep))
    }
    fn mark(step: &mut SequenceStep, reason: IncidentalReason, show_all: bool) {
        if step.incidental.is_none() || step.incidental == Some(IncidentalReason::External) {
            step.incidental = Some(reason);
        }
        step.hidden = !show_all;
        for nested in step.children.iter_mut().chain(&mut step.alternate) {
            mark(nested, reason, show_all);
        }
    }
    let range = steps[at].range.clone();
    let operands = steps[..at]
        .iter()
        .rev()
        .take_while(|s| {
            s.range.start_byte >= range.start_byte && s.range.end_byte <= range.end_byte
        })
        .count();
    for step in &mut steps[at - operands..at] {
        if !kept(step, keep) {
            mark(step, reason, show_all);
        }
    }
    mark(&mut steps[at], reason, show_all);
}

/// The IDs of the trivial accessors (#543) among `symbols`, all declared in `file`: each
/// one's body is one statement that returns a field of `self`/`this` (Rust also `&self.f`,
/// `&mut self.f`, `self.f.clone()`; Java also a bare name) or assigns a plain name to one
/// with `=`. Constructors never qualify. Measured by syntax only; `file` is parsed once.
pub(crate) fn trivial_accessors<'s>(
    file: &SourceFile,
    symbols: impl IntoIterator<Item = &'s Symbol>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    // The same source bound as the language adapters' parse budgets.
    if file.text.len() > 4 * 1024 * 1024 {
        return out;
    }
    let language: tree_sitter::Language = match file.language.as_str() {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        _ => return out,
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        return out;
    }
    let Some(tree) = parser.parse(&file.text, None) else {
        return out;
    };
    for symbol in symbols {
        if accessor_body(
            file,
            tree.root_node(),
            symbol.range.start_byte,
            symbol.range.end_byte,
        ) {
            out.insert(symbol.id.clone());
        }
    }
    out
}

/// Whether the declaration spanning `start..end` of `file` has a trivial accessor body.
fn accessor_body(file: &SourceFile, root: Node<'_>, start: usize, end: usize) -> bool {
    let text = |n: Node<'_>| file.text.get(n.byte_range()).unwrap_or("");
    let Some(mut node) = root.descendant_for_byte_range(start, end) else {
        return false;
    };
    while node.start_byte() != start
        || node.end_byte() != end
        || node.child_by_field_name("body").is_none()
    {
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.start_byte() < start || parent.end_byte() > end {
            return false;
        }
        node = parent;
    }
    // Object creation is not an accessor, however small its body.
    let name = node.child_by_field_name("name").map(text);
    if node.kind() == "constructor_declaration"
        || (file.language == "javascript" && name == Some("constructor"))
        || (file.language == "python" && name == Some("__init__"))
    {
        return false;
    }
    let Some(body) = node.child_by_field_name("body") else {
        return false;
    };
    let mut cursor = body.walk();
    let mut statements: Vec<Node<'_>> = body
        .named_children(&mut cursor)
        .filter(|n| !n.is_extra())
        .collect();
    // A Python docstring before the statement is documentation, not behavior.
    if file.language == "python"
        && statements.len() == 2
        && statements[0].kind() == "expression_statement"
        && statements[0]
            .named_child(0)
            .is_some_and(|s| s.kind() == "string")
    {
        statements.remove(0);
    }
    let [statement] = statements[..] else {
        return false;
    };
    // `self.f` / `this.f`: a field of the receiver.
    let field = |n: Node<'_>| -> bool {
        match n.kind() {
            "field_expression" => n
                .child_by_field_name("value")
                .is_some_and(|v| v.kind() == "self"),
            "field_access" => n
                .child_by_field_name("object")
                .is_some_and(|v| v.kind() == "this"),
            "attribute" => n
                .child_by_field_name("object")
                .is_some_and(|v| text(v) == "self"),
            "member_expression" => n
                .child_by_field_name("object")
                .is_some_and(|v| v.kind() == "this"),
            _ => false,
        }
    };
    let value = |n: Node<'_>| -> bool {
        if field(n) || (file.language == "java" && n.kind() == "identifier") {
            return true;
        }
        match n.kind() {
            // `&self.f`, `&mut self.f`
            "reference_expression" => n.child_by_field_name("value").is_some_and(field),
            // `self.f.clone()`
            "call_expression" if file.language == "rust" => {
                n.child_by_field_name("arguments")
                    .is_some_and(|a| a.named_child_count() == 0)
                    && n.child_by_field_name("function").is_some_and(|f| {
                        f.kind() == "field_expression"
                            && f.child_by_field_name("field")
                                .is_some_and(|m| text(m) == "clone")
                            && f.child_by_field_name("value").is_some_and(field)
                    })
            }
            _ => false,
        }
    };
    // Plain `=` only: Java's `assignment_expression` also spells `+=` and the others.
    let assignment = |n: Node<'_>| -> bool {
        matches!(n.kind(), "assignment_expression" | "assignment")
            && n.child_by_field_name("operator")
                .is_none_or(|o| text(o) == "=")
            && n.child_by_field_name("left")
                .is_some_and(|l| field(l) || (file.language == "java" && l.kind() == "identifier"))
            && n.child_by_field_name("right")
                .is_some_and(|r| r.kind() == "identifier")
    };
    let returned = |n: Node<'_>| -> bool {
        matches!(n.kind(), "return_statement" | "return_expression")
            && n.named_child(0).is_some_and(value)
            && n.named_child_count() == 1
    };
    match statement.kind() {
        "return_statement" => returned(statement),
        "expression_statement" => statement
            .named_child(0)
            .is_some_and(|e| statement.named_child_count() == 1 && (assignment(e) || returned(e))),
        // A Rust tail expression, a setter's included.
        _ => {
            file.language == "rust"
                && (value(statement) || returned(statement) || assignment(statement))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{accessor_body, logging_call};
    use crate::model::SourceFile;

    /// Whether the declaration starting at `decl` (and ending at its first closing brace)
    /// in a Java class is a trivial accessor.
    fn java(class: &str, decl: &str) -> bool {
        let file = SourceFile {
            path: "S.java".into(),
            hash: String::new(),
            language: "java".into(),
            text: class.into(),
        };
        let start = class.find(decl).unwrap();
        let end = start + class[start..].find('}').unwrap() + 1;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(class, None).unwrap();
        accessor_body(&file, tree.root_node(), start, end)
    }

    #[test]
    fn constructors_and_compound_assignment_are_not_accessors() {
        let class = "class S {\n  int x;\n  S(int v) { this.x = v; }\n  void add(int v) { this.x += v; }\n  void set(int v) { this.x = v; }\n  int get() { return x; }\n}\n";
        assert!(!java(class, "S(int v)"));
        assert!(!java(class, "void add"));
        assert!(java(class, "void set"));
        assert!(java(class, "int get"));
    }

    #[test]
    fn logging_spellings() {
        for text in [
            "trace!(\"x\")",
            "tracing::warn!(\"x\")",
            "::log::error!(\"x\")",
            "log::debug!(\"x\")",
            "info!{ \"x\" }",
            "console?.log(1)",
            "this.logger?.info(1)",
            "_logger.critical(1)",
            "self.log.warning(1)",
        ] {
            assert!(logging_call(text), "{text}");
        }
        for text in [
            "println!(\"x\")",
            "format!(\"x\")",
            "logger.send(\"x\")",
            "dialog.info(\"x\")",
            "a.logger.info(1)",
            "info(1)",
        ] {
            assert!(!logging_call(text), "{text}");
        }
    }
}
