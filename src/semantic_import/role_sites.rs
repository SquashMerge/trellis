//! Tree-sitter direct bases and role sites.
use super::staged;
#[cfg(test)]
use super::{
    TargetCallable,
    dispatch::{CallShape, call_dispatch},
    work::complexity_work,
};
use crate::{native_evidence as native, semantic_evidence as semantic};
use anyhow::{Context, Result};
use std::collections::BTreeSet;

/// A clause node contributes only direct named type-use spans, never ancestors inferred
/// from producer flags. The exact raw occurrence at that span supplies endpoint identity.
pub(super) type DirectBase = (native::Range, native::Range, semantic::RelationshipKind);

/// A lazily computed byte estimate handed to a reservation callback. A callback without a
/// live budget (the in-memory oracle) drops it unevaluated.
pub(super) type LazyBytes<'a> = &'a mut dyn FnMut() -> Result<u64>;

// Count and build from the same parsed tree. Only Rust-owned output is admitted here;
// tree-sitter's C tree allocation is not covered by this reservation. The counting walk is
// part of the estimate, so an unmetered caller builds the Vec by ordinary growth.
pub(super) fn collect_direct_bases(
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
    mut emit: impl FnMut(&mut dyn FnMut(DirectBase) -> Result<()>) -> Result<()>,
) -> Result<Vec<DirectBase>> {
    let mut count = 0usize;
    reserve(&mut || {
        emit(&mut |_| {
            count = count.checked_add(1).ok_or_else(|| {
                crate::scip_raw::ScipCaptureError::LimitExceeded("unit expansion".into())
            })?;
            Ok(())
        })?;
        let slots = (count as u64)
            .checked_mul(std::mem::size_of::<DirectBase>() as u64)
            .ok_or_else(|| {
                crate::scip_raw::ScipCaptureError::LimitExceeded("unit expansion".into())
            })?;
        staged::sizes::sum(&[std::mem::size_of::<Vec<DirectBase>>() as u64, slots])
    })?;
    let mut result = Vec::with_capacity(count);
    emit(&mut |row| {
        result.push(row);
        Ok(())
    })?;
    Ok(result)
}

pub(super) fn direct_java_types(
    source: &[u8],
    parsed: Option<tree_sitter::Tree>,
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<Vec<DirectBase>> {
    let tree = match parsed {
        Some(tree) => tree,
        None => {
            #[cfg(test)]
            complexity_work(|work| work.java_type_parses += 1);
            let mut parser = crate::measured_parse::parser();
            parser.set_language(&tree_sitter_java::LANGUAGE.into())?;
            parser.parse(source, None).context("Java parser failed")?
        }
    };
    collect_direct_bases(reserve, |emit| java_base_rows(tree.root_node(), emit))
}

fn java_base_rows(
    root: tree_sitter::Node<'_>,
    emit: &mut dyn FnMut(DirectBase) -> Result<()>,
) -> Result<()> {
    walk_named_reverse(root, &mut |node| {
        let kind = node.kind();
        if matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
        ) && let Some(name) = node.child_by_field_name("name")
        {
            let owner = native::Range {
                start: name.start_byte(),
                end: name.end_byte(),
            };
            for (field, relationship) in [
                ("superclass", semantic::RelationshipKind::Extends),
                ("interfaces", semantic::RelationshipKind::Implements),
            ] {
                if let Some(clause) = node.child_by_field_name(field) {
                    java_clause_rows(clause, &owner, relationship, emit)?;
                }
            }
            if kind == "interface_declaration" {
                let mut cursor = node.walk();
                for child in node
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "extends_interfaces")
                {
                    java_clause_rows(child, &owner, semantic::RelationshipKind::Extends, emit)?;
                }
            }
        }
        Ok(true)
    })
}
fn java_clause_rows(
    clause: tree_sitter::Node<'_>,
    owner: &native::Range,
    relationship: semantic::RelationshipKind,
    emit: &mut dyn FnMut(DirectBase) -> Result<()>,
) -> Result<()> {
    walk_named_reverse(clause, &mut |item| {
        let mut base = None;
        if item.kind() == "type_identifier" {
            base = Some(item);
        } else if matches!(item.kind(), "generic_type" | "scoped_type_identifier") {
            let mut outer = item;
            while matches!(outer.kind(), "generic_type" | "scoped_type_identifier") {
                let index = if outer.kind() == "generic_type" {
                    0
                } else {
                    u32::try_from(outer.named_child_count().saturating_sub(1))?
                };
                let Some(child) = outer.named_child(index) else {
                    break;
                };
                outer = child;
                base = Some(outer);
            }
        } else {
            return Ok(true);
        }
        if let Some(base) = base {
            emit((
                owner.clone(),
                native::Range {
                    start: base.start_byte(),
                    end: base.end_byte(),
                },
                relationship,
            ))?;
        }
        Ok(false)
    })
}

#[derive(Default)]
pub(super) struct RoleSites {
    pub(super) types: BTreeSet<(usize, usize)>,
    pub(super) imports: BTreeSet<(usize, usize)>,
}

/// Visits `root`'s named subtree in reverse pre-order (a node, then its named children last to
/// first), descending only where `visit` returns true: the former push-forward/pop-last
/// traversal. One tree cursor walks it; its stack is bounded by the tree depth, and unlike
/// `Node::parent` and `Node::prev_named_sibling` no step re-descends from the tree root.
fn walk_named_reverse<'t>(
    root: tree_sitter::Node<'t>,
    visit: &mut impl FnMut(tree_sitter::Node<'t>) -> Result<bool>,
) -> Result<()> {
    let mut cursor = root.walk();
    loop {
        if visit(cursor.node())? && goto_last_named_child(&mut cursor) {
            continue;
        }
        loop {
            if cursor.node() == root {
                return Ok(());
            }
            if goto_previous_named_sibling(&mut cursor) {
                break;
            }
            // No earlier named sibling: the cursor rests on a sibling, whose parent is the same.
            anyhow::ensure!(cursor.goto_parent(), "missing AST parent");
        }
    }
}

/// Moves to the last named child, or leaves the cursor where it was.
fn goto_last_named_child(cursor: &mut tree_sitter::TreeCursor<'_>) -> bool {
    if !cursor.goto_last_child() {
        return false;
    }
    if goto_named_or_previous(cursor) {
        return true;
    }
    cursor.goto_parent();
    false
}

/// Moves to the nearest earlier named sibling. On failure the cursor rests on some sibling of
/// the start node (or the start node itself).
fn goto_previous_named_sibling(cursor: &mut tree_sitter::TreeCursor<'_>) -> bool {
    cursor.goto_previous_sibling() && goto_named_or_previous(cursor)
}

/// Stays on a named node, else steps back through siblings to the first named one.
fn goto_named_or_previous(cursor: &mut tree_sitter::TreeCursor<'_>) -> bool {
    loop {
        if cursor.node().is_named() {
            return true;
        }
        if !cursor.goto_previous_sibling() {
            return false;
        }
    }
}

pub(super) fn reserve_role_span(
    spans: &mut BTreeSet<(usize, usize)>,
    span: (usize, usize),
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<()> {
    if !spans.contains(&span) {
        reserve(&mut || staged::sizes::map_entries::<(usize, usize), ()>(1))?;
        spans.insert(span);
    }
    Ok(())
}

pub(super) fn source_role_sites(
    language: &str,
    source: &[u8],
    report: Option<&native::CapabilityReport>,
) -> Result<RoleSites> {
    source_role_sites_reserved(language, source, report, &mut |_| Ok(()))
}

pub(super) fn source_role_sites_reserved(
    language: &str,
    source: &[u8],
    report: Option<&native::CapabilityReport>,
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<RoleSites> {
    // The capability report names the document, so its path picks the TypeScript grammar.
    let path = report.map_or("", |report| report.document.path.as_str());
    Ok(source_role_sites_parsed(language, path, source, report, reserve)?.0)
}

/// The role sites of `source`, and the syntax tree they were read from (`None` for a language
/// without role sites). Each supported language's tree is the one its direct type bases are
/// read from too. `report` is the document's capability report, whose measured macros hold sites
/// of their own. `path` is the document's, which picks the TypeScript or TSX grammar.
pub(super) fn source_role_sites_parsed(
    language: &str,
    path: &str,
    source: &[u8],
    report: Option<&native::CapabilityReport>,
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<(RoleSites, Option<tree_sitter::Tree>)> {
    reserve(&mut || Ok(std::mem::size_of::<RoleSites>() as u64))?;
    let grammar: tree_sitter::Language = match language {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => crate::indexer_ts::grammar(path),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        _ => return Ok((RoleSites::default(), None)),
    };
    let mut parser = crate::measured_parse::parser();
    parser.set_language(&grammar)?;
    let tree = parser
        .parse(source, None)
        .context("semantic role source parser failed")?;
    let mut sites = RoleSites::default();
    let mut visit = |node: tree_sitter::Node<'_>| {
        let kind = node.kind();
        let type_field = match language {
            "java"
                if matches!(
                    kind,
                    "field_declaration"
                        | "local_variable_declaration"
                        | "formal_parameter"
                        | "spread_parameter"
                        | "catch_formal_parameter"
                        | "cast_expression"
                        | "instanceof_expression"
                        | "method_declaration"
                        | "constructor_declaration"
                        | "object_creation_expression"
                        | "array_creation_expression"
                        | "superclass"
                        | "super_interfaces"
                        | "extends_interfaces"
                        | "type_arguments"
                        | "type_parameters"
                        | "array_type"
                ) =>
            {
                Some("type")
            }
            "python"
                if matches!(
                    kind,
                    "typed_parameter"
                        | "typed_default_parameter"
                        | "assignment"
                        | "function_definition"
                ) =>
            {
                Some("type")
            }
            "rust"
                if matches!(
                    kind,
                    "trait_item" | "impl_item" | "struct_item" | "enum_item"
                ) =>
            {
                Some("bounds")
            }
            "rust"
                if matches!(
                    kind,
                    "field_declaration"
                        | "let_declaration"
                        | "parameter"
                        | "function_item"
                        | "type_item"
                        | "const_item"
                        | "static_item"
                ) =>
            {
                Some("type")
            }
            _ => None,
        };
        if let Some(field) = type_field.and_then(|name| node.child_by_field_name(name)) {
            collect_role_leaf_spans_reserved(field, language, &mut sites.types, reserve)?;
        }
        if language == "rust" && kind == "impl_item" {
            for field in ["trait", "type"] {
                if let Some(child) = node.child_by_field_name(field) {
                    collect_role_leaf_spans_reserved(child, language, &mut sites.types, reserve)?;
                }
            }
        }
        if language == "java"
            && matches!(
                kind,
                "superclass" | "super_interfaces" | "extends_interfaces"
            )
        {
            collect_role_leaf_spans_reserved(node, language, &mut sites.types, reserve)?;
        }
        if language == "javascript" && kind == "class_heritage" {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                js_direct_class_span(child, &mut sites.types, reserve)?;
            }
        }
        if language == "typescript" {
            match kind {
                "type_annotation"
                | "type_predicate_annotation"
                | "asserts_annotation"
                | "type_arguments"
                | "type_parameters" => {
                    collect_role_leaf_spans_reserved(node, language, &mut sites.types, reserve)?;
                }
                "type_alias_declaration" => {
                    if let Some(value) = node.child_by_field_name("value") {
                        collect_role_leaf_spans_reserved(
                            value,
                            language,
                            &mut sites.types,
                            reserve,
                        )?;
                    }
                }
                "as_expression" | "satisfies_expression" => {
                    let count = node.named_child_count();
                    if let Some(target) = count
                        .checked_sub(1)
                        .and_then(|last| node.named_child(u32::try_from(last).ok()?))
                    {
                        collect_role_leaf_spans_reserved(
                            target,
                            language,
                            &mut sites.types,
                            reserve,
                        )?;
                    }
                }
                "extends_clause" => {
                    if let Some(value) = node.child_by_field_name("value") {
                        js_direct_class_span(value, &mut sites.types, reserve)?;
                    }
                }
                "implements_clause" | "extends_type_clause" => {
                    let mut cursor = node.walk();
                    for child in node.named_children(&mut cursor) {
                        if let Some(name) = ts_base_name(child) {
                            reserve_role_span(
                                &mut sites.types,
                                (name.start_byte(), name.end_byte()),
                                reserve,
                            )?;
                        }
                    }
                }
                _ => {}
            }
        }
        if language == "python" && kind == "class_definition" {
            let mut cursor = node.walk();
            for child in node
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "argument_list")
            {
                collect_role_leaf_spans_reserved(child, language, &mut sites.types, reserve)?;
            }
        }
        if matches!(language, "javascript" | "typescript")
            && kind == "binary_expression"
            && let (Some(operator), Some(right)) = (
                node.child_by_field_name("operator"),
                node.child_by_field_name("right"),
            )
            && source.get(operator.byte_range()) == Some(b"instanceof".as_slice())
        {
            js_direct_class_span(right, &mut sites.types, reserve)?;
        }
        let import_root = match language {
            "java" => kind == "import_declaration",
            "javascript" | "typescript" => kind == "import_statement",
            "python" => matches!(kind, "import_statement" | "import_from_statement"),
            "rust" => kind == "use_declaration",
            _ => false,
        };
        if import_root {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if language == "python"
                    && kind == "import_from_statement"
                    && node.child_by_field_name("module_name") == Some(child)
                {
                    continue;
                }
                collect_role_leaf_spans_reserved(child, language, &mut sites.imports, reserve)?;
            }
        }
        Ok(true)
    };
    walk_named_reverse(tree.root_node(), &mut visit)?;
    if let Some(report) = report.filter(|report| !report.macro_expansions.is_empty())
        && let Ok(text) = std::str::from_utf8(source)
    {
        // Measured macro contents have syntax only in their expansions (#551). A site missing
        // here is not a conservative omission: its occurrence would join as a plain read.
        use crate::native_rust_macros::{Expansions, MeasuredMacros};
        let mut expansions = Expansions::new(text);
        expansions.expand_measured(tree.root_node(), &MeasuredMacros::of(report));
        for arguments in expansions.arguments() {
            walk_named_reverse(arguments, &mut visit)?;
        }
    }
    Ok((sites, Some(tree)))
}

fn js_direct_class_span(
    node: tree_sitter::Node<'_>,
    spans: &mut BTreeSet<(usize, usize)>,
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<()> {
    let span = match node.kind() {
        "identifier" => Some((node.start_byte(), node.end_byte())),
        "member_expression" => node
            .child_by_field_name("property")
            .map(|property| (property.start_byte(), property.end_byte())),
        _ => None,
    };
    if let Some(span) = span {
        reserve_role_span(spans, span, reserve)?;
    }
    Ok(())
}
fn collect_role_leaf_spans_reserved(
    node: tree_sitter::Node<'_>,
    language: &str,
    spans: &mut BTreeSet<(usize, usize)>,
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<()> {
    walk_named_reverse(node, &mut |node| {
        let leaf = match language {
            "java" | "rust" => matches!(node.kind(), "identifier" | "type_identifier"),
            "javascript" => matches!(
                node.kind(),
                "identifier" | "property_identifier" | "shorthand_property_identifier"
            ),
            "typescript" => matches!(
                node.kind(),
                "identifier"
                    | "property_identifier"
                    | "shorthand_property_identifier"
                    | "type_identifier"
            ),
            "python" => node.kind() == "identifier",
            _ => false,
        };
        if leaf {
            reserve_role_span(spans, (node.start_byte(), node.end_byte()), reserve)?;
        }
        Ok(!leaf)
    })
}

/// The name token of a TypeScript base type or class value: an identifier, the last segment of
/// a `nested_type_identifier` or `member_expression`, or the name of a `generic_type`.
fn ts_base_name(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    match node.kind() {
        "identifier" | "type_identifier" => Some(node),
        "nested_type_identifier" => node.child_by_field_name("name"),
        "member_expression" => node.child_by_field_name("property"),
        "generic_type" => node.child_by_field_name("name").and_then(ts_base_name),
        _ => None,
    }
}

/// The direct bases of TypeScript classes and interfaces: a class's `extends` value is an
/// `Extends` base and each `implements` type an `Implements` base, and an interface's
/// `extends` types are `Extends` bases.
pub(super) fn direct_ts_types(
    source: &[u8],
    language: tree_sitter::Language,
    parsed: Option<tree_sitter::Tree>,
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<Vec<DirectBase>> {
    let tree = match parsed {
        Some(tree) => tree,
        None => {
            let mut parser = crate::measured_parse::parser();
            parser.set_language(&language)?;
            parser
                .parse(source, None)
                .context("semantic source parser failed")?
        }
    };
    collect_direct_bases(reserve, |emit| {
        walk_named_reverse(tree.root_node(), &mut |node| {
            let owner_kind = node.kind();
            if matches!(
                owner_kind,
                "class_declaration" | "abstract_class_declaration" | "interface_declaration"
            ) && let Some(name) = node.child_by_field_name("name")
            {
                let owner = native::Range {
                    start: name.start_byte(),
                    end: name.end_byte(),
                };
                let mut bases: Vec<(tree_sitter::Node<'_>, semantic::RelationshipKind)> = vec![];
                let mut cursor = node.walk();
                for clause in node.named_children(&mut cursor) {
                    match clause.kind() {
                        "extends_type_clause" => {
                            let mut inner = clause.walk();
                            bases.extend(clause.named_children(&mut inner).filter_map(|base| {
                                ts_base_name(base)
                                    .map(|name| (name, semantic::RelationshipKind::Extends))
                            }));
                        }
                        "class_heritage" => {
                            let mut inner = clause.walk();
                            for heritage in clause.named_children(&mut inner) {
                                match heritage.kind() {
                                    "extends_clause" => {
                                        bases.extend(
                                            heritage
                                                .child_by_field_name("value")
                                                .and_then(ts_base_name)
                                                .map(|name| {
                                                    (name, semantic::RelationshipKind::Extends)
                                                }),
                                        );
                                    }
                                    "implements_clause" => {
                                        let mut types = heritage.walk();
                                        bases.extend(
                                            heritage.named_children(&mut types).filter_map(
                                                |base| {
                                                    ts_base_name(base).map(|name| {
                                                        (
                                                            name,
                                                            semantic::RelationshipKind::Implements,
                                                        )
                                                    })
                                                },
                                            ),
                                        );
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                for (name, kind) in bases {
                    emit((
                        owner.clone(),
                        native::Range {
                            start: name.start_byte(),
                            end: name.end_byte(),
                        },
                        kind,
                    ))?;
                }
            }
            Ok(true)
        })
    })
}

pub(super) fn direct_script_types(
    source: &[u8],
    language: tree_sitter::Language,
    owner_kind: &str,
    clause_kind: &str,
    parsed: Option<tree_sitter::Tree>,
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<Vec<DirectBase>> {
    let tree = match parsed {
        Some(tree) => tree,
        None => {
            let mut parser = crate::measured_parse::parser();
            parser.set_language(&language)?;
            parser
                .parse(source, None)
                .context("semantic source parser failed")?
        }
    };
    collect_direct_bases(reserve, |emit| {
        walk_named_reverse(tree.root_node(), &mut |node| {
            if node.kind() == owner_kind
                && let Some(name) = node.child_by_field_name("name")
            {
                let owner = native::Range {
                    start: name.start_byte(),
                    end: name.end_byte(),
                };
                let mut cursor = node.walk();
                for clause in node
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == clause_kind)
                {
                    let mut child_cursor = clause.walk();
                    for base in clause.named_children(&mut child_cursor) {
                        let name = match base.kind() {
                            "identifier" => Some(base),
                            "attribute" => base.named_child(u32::try_from(
                                base.named_child_count().saturating_sub(1),
                            )?),
                            _ => None,
                        };
                        if let Some(name) = name {
                            emit((
                                owner.clone(),
                                native::Range {
                                    start: name.start_byte(),
                                    end: name.end_byte(),
                                },
                                semantic::RelationshipKind::Extends,
                            ))?;
                        }
                    }
                }
            }
            Ok(true)
        })
    })
}

#[cfg(test)]
mod walk_tests {
    use super::*;

    /// The former traversal, over `Node::parent` and `Node::prev_named_sibling`.
    fn reference_walk<'t>(
        root: tree_sitter::Node<'t>,
        visit: &mut impl FnMut(tree_sitter::Node<'t>) -> bool,
    ) {
        let mut node = root;
        loop {
            if visit(node)
                && let Some(child) = node
                    .named_child(u32::try_from(node.named_child_count().saturating_sub(1)).unwrap())
            {
                node = child;
                continue;
            }
            loop {
                if node == root {
                    return;
                }
                if let Some(sibling) = node.prev_named_sibling() {
                    node = sibling;
                    break;
                }
                node = node.parent().unwrap();
            }
        }
    }

    fn sources() -> Vec<(tree_sitter::Language, String)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = vec![];
        let mut pending = vec![root.join("tests/fixtures")];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        files.extend(
            ["src/semantic_import.rs", "src/scip_raw.rs"]
                .iter()
                .map(|path| root.join(path)),
        );
        files.sort();
        let mut sources = vec![];
        for path in files {
            let language: tree_sitter::Language = match path.extension().and_then(|e| e.to_str()) {
                Some("java") => tree_sitter_java::LANGUAGE.into(),
                Some("js") => tree_sitter_javascript::LANGUAGE.into(),
                Some("ts") => crate::indexer_ts::grammar("a.ts"),
                Some("tsx") => crate::indexer_ts::grammar("a.tsx"),
                Some("py") => tree_sitter_python::LANGUAGE.into(),
                Some("rs") => tree_sitter_rust::LANGUAGE.into(),
                _ => continue,
            };
            if let Ok(text) = std::fs::read_to_string(&path) {
                sources.push((language, text));
            }
        }
        // Malformed input parses with error and missing nodes.
        for (language, text) in [
            (
                tree_sitter_java::LANGUAGE.into(),
                "class A extends { void f( }",
            ),
            (tree_sitter_python::LANGUAGE.into(), "def f(:\n  return (\n"),
            (
                tree_sitter_javascript::LANGUAGE.into(),
                "class A extends B { f( } /*",
            ),
            (tree_sitter_rust::LANGUAGE.into(), "impl<T> X for { fn f( }"),
        ] {
            sources.push((language, text.to_owned()));
        }
        sources
    }

    #[test]
    fn cursor_walk_visits_exactly_the_former_traversal() {
        let sources = sources();
        assert!(sources.len() > 20, "{} sources", sources.len());
        for (language, source) in &sources {
            let mut parser = crate::measured_parse::parser();
            parser.set_language(language).unwrap();
            let tree = parser.parse(source, None).unwrap();
            // Descend everywhere, and also prune by kind, as the role-site visitors do.
            for prune in [None, Some(0usize), Some(1)] {
                let descend = |node: tree_sitter::Node<'_>| match prune {
                    None => true,
                    Some(parity) => node.kind().len() % 2 != parity,
                };
                // The whole tree and a nested subtree, whose siblings must stay unvisited.
                let mut roots = vec![tree.root_node()];
                let mut cursor = tree.root_node().walk();
                if let Some(child) = tree.root_node().named_children(&mut cursor).nth(1) {
                    roots.push(child);
                }
                for root in roots {
                    let mut expected = vec![];
                    reference_walk(root, &mut |node| {
                        expected.push(node.id());
                        descend(node)
                    });
                    let mut actual = vec![];
                    walk_named_reverse(root, &mut |node| {
                        actual.push(node.id());
                        Ok(descend(node))
                    })
                    .unwrap();
                    assert_eq!(actual, expected);
                }
            }
        }
    }
}

#[cfg(test)]
mod typescript_tests {
    use super::*;

    fn spelled(source: &str, spans: &BTreeSet<(usize, usize)>) -> Vec<String> {
        spans
            .iter()
            .map(|&(start, end)| source[start..end].to_owned())
            .collect()
    }

    fn type_sites(path: &str, source: &str) -> Vec<String> {
        let sites =
            source_role_sites_parsed("typescript", path, source.as_bytes(), None, &mut |_| Ok(()))
                .unwrap()
                .0;
        spelled(source, &sites.types)
    }

    fn direct_bases(source: &str) -> Vec<(String, String, semantic::RelationshipKind)> {
        direct_ts_types(
            source.as_bytes(),
            crate::indexer_ts::grammar("a.ts"),
            None,
            &mut |_| Ok(()),
        )
        .unwrap()
        .into_iter()
        .map(|(owner, base, kind)| {
            (
                source[owner.start..owner.end].to_owned(),
                source[base.start..base.end].to_owned(),
                kind,
            )
        })
        .collect()
    }

    #[test]
    fn annotation_and_cast_types_are_type_sites_and_the_cast_operand_is_not() {
        let source = "let x: Foo = y as Bar;";
        let sites = type_sites("a.ts", source);
        assert_eq!(sites, ["Foo", "Bar"]);
        assert!(!sites.contains(&"y".to_owned()));
    }

    #[test]
    fn type_roles_cover_alias_arguments_parameters_and_satisfies() {
        let source =
            "type T = Alias; function f<P>(a: Arg<Inner>): Ret { return a satisfies Sat; }";
        let sites = type_sites("a.ts", source);
        for expected in ["Alias", "P", "Arg", "Inner", "Ret", "Sat"] {
            assert!(
                sites.contains(&expected.to_owned()),
                "{expected}: {sites:?}"
            );
        }
        assert!(!sites.contains(&"a".to_owned()), "{sites:?}");
    }

    #[test]
    fn tsx_documents_parse_with_the_tsx_grammar() {
        let source = "const v = <Box/>; let x: Foo = y;";
        assert_eq!(type_sites("a.tsx", source), ["Foo"]);
    }

    #[test]
    fn heritage_names_are_type_sites() {
        let source = "class A extends ns.B implements C, D<E> {} interface I extends J, a.K {}";
        let sites = type_sites("a.ts", source);
        for expected in ["B", "C", "D", "E", "J", "K"] {
            assert!(
                sites.contains(&expected.to_owned()),
                "{expected}: {sites:?}"
            );
        }
        assert!(!sites.contains(&"ns".to_owned()), "{sites:?}");
    }

    #[test]
    fn direct_bases_name_extends_implements_and_interface_extends() {
        use semantic::RelationshipKind::{Extends, Implements};
        let source = "class A extends ns.B implements C, D<E> {} abstract class F extends G<H> {} interface I extends J, a.K {}";
        let mut got = direct_bases(source);
        got.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        let expected = [
            ("A", "B", Extends),
            ("A", "C", Implements),
            ("A", "D", Implements),
            ("F", "G", Extends),
            ("I", "J", Extends),
            ("I", "K", Extends),
        ];
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(o, b, k)| (o.to_owned(), b.to_owned(), k))
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn calls_classify_like_javascript() {
        use CallShape::{Member, New, Plain};
        use TargetCallable::{Constructor, DynamicMethod, FreeFunction};
        assert_eq!(
            call_dispatch("typescript", Plain, FreeFunction),
            semantic::Dispatch::Direct
        );
        assert_eq!(
            call_dispatch("typescript", New, Constructor),
            semantic::Dispatch::Constructor
        );
        assert_eq!(
            call_dispatch("typescript", Member, DynamicMethod),
            semantic::Dispatch::Dynamic
        );
        assert_eq!(
            call_dispatch("typescript", Plain, Constructor),
            semantic::Dispatch::Unknown
        );
    }
}
