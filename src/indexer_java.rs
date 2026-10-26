//! Java syntax-category policy for the validated native-only extractor.
use tree_sitter::Node;

/// Closed #22 syntax categories measured by the Java adapter, not semantic types.
pub(crate) fn native_kind(n: Node<'_>) -> Option<&'static str> {
    Some(match n.kind() {
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration" => "type",
        "method_declaration" | "annotation_type_element_declaration" => "method",
        "constructor_declaration" | "compact_constructor_declaration" => "constructor",
        "lambda_expression" => "anonymousFunction",
        "variable_declarator" if n.parent().is_some_and(|p| p.kind() == "spread_parameter") => {
            return None;
        }
        "variable_declarator" if n.parent().is_some_and(|p| p.kind() == "field_declaration") => {
            "field"
        }
        "variable_declarator" => "variable",
        "formal_parameter" | "spread_parameter" => "parameter",
        _ => return None,
    })
}
