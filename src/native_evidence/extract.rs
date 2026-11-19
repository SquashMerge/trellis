//! Per-document native extraction: `extract_document`, the `ParserState` walk with its
//! bounds and macro sites, and the node classification, naming and header helpers that
//! only the walk uses.
#[cfg(test)]
use super::DOCUMENT_EXTRACTIONS;
use super::{
    awaited_callee, declaration_lookup, hash, in_macro_expansion, js_call_range, language_order,
    native_extraction_context, occurrence_input, rust_test_attribute, safe_path, test_path,
};
use crate::model::SourceFile;
use crate::native_ids::{IdentityRegistry, canonical, digest};
use crate::native_records::{
    Artifact, Call, CapabilityReport, ControlRegion, Coverage, Declaration, DeclarationRole,
    Document, DocumentKey, Header, Key, MeasurementGap, MeasurementKind, MeasurementSupport,
    PRODUCER, Parameter, Provenance, Range, Reference, Signature, lookup,
};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

fn kind(lang: &str, n: Node<'_>, text: &str) -> Option<&'static str> {
    match lang {
        "java" => crate::indexer_java::native_kind(n),
        "rust" => crate::indexer_rust::native_kind(n),
        "python" => crate::indexer_python::native_kind(n),
        "javascript" => crate::indexer::native_js_kind(n),
        "typescript" => crate::indexer_ts::native_ts_kind(n, text),
        _ => None,
    }
}
fn is_call(lang: &str, n: Node<'_>) -> bool {
    matches!(
        (lang, n.kind()),
        (
            "java",
            "method_invocation" | "object_creation_expression" | "explicit_constructor_invocation"
        ) | ("rust", "call_expression" | "method_call_expression")
            | ("python", "call")
            | ("javascript", "call_expression" | "new_expression")
    ) || (lang == "typescript"
        && matches!(n.kind(), "call_expression" | "new_expression")
        && !crate::indexer_ts::in_type_context(n))
}
/// A stored control region: each grammar's branches (ternaries included), loops, switches
/// and matches, `try` statements and `catch`/`except` handlers, and the `else`, `elif`,
/// `case`, `default` and `finally` arms its grammar has as nodes (#1120 audit).
fn is_region(n: Node<'_>) -> bool {
    matches!(
        n.kind(),
        "if_statement"
            | "if_expression"
            | "else_clause"
            | "elif_clause"
            | "while_statement"
            | "while_expression"
            | "do_statement"
            | "for_statement"
            | "enhanced_for_statement"
            | "for_in_statement"
            | "for_expression"
            | "loop_expression"
            | "match_expression"
            | "match_statement"
            | "switch_statement"
            | "switch_expression"
            | "switch_case"
            | "switch_default"
            | "try_statement"
            | "try_with_resources_statement"
            | "catch_clause"
            | "except_clause"
            | "finally_clause"
            | "conditional_expression"
            | "ternary_expression"
    )
}
fn declaration_role(test: bool) -> DeclarationRole {
    if test {
        DeclarationRole::Test
    } else {
        DeclarationRole::Production
    }
}
/// An inner `#![cfg(test)]`-style attribute among `n`'s own children marks
/// its enclosing file or module body as test code.
fn rust_inner_test(n: Node<'_>, file: &SourceFile) -> bool {
    child_nodes(n)
        .into_iter()
        .any(|child| child.kind() == "inner_attribute_item" && rust_test_attribute(child, file))
}
fn java_test_method(n: Node<'_>, file: &SourceFile) -> bool {
    if n.kind() != "method_declaration" {
        return false;
    }
    let Some(modifiers) = child_nodes(n).into_iter().find(|c| c.kind() == "modifiers") else {
        return false;
    };
    child_nodes(modifiers)
        .into_iter()
        .filter(|c| matches!(c.kind(), "annotation" | "marker_annotation"))
        .any(|annotation| {
            let Some(name) = annotation
                .child_by_field_name("name")
                .or_else(|| child_nodes(annotation).into_iter().next())
            else {
                return false;
            };
            let Some(value) = file.text.get(name.start_byte()..name.end_byte()) else {
                return false;
            };
            matches!(
                value.rsplit('.').next(),
                Some(
                    "Test" | "ParameterizedTest" | "RepeatedTest" | "TestFactory" | "TestTemplate"
                )
            )
        })
}
fn local_test_marker(n: Node<'_>, file: &SourceFile) -> bool {
    match file.language.as_str() {
        "java" => java_test_method(n, file),
        "python" if test_path(&file.path) => {
            let named = name(n, file).map(|(name, _)| name);
            (matches!(n.kind(), "function_definition")
                && named.as_deref().is_some_and(|s| s.starts_with("test_")))
                || (n.kind() == "class_definition"
                    && named.as_deref().is_some_and(|s| s.starts_with("Test")))
        }
        "rust" => match n.kind() {
            "source_file" | "declaration_list" => rust_inner_test(n, file),
            "mod_item" => n
                .child_by_field_name("body")
                .is_some_and(|body| rust_inner_test(body, file)),
            _ => false,
        },
        _ => false,
    }
}
fn child_nodes(n: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = n.walk();
    n.named_children(&mut cursor).collect()
}
fn name(n: Node<'_>, f: &SourceFile) -> Option<(String, Range)> {
    if f.language == "rust"
        && n.kind() == "use_as_clause"
        && n.child_by_field_name("alias")
            .and_then(|alias| f.text.get(alias.byte_range()))
            == Some("_")
    {
        return None;
    }
    let typescript_name = if f.language == "typescript" {
        crate::indexer_ts::name_node(n)
    } else {
        None
    };
    let node = if let Some(decided) = typescript_name {
        decided
    } else if matches!(n.kind(), "import_specifier" | "use_as_clause") {
        n.child_by_field_name("alias")
    } else {
        n.child_by_field_name("name")
    }
    .or_else(|| {
        (n.kind() == "assignment")
            .then(|| n.child_by_field_name("left"))
            .flatten()
    })
    .or_else(|| {
        (n.kind() == "field_definition")
            .then(|| n.child_by_field_name("property"))
            .flatten()
    })
    .or_else(|| {
        (n.kind() == "parameter" && n.named_child_count() > 0)
            .then(|| n.named_child(0))
            .flatten()
    })
    .or_else(|| {
        matches!(n.kind(), "formal_parameter" | "typed_parameter")
            .then(|| n.named_child(0))
            .flatten()
    })
    .or_else(|| {
        (n.kind() == "spread_parameter")
            .then(|| java_parameter_parts(n).1)
            .flatten()
    })
    .or_else(|| (n.kind() == "identifier").then_some(n))?;
    if !matches!(
        node.kind(),
        "identifier"
            | "property_identifier"
            | "field_identifier"
            | "type_identifier"
            | "string_fragment"
    ) {
        return None;
    }
    let value = f.text.get(node.byte_range())?;
    if value.is_empty() {
        return None;
    }
    Some((value.to_owned(), Range::node(node)))
}
fn java_parameter_parts(n: Node<'_>) -> (Option<Node<'_>>, Option<Node<'_>>) {
    let type_node = n.child_by_field_name("type").or_else(|| {
        (n.kind() == "spread_parameter")
            .then(|| {
                child_nodes(n).into_iter().find(|child| {
                    !matches!(
                        child.kind(),
                        "modifiers" | "annotation" | "marker_annotation" | "variable_declarator"
                    )
                })
            })
            .flatten()
    });
    let name_node = n.child_by_field_name("name").or_else(|| {
        (n.kind() == "spread_parameter")
            .then(|| {
                child_nodes(n)
                    .into_iter()
                    .find(|child| child.kind() == "variable_declarator")
                    .and_then(|declarator| declarator.child_by_field_name("name"))
            })
            .flatten()
    });
    (type_node, name_node)
}
fn java_signature(n: Node<'_>, f: &SourceFile) -> Signature {
    let parameters = n
        .child_by_field_name("parameters")
        .map(child_nodes)
        .unwrap_or_default();
    let mut types = vec![];
    let mut variadic = false;
    for parameter in parameters {
        if !parameter.kind().contains("parameter") {
            continue;
        }
        if let Some(t) = java_parameter_parts(parameter)
            .0
            .and_then(|part| f.text.get(part.byte_range()))
        {
            types.push(t.to_owned());
        }
        if parameter.kind().contains("spread") {
            variadic = true;
        }
    }
    let type_parameter_count = n
        .child_by_field_name("type_parameters")
        .map(child_nodes)
        .unwrap_or_default()
        .len();
    Signature {
        parameter_types: types,
        type_parameter_count,
        variadic,
    }
}
fn key_of(n: Node<'_>, f: &SourceFile, k: &str) -> Key {
    Key {
        kind: k.into(),
        name: name(n, f).map(|v| v.0),
        signature: if f.language == "java" && (k == "method" || k == "constructor") {
            Some(java_signature(n, f))
        } else {
            None
        },
        ordinal: 0,
    }
}
fn modifier_keyword(value: &str) -> bool {
    matches!(
        value,
        "public"
            | "protected"
            | "private"
            | "static"
            | "abstract"
            | "final"
            | "native"
            | "synchronized"
            | "volatile"
            | "transient"
            | "strictfp"
            | "default"
            | "async"
            | "const"
            | "unsafe"
            | "extern"
            | "override"
            | "get"
            | "set"
            | "readonly"
    )
}
/// A TypeScript type annotation measures as the type it wraps, without the `: `.
fn annotated<'t>(f: &SourceFile, node: Node<'t>) -> Node<'t> {
    if f.language == "typescript" {
        crate::indexer_ts::unwrap_annotation(node)
    } else {
        node
    }
}
fn header_with_witnesses(
    n: Node<'_>,
    f: &SourceFile,
    key: &Key,
    _witness: &mut impl FnMut(String, Range),
) -> Header {
    let from = |node: Node<'_>| f.text.get(node.byte_range()).map(str::to_owned);
    macro_rules! measured {
        ($field:expr, $node:expr) => {{
            let node = $node;
            let _field = || -> String { $field };
            from(node).inspect(|_value| {
                #[cfg(feature = "test-support")]
                _witness(_field(), Range::node(node));
            })
        }};
    }
    let mut modifier_tokens = Vec::new();
    let mut cursor = n.walk();
    for child in n.children(&mut cursor) {
        match child.kind() {
            "visibility_modifier" | "accessibility_modifier" | "override_modifier" => {
                if let Some(value) = from(child) {
                    modifier_tokens.push((child, value));
                }
            }
            "modifiers" | "function_modifiers" => {
                let mut nested_cursor = child.walk();
                for token in child.children(&mut nested_cursor) {
                    if let Some(value) = from(token)
                        && (token.is_named() || modifier_keyword(&value))
                    {
                        modifier_tokens.push((token, value));
                    }
                }
            }
            _ if !child.is_named() => {
                if let Some(value) = from(child)
                    && modifier_keyword(&value)
                {
                    modifier_tokens.push((child, value));
                }
            }
            _ => {}
        }
    }
    modifier_tokens.sort_by_key(|(node, _)| (node.start_byte(), node.end_byte()));
    let modifiers = modifier_tokens
        .into_iter()
        .enumerate()
        .filter_map(|(_i, (node, _))| measured!(format!("header.modifiers[{_i}]"), node))
        .collect();
    let type_parameters = n
        .child_by_field_name("type_parameters")
        .map(child_nodes)
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .filter_map(|(_i, node)| measured!(format!("header.typeParameters[{_i}]"), node))
        .collect();
    let parameters = n
        .child_by_field_name("parameters")
        .map(child_nodes)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| {
            p.kind().contains("parameter")
                || matches!(p.kind(), "identifier" | "rest_pattern" | "self_parameter")
        })
        .enumerate()
        .map(|(_i, p)| {
            let pname = (if f.language == "java" {
                java_parameter_parts(p).1
            } else {
                p.child_by_field_name("name")
            })
            .or_else(|| {
                child_nodes(p)
                    .into_iter()
                    .find(|c| c.kind() == "identifier")
            })
            .or_else(|| (p.kind() == "identifier").then_some(p))
            .and_then(|node| measured!(format!("header.parameters[{_i}].name"), node));
            let type_name = (if f.language == "java" {
                java_parameter_parts(p).0
            } else {
                p.child_by_field_name("type")
            })
            .map(|node| annotated(f, node))
            .and_then(|node| measured!(format!("header.parameters[{_i}].type"), node));
            Parameter {
                name: pname,
                type_name,
                variadic: p.kind().contains("spread")
                    || p.kind().contains("rest")
                    || f.text
                        .get(p.byte_range())
                        .is_some_and(|s| s.starts_with('*') || s.starts_with("...")),
            }
        })
        .collect();
    let result_type = n
        .child_by_field_name("return_type")
        .or_else(|| {
            (f.language == "java" && key.kind == "method")
                .then(|| n.child_by_field_name("type"))
                .flatten()
        })
        .map(|node| annotated(f, node))
        .and_then(|node| measured!("header.resultType".into(), node));
    let bases = [
        "superclass",
        "interfaces",
        "super_interfaces",
        "traits",
        "trait",
    ]
    .into_iter()
    .flat_map(|field| n.child_by_field_name(field))
    .enumerate()
    .filter_map(|(_i, node)| measured!(format!("header.bases[{_i}]"), node))
    .collect();
    Header {
        kind: key.kind.clone(),
        name: key.name.clone(),
        modifiers,
        type_parameters,
        parameters,
        result_type,
        bases,
    }
}
fn callee(n: Node<'_>, f: &SourceFile) -> (Option<String>, Option<Range>) {
    let expr = n
        .child_by_field_name(match f.language.as_str() {
            "java" => "name",
            "python" => "function",
            "javascript" | "typescript" if n.kind() == "new_expression" => "constructor",
            "javascript" | "typescript" => "function",
            _ => "function",
        })
        .or_else(|| n.child_by_field_name("constructor"));
    let Some(expr) = expr else {
        return (None, None);
    };
    // The `await f<T>(x)` misparse names the awaited callee `f` (#958).
    let expr = match f.language.as_str() {
        "javascript" | "typescript" => awaited_callee(n).unwrap_or(expr),
        _ => expr,
    };
    let token_expr = if f.language == "rust" && expr.kind() == "generic_function" {
        expr.child_by_field_name("function").unwrap_or(expr)
    } else {
        expr
    };
    let member = match f.language.as_str() {
        "java" => Some(expr),
        "python" if expr.kind() == "attribute" => expr.child_by_field_name("attribute"),
        "javascript" | "typescript" if expr.kind() == "member_expression" => {
            expr.child_by_field_name("property")
        }
        "rust" if token_expr.kind() == "field_expression" => {
            token_expr.child_by_field_name("field")
        }
        "rust" if token_expr.kind() == "scoped_identifier" => {
            token_expr.child_by_field_name("name")
        }
        "rust" => Some(token_expr),
        _ => Some(expr),
    };
    if let Some(member) = member
        && matches!(
            member.kind(),
            "identifier" | "property_identifier" | "field_identifier" | "type_identifier"
        )
        && let Some(token) = f.text.get(member.byte_range())
        && !token.is_empty()
    {
        return (Some(token.into()), Some(Range::node(member)));
    }
    (
        f.text
            .get(expr.byte_range())
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        None,
    )
}
/// `expand_macros: false` keeps every Rust macro opaque, as before Decision 0019; only tests
/// use it, to show that expansion leaves every fact outside the expanded macros unchanged.
pub(super) fn extract_document(
    a: &mut Artifact,
    f: &SourceFile,
    document: &Document,
    ids: &mut IdentityRegistry,
    header_witness: &mut dyn FnMut(&str, String, Range),
    expand_macros: bool,
) -> Result<()> {
    #[cfg(test)]
    {
        *DOCUMENT_EXTRACTIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(a.source_set.root_id.clone())
            .or_default() += 1;
    }
    language_order(&f.language)?;
    safe_path(&f.path)?;
    // Occurrence identity binds this captured document version. Authenticate its content hash
    // and extraction context against the captured revision before measuring, so a failed
    // precondition mints no occ:v2 record at all.
    ensure!(
        f.hash == hash(f.text.as_bytes()),
        "native captured source hash mismatch"
    );
    ensure!(
        document.key.source_set_id == a.source_set.id
            && document.key.language == f.language
            && document.key.path == f.path
            && document.content_hash == f.hash
            && document.byte_length == f.text.len(),
        "native document not in captured revision"
    );
    let extraction_context = native_extraction_context(&a.producer, &f.language, &a.revision)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&match f.language.as_str() {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => crate::indexer_ts::grammar(&f.path),
        _ => anyhow::bail!("unsupported native language"),
    })?;
    let tree = parser
        .parse(&f.text, None)
        .context("native parser failed")?;
    let key = DocumentKey {
        source_set_id: a.source_set.id.clone(),
        language: f.language.clone(),
        path: f.path.clone(),
    };
    let proof_id = format!(
        "native-proof:v1:{}",
        digest(
            b"trellis.native-proof.v1\0",
            &canonical(&json!({"producerId":PRODUCER,"document":key,"revisionId":a.revision.id}))
        )
    );
    a.provenance.push(Provenance {
        id: proof_id.clone(),
        producer_id: PRODUCER.into(),
        document: key.clone(),
        revision_id: a.revision.id.clone(),
        content_hash: f.hash.clone(),
        evidence_kind: "measuredSyntax".into(),
        basis: None,
        freshness: "fresh".into(),
        derived_from: None,
    });
    let module_key = Key {
        kind: "module".into(),
        name: None,
        signature: None,
        ordinal: 0,
    };
    let module_id=ids.stable(&json!({"sourceSet":a.source_set.id,"path":f.path,"language":f.language,"ancestors":[],"declaration":module_key}))?;
    // A file-level inner `#![cfg(test)]` makes the whole document test code.
    let file_test = test_path(&f.path) || local_test_marker(tree.root_node(), f);
    let mut module_range = Range::node(tree.root_node());
    if !f.text.is_empty() && module_range.start == module_range.end {
        module_range = Range {
            start: 0,
            end: f.text.len(),
        };
    }
    a.declarations.push(Declaration {
        syntax_id: module_id.clone(),
        role: declaration_role(file_test),
        document: key.clone(),
        revision_id: a.revision.id.clone(),
        kind: "module".into(),
        name: None,
        lookup_key: None,
        ancestors: vec![],
        key: module_key,
        range: module_range,
        name_range: None,
        header: Header {
            kind: "module".into(),
            name: None,
            modifiers: vec![],
            type_parameters: vec![],
            parameters: vec![],
            result_type: None,
            bases: vec![],
        },
        provenance_id: proof_id.clone(),
    });
    let declaration_start = a.declarations.len() - 1;
    let mut state = ParserState {
        search_nodes: BTreeMap::new(),
        call_start: a.calls.len(),
        control_start: a.control_regions.len(),
        reference_start: a.references.len(),
        artifact: a,
        file: f,
        ids,
        document: key,
        proof_id,
        extraction_context,
        seen: BTreeSet::new(),
        declaration_ordinals: BTreeMap::new(),
        call_ordinals: BTreeMap::new(),
        control_ordinals: BTreeMap::new(),
        saw_definition: false,
        saw_call: false,
        visits: 0,
        opaque_rust: false,
        gaps: Vec::new(),
        bound_gaps: Vec::new(),
        bounded: false,
        module_only: None,
        name_owners: BTreeMap::new(),
        lexical_owners: BTreeMap::from([(canonical(&json!([])), module_id.clone())]),
        header_witness,
        expand_macros,
        expanding: false,
        macro_expansions: Vec::new(),
        measured_macros: BTreeMap::new(),
        expansion_bytes: 0,
        macro_parser: None,
        visit_ceiling: VISIT_LIMIT,
        reserved_until: 0,
        declaration_only: f.language == "typescript"
            && crate::indexer_ts::declaration_only(&f.path),
    };
    let mut macros = Vec::new();
    let skipped = crate::skipped_source::skipped(&f.language, &f.text);
    if skipped.is_none() {
        state.walk(
            tree.root_node(),
            &module_id,
            &[],
            &[],
            0,
            (file_test, false),
            &mut macros,
        )?;
    }
    // Allowlisted expression macros are measured only after every ordinary fact, so their
    // declarations take later sibling ordinals and everything outside them is unchanged.
    state.expanding = true;
    for site in macros {
        let range = Range::node(site.node);
        if state.measure_macro(site)? {
            state.macro_expansions.push(range);
        }
    }
    state.finish_bound_gaps(declaration_start);
    state
        .gaps
        .sort_by_key(|gap| (gap.range.start, gap.range.end));
    state.finish_occurrences()?;
    match skipped {
        Some(skip) => state.keep_file_module(declaration_start, skip.diagnostic()),
        None => state.keep_within_budget(declaration_start),
    }
    state
        .artifact
        .search_cards
        .extend(crate::search_capture::capture(
            tree.root_node(),
            f,
            &state.artifact.declarations[declaration_start..],
            &state.artifact.calls[state.call_start..],
            &state.search_nodes,
            &state.measured_macros,
        )?);
    state.artifact.capability_reports.push(CapabilityReport {
        document: state.document.clone(),
        revision_id: state.artifact.revision.id.clone(),
        families: MeasurementKind::ALL
            .into_iter()
            .map(|kind| MeasurementSupport {
                kind,
                available: true,
                diagnostic: None,
                gaps: state.gaps.clone(),
            })
            .collect(),
        macro_expansions: std::mem::take(&mut state.macro_expansions),
    });
    let observed_roles = [
        (state.saw_definition, "definition"),
        (state.saw_call, "call"),
    ]
    .into_iter()
    .filter_map(|(present, role)| present.then_some(role.into()))
    .collect();
    state.artifact.coverage.push(Coverage {
        producer_id: PRODUCER.into(),
        language: f.language.clone(),
        source_set_id: state.artifact.source_set.id.clone(),
        document_path: f.path.clone(),
        revision_id: state.artifact.revision.id.clone(),
        requested: true,
        selected: true,
        state: if tree.root_node().has_error()
            || state.opaque_rust
            || state.bounded
            || state.module_only.is_some()
        {
            "partial"
        } else {
            "complete"
        }
        .into(),
        supported_roles: vec!["definition".into(), "call".into()],
        observed_roles,
        diagnostic: match (
            tree.root_node().has_error(),
            state.opaque_rust,
            state.bounded,
        ) {
            _ if state.module_only.is_some() => state.module_only.map(str::to_owned),
            (true, true, false) => Some("parser recovered from invalid source; opaque Rust macro/attribute token trees skipped".into()),
            (true, false, false) => Some("parser recovered from invalid source".into()),
            (false, true, false) => Some("opaque Rust macro/attribute token trees skipped; calls and definitions inside were not measured".into()),
            (false, false, false) => None,
            (true, _, true) => Some(format!("parser recovered from invalid source; {BOUND_DIAGNOSTIC}")),
            (false, _, true) => Some(BOUND_DIAGNOSTIC.into()),
        },
    });
    Ok(())
}
struct ParserState<'a> {
    search_nodes: BTreeMap<usize, String>,
    artifact: &'a mut Artifact,
    call_start: usize,
    control_start: usize,
    reference_start: usize,
    file: &'a SourceFile,
    ids: &'a mut IdentityRegistry,
    document: DocumentKey,
    proof_id: String,
    extraction_context: String,
    seen: BTreeSet<(String, String, usize, usize)>,
    declaration_ordinals: BTreeMap<String, usize>,
    call_ordinals: BTreeMap<String, usize>,
    control_ordinals: BTreeMap<String, usize>,
    saw_definition: bool,
    saw_call: bool,
    visits: usize,
    opaque_rust: bool,
    gaps: Vec<MeasurementGap>,
    /// Subtrees skipped at the walk bounds, at most [`BOUND_GAPS`], and whether any was.
    bound_gaps: Vec<Range>,
    bounded: bool,
    /// Why only the document's file module was kept: its facts exceeded the stored budgets
    /// (#564), or its source was not read (#576).
    module_only: Option<&'static str>,
    name_owners: BTreeMap<(usize, usize), String>,
    lexical_owners: BTreeMap<Vec<u8>, String>,
    header_witness: &'a mut dyn FnMut(&str, String, Range),
    expand_macros: bool,
    /// Set once the ordinary walk is done: allowlisted macros then expand where found.
    expanding: bool,
    macro_expansions: Vec<Range>,
    /// Every measured macro's re-parse, nested ones included, by invocation byte range. Kept
    /// for the search capture, which reads the declarations recorded from these trees.
    measured_macros: crate::search_capture::MeasuredMacros,
    expansion_bytes: usize,
    macro_parser: Option<tree_sitter::Parser>,
    /// Visit ceiling for the expansion being measured, and the visit count its reservation
    /// runs to. A nested expansion must leave room for the rest of its enclosing one.
    visit_ceiling: usize,
    reserved_until: usize,
    /// A `.d.ts`-style document: no calls, no control regions, and function bodies are
    /// skipped without a gap (Q2, Q7).
    declaration_only: bool,
}
/// Per-document walk bounds. The walk skips what lies past them and leaves a gap (#554). A
/// macro expansion measures completely or not at all, so it reserves its extent first and
/// stays opaque rather than cross them.
const VISIT_LIMIT: usize = 1_000_000;
const DEPTH_LIMIT: usize = 128;
/// A pathological file can have a skipped subtree per node: past this many it is partial
/// without a further range.
const BOUND_GAPS: usize = 1024;
const BOUND_DIAGNOSTIC: &str =
    "syntax beyond native walk bounds skipped; calls and definitions inside were not measured";
/// An allowlisted Rust expression macro, measured after the ordinary walk in its own context.
struct MacroSite<'t> {
    node: Node<'t>,
    form: crate::native_rust_macros::MacroForm,
    owner: String,
    ancestry: Vec<Key>,
    regions: Vec<String>,
    depth: usize,
    test: bool,
}
impl ParserState<'_> {
    fn finish_occurrences(&mut self) -> Result<()> {
        let mut replacements = BTreeMap::new();
        let mut by_owner: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (offset, region) in self.artifact.control_regions[self.control_start..]
            .iter()
            .enumerate()
        {
            ensure!(
                region.document == self.document,
                "foreign native control in current document"
            );
            by_owner
                .entry(region.owner_syntax_id.clone())
                .or_default()
                .push(self.control_start + offset);
        }
        for indexes in by_owner.values_mut() {
            indexes.sort_by_key(|i| {
                let r = &self.artifact.control_regions[*i].range;
                (
                    in_macro_expansion(&self.macro_expansions, r),
                    r.start,
                    r.end,
                )
            });
            for (ordinal, i) in indexes.iter().enumerate() {
                let input = occurrence_input(
                    &self.file.hash,
                    &self.extraction_context,
                    &self.artifact.producer,
                    &self.artifact.control_regions[*i].owner_syntax_id,
                    "control",
                    ordinal,
                );
                let id = self.ids.occurrence_ref(&input)?;
                let region = &mut self.artifact.control_regions[*i];
                replacements.insert(std::mem::replace(&mut region.id, id.clone()), id);
                region.ordinal = ordinal;
            }
        }
        for region in &mut self.artifact.control_regions[self.control_start..] {
            if let Some(parent) = &mut region.parent_id {
                *parent = replacements
                    .get(parent)
                    .context("native region parent not measured")?
                    .clone();
            }
        }
        let mut by_owner: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (offset, call) in self.artifact.calls[self.call_start..].iter().enumerate() {
            ensure!(
                call.document == self.document,
                "foreign native call in current document"
            );
            by_owner
                .entry(call.owner_syntax_id.clone())
                .or_default()
                .push(self.call_start + offset);
        }
        for indexes in by_owner.values_mut() {
            indexes.sort_by_key(|i| {
                let r = &self.artifact.calls[*i].range;
                (
                    in_macro_expansion(&self.macro_expansions, r),
                    r.start,
                    r.end,
                )
            });
            for (ordinal, i) in indexes.iter().enumerate() {
                let input = occurrence_input(
                    &self.file.hash,
                    &self.extraction_context,
                    &self.artifact.producer,
                    &self.artifact.calls[*i].owner_syntax_id,
                    "call",
                    ordinal,
                );
                let id = self.ids.occurrence_ref(&input)?;
                let call = &mut self.artifact.calls[*i];
                call.ordinal = ordinal;
                call.id = id;
                for region in &mut call.region_ids {
                    *region = replacements
                        .get(region)
                        .context("native call region not measured")?
                        .clone();
                }
            }
        }
        let mut by_owner: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (offset, reference) in self.artifact.references[self.reference_start..]
            .iter()
            .enumerate()
        {
            by_owner
                .entry(reference.owner_syntax_id.clone())
                .or_default()
                .push(self.reference_start + offset);
        }
        for indexes in by_owner.values_mut() {
            indexes.sort_by_key(|i| {
                let r = &self.artifact.references[*i].range;
                (
                    in_macro_expansion(&self.macro_expansions, r),
                    r.start,
                    r.end,
                )
            });
            for (ordinal, i) in indexes.iter().enumerate() {
                let input = occurrence_input(
                    &self.file.hash,
                    &self.extraction_context,
                    &self.artifact.producer,
                    &self.artifact.references[*i].owner_syntax_id,
                    "reference",
                    ordinal,
                );
                let id = self.ids.occurrence_ref(&input)?;
                let reference = &mut self.artifact.references[*i];
                reference.ordinal = ordinal;
                reference.id = id;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // The walk's lexical context plus its macro queue.
    fn walk<'t>(
        &mut self,
        n: Node<'t>,
        owner: &str,
        ancestors: &[Key],
        regions: &[String],
        depth: usize,
        test_flags: (bool, bool),
        macros: &mut Vec<MacroSite<'t>>,
    ) -> Result<()> {
        if depth >= DEPTH_LIMIT || self.visits >= VISIT_LIMIT {
            ensure!(!self.expanding, "native expansion exceeded its reservation");
            self.bounded = true;
            if self.bound_gaps.len() < BOUND_GAPS && n.start_byte() < n.end_byte() {
                self.bound_gaps.push(Range::node(n));
            }
            return Ok(());
        }
        if self.declaration_only && crate::indexer_ts::is_executable_body(n) {
            return Ok(());
        }
        self.visits += 1;
        // Rust token trees and attributes are opaque source syntax: a call-shaped token
        // inside one is not a measured call. Only allowlisted expression macros are
        // measured, after the ordinary walk (Decision 0019). Still count this subtree root
        // against the bounded visit budget.
        if self.file.language == "rust"
            && matches!(
                n.kind(),
                "macro_invocation"
                    | "macro_definition"
                    | "token_tree"
                    | "attribute_item"
                    | "inner_attribute_item"
            )
        {
            if self.expand_macros
                && n.kind() == "macro_invocation"
                && let Some(form) = crate::native_rust_macros::expression_macro(n, &self.file.text)
            {
                let site = MacroSite {
                    node: n,
                    form,
                    owner: owner.to_owned(),
                    ancestry: ancestors.to_vec(),
                    regions: regions.to_vec(),
                    depth,
                    test: test_flags.0 || test_flags.1,
                };
                if self.expanding {
                    self.measure_macro(site)?;
                } else {
                    macros.push(site);
                }
                return Ok(());
            }
            self.opaque_gap(
                n,
                format!(
                    "opaque Rust {} skipped; contents require macro/attribute interpretation",
                    n.kind()
                ),
            );
            return Ok(());
        }
        if self.file.language == "rust" && matches!(n.kind(), "lifetime" | "label" | "metavariable")
        {
            return Ok(());
        }
        let test_context = test_flags.0 || test_flags.1 || local_test_marker(n, self.file);
        let mut owner = owner.to_owned();
        // Most nodes declare nothing, and a key can carry a whole Java signature.
        let mut ancestry = std::borrow::Cow::Borrowed(ancestors);
        let mut regions = regions.to_vec();
        if let Some(k) = kind(&self.file.language, n, &self.file.text)
            && (matches!(k, "implementation" | "anonymousFunction") || name(n, self.file).is_some())
        {
            let mut key = key_of(n, self.file, k);
            // Repeated declaration keys are document-local. A cumulative artifact
            // scan would make a multi-file full capture quadratic in its facts.
            // The synthetic module is not a previous measured declaration.
            if k != "module" {
                let ordinal_key =
                    serde_json::to_string(&(&ancestry, &key.kind, &key.name, &key.signature))?;
                let count = self.declaration_ordinals.entry(ordinal_key).or_default();
                key.ordinal = *count;
                *count += 1;
            }
            let id=self.ids.stable(&json!({"sourceSet":self.artifact.source_set.id,"path":self.file.path,"language":self.file.language,"ancestors":ancestry,"declaration":key}))?;
            let named = name(n, self.file);
            if let Some((_, range)) = &named {
                let parent = self
                    .lexical_owners
                    .get(&canonical(&json!(ancestry)))
                    .context("native reference declaration parent missing")?;
                self.name_owners
                    .insert((range.start, range.end), parent.clone());
            }
            let mut lexical_ancestry = ancestry.to_vec();
            lexical_ancestry.push(key.clone());
            self.lexical_owners
                .insert(canonical(&json!(lexical_ancestry)), id.clone());
            #[cfg(feature = "test-support")]
            if key.signature.is_some() {
                for (i, parameter) in n
                    .child_by_field_name("parameters")
                    .map(child_nodes)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|p| p.kind().contains("parameter"))
                    .filter_map(|p| java_parameter_parts(p).0)
                    .enumerate()
                {
                    (self.header_witness)(
                        &id,
                        format!("signature.parameterTypes[{i}]"),
                        Range::node(parameter),
                    );
                }
            }
            self.saw_definition |= k != "module";
            // Search capture walks the same trees by node ID: measured macros' stay alive.
            self.search_nodes.insert(n.id(), id.clone());
            self.artifact.declarations.push(Declaration {
                syntax_id: id.clone(),
                role: declaration_role(test_context),
                document: self.document.clone(),
                revision_id: self.artifact.revision.id.clone(),
                kind: k.into(),
                name: named.as_ref().map(|v| v.0.clone()),
                lookup_key: named
                    .as_ref()
                    .map(|v| declaration_lookup(&self.file.language, &v.0))
                    .transpose()?,
                ancestors: ancestry.to_vec(),
                key: key.clone(),
                range: Range::node(n),
                name_range: named.map(|v| v.1),
                header: header_with_witnesses(n, self.file, &key, &mut |field, range| {
                    (self.header_witness)(&id, field, range);
                }),
                provenance_id: self.proof_id.clone(),
            });
            // A lexical declaration key may nest syntax, but its initializer runs in
            // the enclosing executable scope. Only executable containers own calls.
            if matches!(
                k,
                "module"
                    | "namespace"
                    | "type"
                    | "implementation"
                    | "function"
                    | "method"
                    | "constructor"
                    | "anonymousFunction"
            ) {
                owner = id;
                regions.clear();
            }
            ancestry.to_mut().push(key);
        }
        if !self.declaration_only && is_region(n) {
            let count = self.control_ordinals.entry(owner.clone()).or_default();
            let ordinal = *count;
            *count += 1;
            let range = Range::node(n);
            ensure!(
                self.seen
                    .insert((owner.clone(), "control".into(), range.start, range.end)),
                "duplicate native control range"
            );
            let id = format!(
                "pending:control:{}:{}:{}:{}",
                self.file.path, owner, range.start, range.end
            );
            self.artifact.control_regions.push(ControlRegion {
                id: id.clone(),
                owner_syntax_id: owner.clone(),
                ordinal,
                document: self.document.clone(),
                revision_id: self.artifact.revision.id.clone(),
                kind: n.kind().into(),
                range,
                parent_id: regions.last().cloned(),
                arm: None,
                provenance_id: self.proof_id.clone(),
            });
            regions.push(id);
        }
        if !self.declaration_only && is_call(&self.file.language, n) {
            let count = self.call_ordinals.entry(owner.clone()).or_default();
            let ordinal = *count;
            *count += 1;
            self.saw_call = true;
            let range = match self.file.language.as_str() {
                "javascript" | "typescript" => js_call_range(n),
                _ => Range::node(n),
            };
            ensure!(
                self.seen
                    .insert((owner.clone(), "call".into(), range.start, range.end)),
                "duplicate native call range"
            );
            let id = String::new();
            let (spelling, callee_range) = callee(n, self.file);
            self.artifact.calls.push(Call {
                id,
                owner_syntax_id: owner.clone(),
                ordinal,
                document: self.document.clone(),
                revision_id: self.artifact.revision.id.clone(),
                range,
                callee_range,
                spelling,
                region_ids: regions.clone(),
                provenance_id: self.proof_id.clone(),
            });
        }
        let reference_kind = match self.file.language.as_str() {
            "java" => matches!(n.kind(), "identifier" | "type_identifier"),
            "rust" => matches!(
                n.kind(),
                "identifier" | "type_identifier" | "field_identifier"
            ),
            "python" => n.kind() == "identifier",
            "javascript" => matches!(
                n.kind(),
                "identifier"
                    | "property_identifier"
                    | "shorthand_property_identifier"
                    | "shorthand_property_identifier_pattern"
            ),
            "typescript" => matches!(
                n.kind(),
                "identifier"
                    | "property_identifier"
                    | "shorthand_property_identifier"
                    | "shorthand_property_identifier_pattern"
                    | "type_identifier"
            ),
            _ => false,
        };
        if reference_kind
            && n.named_child_count() == 0
            && let Some(spelling) = self.file.text.get(n.byte_range())
            && let Ok(lookup_key) = lookup(&self.file.language, spelling)
        {
            let range = Range::node(n);
            let reference_owner = self
                .name_owners
                .get(&(range.start, range.end))
                .unwrap_or(&owner)
                .clone();
            if self.seen.insert((
                reference_owner.clone(),
                "reference".into(),
                range.start,
                range.end,
            )) {
                self.artifact.references.push(Reference {
                    id: String::new(),
                    owner_syntax_id: reference_owner,
                    ordinal: 0,
                    document: self.document.clone(),
                    revision_id: self.artifact.revision.id.clone(),
                    range,
                    spelling: spelling.to_owned(),
                    lookup_key,
                    provenance_id: self.proof_id.clone(),
                });
            }
        }
        let mut marked = false;
        for child in child_nodes(n) {
            if self.file.language == "rust" && child.kind() == "attribute_item" {
                marked |= rust_test_attribute(child, self.file);
                self.walk(
                    child,
                    &owner,
                    &ancestry,
                    &regions,
                    depth + 1,
                    (test_context, false),
                    macros,
                )?;
                continue;
            }
            if self.file.language == "rust"
                && matches!(child.kind(), "line_comment" | "block_comment")
            {
                self.walk(
                    child,
                    &owner,
                    &ancestry,
                    &regions,
                    depth + 1,
                    (test_context, false),
                    macros,
                )?;
                continue;
            }
            self.walk(
                child,
                &owner,
                &ancestry,
                &regions,
                depth + 1,
                (test_context, marked),
                macros,
            )?;
            marked = false;
        }
        Ok(())
    }

    /// Publish the skipped subtrees as gaps. An enclosing node records its name, callee and
    /// invocation before walking its children, so a skipped child can span one of those
    /// measured candidates; a gap must not claim a measured candidate, so that child is not
    /// one. The document stays partial either way.
    fn finish_bound_gaps(&mut self, declaration_start: usize) {
        if self.bound_gaps.is_empty() {
            return;
        }
        let artifact = &*self.artifact;
        let mut candidates: Vec<_> = artifact.declarations[declaration_start..]
            .iter()
            .filter_map(|d| d.name_range.as_ref())
            .chain(
                artifact.calls[self.call_start..]
                    .iter()
                    .flat_map(|c| std::iter::once(&c.range).chain(c.callee_range.as_ref())),
            )
            .chain(
                artifact.references[self.reference_start..]
                    .iter()
                    .map(|r| &r.range),
            )
            .map(|range| (range.start, range.end))
            .collect();
        candidates.sort_unstable();
        for range in std::mem::take(&mut self.bound_gaps) {
            let first = candidates.partition_point(|&(start, _)| start < range.start);
            let claimed = candidates[first..]
                .iter()
                .take_while(|&&(start, _)| start < range.end)
                .any(|&(_, end)| end <= range.end);
            if !claimed {
                self.gaps.push(MeasurementGap {
                    range,
                    diagnostic: format!("{BOUND_DIAGNOSTIC}; nesting exceeds {DEPTH_LIMIT} levels or {VISIT_LIMIT} visits"),
                });
            }
        }
    }

    /// One document past the stored budgets publishes as a coverage gap, whatever its
    /// language or shape of evidence (#564). What reads can't admit must not be published:
    /// nothing is left to fail a read, or to send the index back to reconciliation.
    fn keep_within_budget(&mut self, declaration_start: usize) {
        let artifact = &*self.artifact;
        let facts = crate::evidence_budget::Facts {
            declarations: &artifact.declarations[declaration_start..],
            calls: &artifact.calls[self.call_start..],
            control_regions: &artifact.control_regions[self.control_start..],
            references: &artifact.references[self.reference_start..],
            gaps: &self.gaps,
            macro_expansions: self.macro_expansions.len(),
        };
        if crate::evidence_budget::exceeded(self.file, &artifact.source_set.id, facts) {
            self.keep_file_module(declaration_start, crate::evidence_budget::DIAGNOSTIC);
        }
    }

    /// Keep only the file module, with a whole-file gap that says why.
    fn keep_file_module(&mut self, declaration_start: usize, diagnostic: &'static str) {
        self.module_only = Some(diagnostic);
        self.artifact.declarations.truncate(declaration_start + 1);
        self.artifact.calls.truncate(self.call_start);
        self.artifact.control_regions.truncate(self.control_start);
        self.artifact.references.truncate(self.reference_start);
        self.macro_expansions.clear();
        self.measured_macros.clear();
        self.search_nodes.clear();
        self.saw_definition = false;
        self.saw_call = false;
        self.gaps = vec![MeasurementGap {
            range: Range {
                start: 0,
                end: self.file.text.len(),
            },
            diagnostic: diagnostic.into(),
        }];
    }

    fn opaque_gap(&mut self, n: Node<'_>, diagnostic: String) {
        self.opaque_rust = true;
        self.gaps.push(MeasurementGap {
            range: Range::node(n),
            diagnostic,
        });
    }

    /// Measure one allowlisted expression macro in its recorded context (Decision 0019).
    /// Returns whether it was measured; otherwise the whole invocation is an opaque gap, as
    /// for any other macro. The parse is bounded by a per-document byte budget, and the
    /// measured extent is reserved against the walk's visit and depth bounds before any fact
    /// is recorded, so an expansion either measures completely or not at all.
    fn measure_macro(&mut self, site: MacroSite<'_>) -> Result<bool> {
        use crate::native_rust_macros as macros;
        let file = self.file;
        if self.macro_parser.is_none() {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
            self.macro_parser = Some(parser);
        }
        let Some(tree) = macros::token_tree(site.node).and_then(|tt| {
            let (start, end) = (tt.start_byte(), tt.end_byte());
            if self.expansion_bytes.saturating_add(end - start)
                > macros::byte_budget(file.text.len())
            {
                return None;
            }
            self.expansion_bytes += end - start;
            let parser = self.macro_parser.as_mut()?;
            // Measurement reads byte ranges only, so it needs no line table.
            let position = |byte| tree_sitter::Point::new(0, byte);
            macros::parse(
                parser,
                file.text.as_bytes(),
                site.form,
                start,
                end,
                &position,
                &mut || false,
            )
            .map(|tree| (tree, start, end))
        }) else {
            self.unmeasured_macro(site.node);
            return Ok(false);
        };
        let (tree, start, end) = tree;
        let Some(arguments) = macros::arguments(&tree, site.form, start, end) else {
            self.unmeasured_macro(site.node);
            return Ok(false);
        };
        // Reserve the whole expansion before recording anything. A nested expansion's own
        // parse can hold more nodes than the tokens its enclosing reservation counted, so it
        // must also leave the rest of that reservation available. The path, `!` and wrapper
        // add a few visits beyond the argument subtree.
        let ceiling = self
            .visit_ceiling
            .saturating_sub(self.reserved_until.saturating_sub(self.visits));
        let available = ceiling.saturating_sub(self.visits).saturating_sub(16);
        let Some((nodes, height)) = macros::extent(arguments, available) else {
            self.unmeasured_macro(site.node);
            return Ok(false);
        };
        if site.depth + 2 + height >= DEPTH_LIMIT {
            self.unmeasured_macro(site.node);
            return Ok(false);
        }
        let enclosing = (self.visit_ceiling, self.reserved_until);
        self.visit_ceiling = ceiling;
        self.reserved_until = self.visits + nodes + 16;
        let measured = self.measure_arguments(&site, arguments);
        (self.visit_ceiling, self.reserved_until) = enclosing;
        measured?;
        self.measured_macros.insert(
            (site.node.start_byte(), site.node.end_byte()),
            macros::Expansion::new(tree, site.form, start, end),
        );
        Ok(true)
    }

    fn measure_arguments(&mut self, site: &MacroSite<'_>, arguments: Node<'_>) -> Result<()> {
        use crate::native_rust_macros as macros;
        let file = self.file;
        let flags = (site.test, false);
        let (owner, ancestry, regions) = (&site.owner, &site.ancestry, &site.regions);
        if let Some(path) = site.node.child_by_field_name("macro") {
            self.walk(
                path,
                owner,
                ancestry,
                regions,
                site.depth + 1,
                flags,
                &mut Vec::new(),
            )?;
        }
        let format = match site.form {
            macros::MacroForm::Arguments { format } => format,
            macros::MacroForm::Array => None,
        };
        let mut index = 0;
        let mut cursor = arguments.walk();
        for child in arguments.named_children(&mut cursor) {
            let expression = !matches!(
                child.kind(),
                "line_comment" | "block_comment" | "attribute_item"
            );
            let position = index;
            index += usize::from(expression);
            if expression
                && Some(position) == format
                && matches!(child.kind(), "string_literal" | "raw_string_literal")
            {
                // The format string stays opaque; it is a gap only where it names arguments.
                if file.text.get(child.byte_range()).is_some_and(|literal| {
                    macros::names_arguments(literal, child.kind() == "raw_string_literal")
                }) {
                    self.opaque_gap(
                        child,
                        "opaque Rust format string skipped; implicit captures and named arguments require format interpretation".into(),
                    );
                }
                continue;
            }
            if expression
                && format.is_some_and(|format| position > format)
                && child.kind() == "assignment_expression"
                && let Some(name) = child.child_by_field_name("left")
                && name.kind() == "identifier"
                && let Some(value) = child.child_by_field_name("right")
            {
                // `name = value` names a format argument; only the value is an expression.
                self.opaque_gap(
                    name,
                    "opaque Rust named format argument skipped; argument names require format interpretation".into(),
                );
                self.walk(
                    value,
                    owner,
                    ancestry,
                    regions,
                    site.depth + 3,
                    flags,
                    &mut Vec::new(),
                )?;
                continue;
            }
            self.walk(
                child,
                owner,
                ancestry,
                regions,
                site.depth + 2,
                flags,
                &mut Vec::new(),
            )?;
        }
        Ok(())
    }

    fn unmeasured_macro(&mut self, n: Node<'_>) {
        self.opaque_gap(
            n,
            "opaque Rust macro_invocation skipped; its token tree is not measurable as Rust expressions within native bounds".into(),
        );
    }
}
