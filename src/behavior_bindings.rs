//! Syntactic participant identification only. Never resolves a call implementation.
//! All analysis uses the already parsed, cached JavaScript source.
use crate::behavior::Participant;
use std::collections::HashSet;
use tree_sitter::Node;

struct Binding {
    name: String,
    start: usize,
    end: usize,
    declaration: usize,
    import: Option<(String, String)>,
}
pub(crate) struct Bindings {
    bindings: Vec<Binding>,
    // Writes are deliberately file-wide: even a nested closure could mutate a global.
    writes: HashSet<String>,
    unsafe_globals: bool,
}
fn children(n: Node<'_>) -> Vec<Node<'_>> {
    let mut c = n.walk();
    n.named_children(&mut c).collect()
}
fn function(n: Node<'_>) -> bool {
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
fn scope(mut n: Node<'_>, var: bool) -> Node<'_> {
    loop {
        if n.kind() == "program"
            || function(n)
            || (!var
                && matches!(
                    n.kind(),
                    "statement_block"
                        | "for_statement"
                        | "for_in_statement"
                        | "catch_clause"
                        | "switch_body"
                        | "class_body"
                ))
        {
            return n;
        }
        match n.parent() {
            Some(p) => n = p,
            None => return n,
        }
    }
}
fn text<'a>(source: &'a str, n: Node<'_>) -> &'a str {
    &source[n.byte_range()]
}
fn root(mut n: Node<'_>) -> Option<Node<'_>> {
    loop {
        match n.kind() {
            "identifier" => return Some(n),
            "member_expression" | "subscript_expression" => n = n.child_by_field_name("object")?,
            "parenthesized_expression" => n = *children(n).first()?,
            _ => return None,
        }
    }
}
impl Bindings {
    pub(crate) fn new(tree: Node<'_>, source: &str) -> Self {
        let mut out = Self {
            bindings: vec![],
            writes: HashSet::new(),
            unsafe_globals: tree.has_error(),
        };
        let mut todo = vec![tree];
        while let Some(n) = todo.pop() {
            // Escaped identifiers need ECMAScript name decoding. Until supported,
            // suppress proof rather than miss an escaped shadow/write target.
            if matches!(
                n.kind(),
                "identifier" | "shorthand_property_identifier_pattern"
            ) && text(source, n).contains('\\')
            {
                out.unsafe_globals = true;
            }
            // Any syntactic eval reference may escape or be parenthesized/aliased.
            // Fail closed rather than attempt to prove whether eval stays direct.
            if matches!(n.kind(), "identifier" | "property_identifier") && text(source, n) == "eval"
            {
                out.unsafe_globals = true;
            }
            if n.kind() == "subscript_expression"
                && n.child_by_field_name("index")
                    .is_some_and(|index| matches!(text(source, index), "\"eval\"" | "'eval'"))
            {
                out.unsafe_globals = true;
            }
            match n.kind() {
                "import_statement" => {
                    if let Some(module) = n.child_by_field_name("source") {
                        let raw = text(source, module);
                        // Escape decoding is deliberately unsupported; do not invent a module name.
                        if raw.contains('\\') {
                            // Still record shadowing even when module spelling is unsupported.
                            for clause in children(n)
                                .into_iter()
                                .filter(|x| x.kind() == "import_clause")
                            {
                                out.pattern(clause, tree, source);
                            }
                        }
                        if raw.len() >= 2 && !raw.contains('\\') {
                            let module = &raw[1..raw.len() - 1];
                            for clause in children(n)
                                .into_iter()
                                .filter(|x| x.kind() == "import_clause")
                            {
                                for item in children(clause) {
                                    match item.kind() {
                                        "identifier" => out.add(
                                            item,
                                            tree,
                                            Some((module.into(), "default".into())),
                                            source,
                                        ),
                                        "namespace_import" => {
                                            if let Some(id) = children(item)
                                                .into_iter()
                                                .find(|x| x.kind() == "identifier")
                                            {
                                                out.add(
                                                    id,
                                                    tree,
                                                    Some((module.into(), "*".into())),
                                                    source,
                                                );
                                            }
                                        }
                                        "named_imports" => {
                                            for spec in children(item) {
                                                if let Some(name) = spec.child_by_field_name("name")
                                                {
                                                    let local = spec
                                                        .child_by_field_name("alias")
                                                        .unwrap_or(name);
                                                    out.add(
                                                        local,
                                                        tree,
                                                        Some((
                                                            module.into(),
                                                            text(source, name).into(),
                                                        )),
                                                        source,
                                                    );
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
                "variable_declarator" => {
                    if let Some(name) = n.child_by_field_name("name") {
                        let var = n
                            .parent()
                            .is_some_and(|p| p.kind() == "variable_declaration");
                        out.pattern(name, scope(n, var), source);
                    }
                }
                "catch_clause" => {
                    if let Some(p) = n.child_by_field_name("parameter") {
                        out.pattern(p, n, source);
                    }
                }
                "class_declaration" | "class" => {
                    if let Some(name) = n.child_by_field_name("name") {
                        out.add(name, scope(n.parent().unwrap_or(tree), false), None, source);
                    }
                }
                "for_in_statement" => {
                    // Grammar places `let x` directly in the loop rather than a declarator.
                    if n.child_by_field_name("kind").is_some() {
                        if let Some(left) = n.child_by_field_name("left") {
                            let var = n
                                .child_by_field_name("kind")
                                .is_some_and(|k| text(source, k) == "var");
                            out.pattern(left, scope(n, var), source);
                        }
                    } else if let Some(left) = n.child_by_field_name("left") {
                        out.write(left, source);
                    }
                }
                "assignment_expression" | "augmented_assignment_expression" => {
                    if let Some(left) = n.child_by_field_name("left") {
                        out.write(left, source);
                    }
                }
                "update_expression" => {
                    if let Some(arg) = n.child_by_field_name("argument") {
                        out.write(arg, source);
                    }
                }
                "unary_expression" if text(source, n).trim_start().starts_with("delete ") => {
                    if let Some(arg) = n.child_by_field_name("argument") {
                        out.write(arg, source);
                    }
                }
                "with_statement" => out.unsafe_globals = true,
                "call_expression"
                    if n.child_by_field_name("function")
                        .is_some_and(|f| text(source, f) == "eval") =>
                {
                    out.unsafe_globals = true;
                }
                _ => {}
            }
            if function(n) {
                if let Some(p) = n
                    .child_by_field_name("parameters")
                    .or_else(|| n.child_by_field_name("parameter"))
                {
                    out.pattern(p, n, source);
                }
                if let Some(name) = n.child_by_field_name("name") {
                    if matches!(
                        n.kind(),
                        "function_declaration" | "generator_function_declaration"
                    ) {
                        out.add(name, scope(n.parent().unwrap_or(tree), false), None, source);
                        // Sloppy-script Annex B may also expose block functions in
                        // the enclosing function. Conservatively allow that shadow.
                        out.add(name, scope(n.parent().unwrap_or(tree), true), None, source);
                    }
                    out.add(name, n, None, source);
                }
            }
            todo.extend(children(n));
        }
        out
    }
    fn add(&mut self, n: Node<'_>, s: Node<'_>, import: Option<(String, String)>, source: &str) {
        self.bindings.push(Binding {
            name: text(source, n).into(),
            start: s.start_byte(),
            end: s.end_byte(),
            declaration: n.start_byte(),
            import,
        });
    }
    // Iterative, like `write`: repository nesting has no bound here.
    fn pattern(&mut self, n: Node<'_>, s: Node<'_>, source: &str) {
        let mut pending = vec![n];
        while let Some(n) = pending.pop() {
            match n.kind() {
                "identifier" | "shorthand_property_identifier_pattern" => {
                    self.add(n, s, None, source)
                }
                "assignment_pattern" | "object_assignment_pattern" => {
                    pending.extend(n.child_by_field_name("left"));
                }
                "pair_pattern" => pending.extend(n.child_by_field_name("value")),
                _ => pending.extend(children(n).into_iter().rev()),
            }
        }
    }
    fn write(&mut self, n: Node<'_>, source: &str) {
        let mut pending = vec![n];
        while let Some(n) = pending.pop() {
            if let Some(id) = root(n) {
                let name = text(source, id);
                self.writes.insert(name.into());
                if matches!(name, "globalThis" | "window" | "global" | "self") {
                    self.unsafe_globals = true;
                }
                continue;
            }
            // Destructuring write targets: ignore RHS/default expressions only when known.
            match n.kind() {
                "assignment_pattern" | "object_assignment_pattern" => {
                    pending.extend(n.child_by_field_name("left"));
                }
                "pair_pattern" => pending.extend(n.child_by_field_name("value")),
                "shorthand_property_identifier_pattern" => {
                    self.writes.insert(text(source, n).into());
                }
                _ => pending.extend(children(n).into_iter().rev()),
            }
        }
    }
    pub(crate) fn identify(&self, call: Node<'_>, source: &str, path: &str) -> Option<Participant> {
        let callee = call
            .child_by_field_name("function")
            .or_else(|| call.child_by_field_name("constructor"))?;
        // Computed names and chained return values are not static member evidence.
        let mut receiver = callee;
        while receiver.kind() == "member_expression" {
            let property = receiver.child_by_field_name("property")?;
            if property.kind() != "property_identifier" {
                return None;
            }
            receiver = receiver.child_by_field_name("object")?;
        }
        if receiver.kind() != "identifier" {
            return None;
        }
        let name = text(source, receiver);
        let binding = self
            .bindings
            .iter()
            .filter(|b| b.name == name && b.start <= call.start_byte() && b.end >= call.end_byte())
            .min_by_key(|b| (b.end - b.start, b.declaration));
        let clean = !self.unsafe_globals && !self.writes.contains(name);
        if clean {
            if let Some((module, symbol)) = binding.and_then(|b| b.import.as_ref()) {
                return Some(Participant {
                    declaration: None,
                    id: format!("import:{}:{}", module.len(), module),
                    label: module.clone(),
                    kind: "import".into(),
                    identification: format!(
                        "Static import module; {name} imports {symbol} from {module}. Lane groups module bindings, not objects or resolved implementations."
                    ),
                });
            }
            if binding.is_none()
                && matches!(
                    name,
                    "JSON"
                        | "Math"
                        | "Object"
                        | "Array"
                        | "String"
                        | "Number"
                        | "Boolean"
                        | "BigInt"
                        | "Symbol"
                        | "Reflect"
                        | "Promise"
                        | "Date"
                        | "RegExp"
                        | "Error"
                )
            {
                return Some(Participant { declaration: None, id:format!("builtin:{name}"), label:name.into(), kind:"builtin".into(), identification:"Unshadowed standard JavaScript global in cached syntax; no detected writes. Member implementation and runtime identity are not resolved.".into() });
            }
        }
        if callee.kind() == "member_expression" || (name == "Buffer" && binding.is_none() && clean)
        {
            return Some(Participant {
                declaration: None,
                id: format!(
                    "receiver:{}:{}:{}:{name}",
                    path.len(),
                    path,
                    binding.map_or(0, |b| b.declaration + 1)
                ),
                label: name.into(),
                kind: "receiver".into(),
                identification: if name == "Buffer" && binding.is_none() {
                    "Receiver-name hint; Buffer may be a Node.js runtime global, but the runtime is not established.".into()
                } else {
                    "Receiver-name hint only; type, object identity, and member implementation are unknown.".into()
                },
            });
        }
        None
    }
}

#[cfg(test)]
mod deep_input_tests {
    use super::*;

    #[test]
    fn extremely_deep_patterns_and_targets_do_not_recurse() {
        let depth = 200_000;
        let source = format!(
            "let {}x{} = 1; {}y{} = 2; a{} = 3;",
            "[".repeat(depth),
            "]".repeat(depth),
            "[".repeat(depth),
            "]".repeat(depth),
            ".b".repeat(depth)
        );
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(&source, None).unwrap();
        let bindings = Bindings::new(tree.root_node(), &source);
        assert!(bindings.writes.contains("y") && bindings.writes.contains("a"));
    }
}
