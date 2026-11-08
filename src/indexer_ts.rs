//! TypeScript grammar choice and syntax-category policy for the native extractor.
use tree_sitter::{Language, Node};

/// The TSX grammar for a `.tsx` path, otherwise the TypeScript grammar. The language is
/// always `typescript`; only the parser depends on the path.
pub(crate) fn grammar(path: &str) -> Language {
    if path.ends_with(".tsx") {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    }
}

/// A declaration file holds ambient declarations only (`.d.ts`, `.d.mts`, `.d.cts`). The
/// match is case-sensitive, like the extension match that admits the document.
pub(crate) fn declaration_only(path: &str) -> bool {
    path.ends_with(".d.ts") || path.ends_with(".d.mts") || path.ends_with(".d.cts")
}

/// The body of a function, method, arrow or generator, or a class static block. A
/// declaration-only document measures nothing inside one, and the skip is not a gap.
pub(crate) fn is_executable_body(n: Node<'_>) -> bool {
    match n.kind() {
        "class_static_block" => true,
        "statement_block" => n.parent().is_some_and(|parent| {
            matches!(
                parent.kind(),
                "function_declaration"
                    | "function_expression"
                    | "generator_function_declaration"
                    | "generator_function"
                    | "method_definition"
                    | "arrow_function"
            )
        }),
        _ => false,
    }
}

/// The declaration kind of a TypeScript-only node, or else of a shared JavaScript one, in
/// the document `text`. A parameter declares a field only as a parameter property, in a
/// constructor.
pub(crate) fn native_ts_kind(n: Node<'_>, text: &str) -> Option<&'static str> {
    Some(match n.kind() {
        "abstract_class_declaration" | "interface_declaration" | "enum_declaration" => "type",
        "type_alias_declaration" => "alias",
        "internal_module" | "module" => "namespace",
        "function_signature" => "function",
        "method_signature" | "abstract_method_signature" => "method",
        "public_field_definition" | "property_signature" => "field",
        "required_parameter" | "optional_parameter" if is_parameter_property(n, text) => "field",
        _ => return crate::indexer::native_js_kind(n),
    })
}

/// A constructor parameter with an accessibility, `override` or `readonly` modifier.
fn is_parameter_property(n: Node<'_>, text: &str) -> bool {
    let mut cursor = n.walk();
    let modified = n.children(&mut cursor).any(|child| {
        matches!(
            child.kind(),
            "accessibility_modifier" | "override_modifier" | "readonly"
        )
    });
    modified
        && n.parent()
            .and_then(|list| list.parent())
            .filter(|method| method.kind() == "method_definition")
            .and_then(|method| method.child_by_field_name("name"))
            .is_some_and(|name| {
                name.kind() == "property_identifier"
                    && text.get(name.byte_range()) == Some("constructor")
            })
}

/// The node that names a TypeScript parameter or namespace, when the kind has its own rule:
/// `Some(None)` is decided as nameless. A parameter is named by an identifier `pattern`. A
/// namespace by its identifier, the rightmost segment of a dotted name, or the text of a
/// string; `declare global` is no namespace node and so declares nothing.
pub(crate) fn name_node(n: Node<'_>) -> Option<Option<Node<'_>>> {
    match n.kind() {
        "required_parameter" | "optional_parameter" => Some(
            n.child_by_field_name("pattern")
                .filter(|pattern| pattern.kind() == "identifier"),
        ),
        "internal_module" | "module" => {
            Some(
                n.child_by_field_name("name")
                    .and_then(|name| match name.kind() {
                        "identifier" => Some(name),
                        "nested_identifier" => name.child_by_field_name("property"),
                        "string" => {
                            let mut cursor = name.walk();
                            name.named_children(&mut cursor)
                                .find(|child| child.kind() == "string_fragment")
                        }
                        _ => None,
                    }),
            )
        }
        _ => None,
    }
}

/// A type annotation (`: T`, `: x is T`, `: asserts x`) measures as the type it wraps.
pub(crate) fn unwrap_annotation(n: Node<'_>) -> Node<'_> {
    if matches!(
        n.kind(),
        "type_annotation" | "type_predicate_annotation" | "asserts_annotation"
    ) {
        n.named_child(0).unwrap_or(n)
    } else {
        n
    }
}

/// A call written in type syntax, such as `typeof import("m")` or `import("m").T`. Types
/// are erased, so it is no invocation. Type syntax never encloses runtime code, so the walk
/// stops at the first statement, block or class body.
pub(crate) fn in_type_context(n: Node<'_>) -> bool {
    let mut child = n;
    while let Some(parent) = child.parent() {
        match parent.kind() {
            "type_annotation"
            | "type_predicate_annotation"
            | "asserts_annotation"
            | "opting_type_annotation"
            | "omitting_type_annotation"
            | "adding_type_annotation"
            | "type_alias_declaration"
            | "type_arguments"
            | "type_parameters"
            | "type_query"
            | "interface_declaration"
            | "implements_clause"
            | "extends_type_clause" => return true,
            // `x as T` and `x satisfies T`: the type follows the expression.
            "as_expression" | "satisfies_expression" => {
                return parent
                    .named_child(0)
                    .is_some_and(|value| value.id() != child.id());
            }
            "program" | "statement_block" | "class_body" => return false,
            kind if kind.ends_with("_statement") => return false,
            _ => child = parent,
        }
    }
    false
}
