//! View-time, source-witnessed module references. Structural mappings are not bindings.
use crate::model::{IndexPin, SourceFile};
use crate::native_evidence::{Call, Declaration, DeclarationRole, DocumentKey};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::{Node, Parser, Tree};

const SOURCE_BYTES: usize = 2 * 1024 * 1024;
const TOTAL_BYTES: usize = 256 * 1024 * 1024;
const VISITS: usize = 1_000_000;
const DEPTH: usize = 128;
const SITES: usize = 250_000;
const OWNER_WORK: usize = 5_000_000;

pub struct DocumentInput {
    pub key: DocumentKey,
    pub file: SourceFile,
    pub declarations: Vec<Declaration>,
    pub calls: Vec<Call>,
}
pub struct Snapshot {
    pub pin: IndexPin,
    pub documents: Vec<DocumentInput>,
}
#[derive(Debug)]
pub struct Projection {
    pub data: Value,
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
struct Identity {
    source_set_id: String,
    language: String,
    path: String,
    sid: String,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Site {
    owner_syntax_id: Option<String>,
    role: Option<&'static str>,
    range: crate::native_evidence::Range,
    lines: Option<Lines>,
    relation_kind: &'static str,
    spelling: String,
    call_id: Option<String>,
}
#[derive(Clone, Serialize)]
struct Lines {
    start: usize,
    end: usize,
}
struct RawSite {
    start: usize,
    end: usize,
    kind: &'static str,
    spelling: String,
    destination: std::result::Result<Vec<String>, &'static str>,
}
#[derive(Default)]
struct RustLayout {
    custom: bool,
    wildcard: bool,
    blocked: BTreeSet<String>,
}

/// The closed path-glob grammar shared by architecture request surfaces.
pub fn valid_path_glob(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['\\', '\0', ':', '[', ']', '{', '}', '!'])
        && !path
            .split('/')
            .any(|part| matches!(part, "" | "." | "..") || (part.contains("**") && part != "**"))
        && globset::GlobBuilder::new(path)
            .literal_separator(true)
            .backslash_escape(false)
            .build()
            .is_ok()
}

pub fn project(
    snapshot: &Snapshot,
    paths: &[String],
    sample_limit: usize,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Projection> {
    ensure!(
        (1..=50).contains(&sample_limit),
        "invalid_request: reference sample limit"
    );
    let mut globs = globset::GlobSetBuilder::new();
    for path in paths {
        check()?;
        ensure!(
            valid_path_glob(path),
            "invalid_request: reference path glob"
        );
        globs.add(
            globset::GlobBuilder::new(path)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .context("invalid_request: reference path glob")?,
        );
    }
    let globs = globs.build()?;
    let mut docs: Vec<_> = snapshot.documents.iter().collect();
    docs.sort_by(|a, b| a.key.cmp(&b.key));
    let mut identities = Vec::new();
    let mut modules = Vec::new();
    let mut trees = Vec::new();
    let mut layouts: BTreeMap<String, RustLayout> = BTreeMap::new();
    let mut total_bytes = 0usize;
    let mut document_map = BTreeMap::new();
    for (i, doc) in docs.iter().enumerate() {
        check()?;
        ensure!(
            doc.key.path == doc.file.path && doc.key.language == doc.file.language,
            "store_unavailable: reference document identity"
        );
        ensure!(
            document_map.insert(doc.key.clone(), i).is_none(),
            "store_unavailable: duplicate reference document"
        );
        let roots: Vec<_> = doc
            .declarations
            .iter()
            .filter(|d| d.kind == "module" && d.ancestors.is_empty())
            .collect();
        ensure!(roots.len() == 1, "store_unavailable: reference file module");
        let root = roots[0];
        let identity = Identity {
            source_set_id: doc.key.source_set_id.clone(),
            language: doc.key.language.clone(),
            path: doc.key.path.clone(),
            sid: root.syntax_id.clone(),
        };
        modules.push(json!({"identity":identity,"revisionId":root.revision_id,
            "contentHash":doc.file.hash,"provenanceId":root.provenance_id}));
        identities.push(identity);
        total_bytes = total_bytes
            .checked_add(doc.file.text.len())
            .context("bounded_work: reference bytes")?;
        ensure!(
            doc.file.text.len() <= SOURCE_BYTES && total_bytes <= TOTAL_BYTES,
            "bounded_work: reference source bytes"
        );
        let tree = parse(doc, check)?;
        if doc.key.language == "rust" {
            inspect_layout(
                doc,
                &tree,
                layouts.entry(doc.key.source_set_id.clone()).or_default(),
                check,
            )?;
        }
        trees.push(tree);
    }
    let mut edges: BTreeMap<(usize, usize, bool), (usize, Vec<Site>)> = BTreeMap::new();
    let mut unresolved = Vec::new();
    let mut site_count = 0usize;
    let mut scanned = 0usize;
    let mut mapped = 0usize;
    let mut owner_work = 0usize;
    for (source, (doc, tree)) in docs.iter().zip(&trees).enumerate() {
        if !paths.is_empty() && !globs.is_match(&doc.key.path) {
            continue;
        }
        check()?;
        scanned += 1;
        let raw = extract(doc, tree, check)?;
        let newlines: Vec<_> = doc
            .file
            .text
            .bytes()
            .enumerate()
            .filter_map(|(i, b)| (b == b'\n').then_some(i))
            .collect();
        let mut seen = BTreeSet::new();
        for item in raw {
            check()?;
            if !seen.insert((item.start, item.end, item.kind, item.spelling.clone())) {
                continue;
            }
            site_count += 1;
            ensure!(site_count <= SITES, "bounded_work: reference site count");
            owner_work = owner_work
                .saturating_add(doc.declarations.len())
                .saturating_add(doc.calls.len());
            ensure!(
                owner_work <= OWNER_WORK,
                "bounded_work: reference owner lookup"
            );
            let owner = owner(doc, item.start, item.end);
            let role = owner.map(|d| match d.role {
                DeclarationRole::Test => "test",
                DeclarationRole::Production => "production",
            });
            let site = Site {
                owner_syntax_id: owner.map(|d| d.syntax_id.clone()),
                role,
                range: crate::native_evidence::Range {
                    start: item.start,
                    end: item.end,
                },
                lines: (!doc.file.text.is_empty()).then(|| Lines {
                    start: newlines.partition_point(|&p| p < item.start) + 1,
                    end: newlines.partition_point(|&p| p < item.end.saturating_sub(1)) + 1,
                }),
                relation_kind: item.kind,
                spelling: item.spelling.clone(),
                call_id: doc
                    .calls
                    .iter()
                    .find(|c| c.range.start <= item.start && c.range.end >= item.end)
                    .map(|c| c.id.clone()),
            };
            let target = if owner.is_none() {
                Err("reference_owner_ambiguous")
            } else {
                map_target(
                    doc,
                    &item,
                    &document_map,
                    layouts.get(&doc.key.source_set_id),
                )
            };
            match target {
                Ok(target) => {
                    mapped += 1;
                    let edge = edges
                        .entry((source, target, role == Some("test")))
                        .or_default();
                    edge.0 += 1;
                    if edge.1.len() < sample_limit {
                        edge.1.push(site);
                    }
                }
                Err(reason) => unresolved
                    .push(json!({"source":identities[source],"site":site,"reason":reason})),
            }
        }
    }
    let mut production = Vec::new();
    let mut tests = Vec::new();
    for ((source, target, test), (count, sites)) in edges {
        check()?;
        let omitted = count - sites.len();
        let edge = json!({"source":identities[source],"target":identities[target],"count":count,
            "sites":sites,"omittedSites":omitted,"mapping":"structural"});
        if test {
            tests.push(edge);
        } else {
            production.push(edge);
        }
    }
    check()?;
    Ok(Projection {
        data: json!({
            "referencesVersion":"references:v1","indexGeneration":snapshot.pin.index_generation.to_string(),
            "indexRevision":snapshot.pin.index_revision,"evidenceTier":"syntax","semanticComplete":false,
            "modules":modules,"production":production,"tests":tests,"unresolved":unresolved,
            "counts":{"scannedDocuments":scanned,"mappedSites":mapped,"unresolvedSites":site_count-mapped},
            "limitations":[
                "Structural file conventions are not compiler, package, type or call resolution; missing sites do not prove absence.",
                "Rust maps explicit crate paths only; aliases, reexports, wildcards, custom path attributes and inline ambiguity remain unresolved; macros are opaque.",
                "JavaScript maps static relative imports/reexports using explicit files or .js/.mjs/.cjs/index.js; dynamic and bare imports remain unresolved.",
                "Python maps explicit relative module imports using .py/__init__.py; absolute, alias, wildcard and module-versus-attribute forms remain unresolved.",
                "Java import and qualified type sites remain unresolved; package grouping is not produced."
            ]
        }),
    })
}

fn grammar(language: &str) -> Result<tree_sitter::Language> {
    Ok(match language {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        _ => anyhow::bail!("store_unavailable: reference language"),
    })
}
fn parse(doc: &DocumentInput, check: &mut impl FnMut() -> Result<()>) -> Result<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&grammar(&doc.key.language)?)?;
    let mut aborted = None;
    let tree = parser.parse_with_options(
        &mut |offset, _| &doc.file.text.as_bytes()[offset..],
        None,
        Some(
            tree_sitter::ParseOptions::new().progress_callback(&mut |_| {
                if let Err(error) = check() {
                    aborted = Some(error);
                    true
                } else {
                    false
                }
            }),
        ),
    );
    if let Some(error) = aborted {
        return Err(error);
    }
    let tree = tree.context("bounded_work: reference parser stopped")?;
    ensure!(!tree.root_node().has_error(), "invalid_source_syntax");
    walk(tree.root_node(), check, &mut |node, _check| {
        ensure!(
            !node.is_error() && !node.is_missing(),
            "invalid_source_syntax"
        );
        Ok(false)
    })?;
    Ok(tree)
}
fn walk<'a, C: FnMut() -> Result<()>>(
    root: Node<'a>,
    check: &mut C,
    visit: &mut impl FnMut(Node<'a>, &mut C) -> Result<bool>,
) -> Result<()> {
    let mut stack = vec![(root, 0)];
    let mut visits = 0;
    while let Some((node, depth)) = stack.pop() {
        check()?;
        visits += 1;
        ensure!(
            visits <= VISITS && depth <= DEPTH,
            "bounded_work: reference AST"
        );
        if !visit(node, check)? {
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                stack.push((child, depth + 1));
            }
        }
    }
    Ok(())
}
fn text<'a>(doc: &'a DocumentInput, node: Node<'_>) -> &'a str {
    &doc.file.text[node.byte_range()]
}
fn owner(doc: &DocumentInput, start: usize, end: usize) -> Option<&Declaration> {
    let mut owners: Vec<_> = doc
        .declarations
        .iter()
        .filter(|d| d.range.start <= start && d.range.end >= end)
        .collect();
    owners.sort_by_key(|d| {
        (
            d.range.end - d.range.start,
            d.kind == "module",
            std::cmp::Reverse(d.ancestors.len()),
        )
    });
    let first = *owners.first()?;
    if owners.get(1).is_some_and(|d| {
        d.range == first.range && d.kind == first.kind && d.ancestors.len() == first.ancestors.len()
    }) {
        None
    } else {
        Some(first)
    }
}
fn opaque(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "macro_invocation"
            | "macro_definition"
            | "token_tree"
            | "attribute_item"
            | "inner_attribute_item"
    )
}
fn inspect_layout(
    doc: &DocumentInput,
    tree: &Tree,
    layout: &mut RustLayout,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    walk(tree.root_node(), check, &mut |node, check| {
        if matches!(node.kind(), "attribute_item" | "inner_attribute_item") {
            let value: String = text(doc, node)
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            let mut cursor = node.walk();
            let path_attribute = node.named_children(&mut cursor).any(|attribute| {
                if attribute.kind() != "attribute" {
                    return false;
                }
                let mut cursor = attribute.walk();
                attribute
                    .named_children(&mut cursor)
                    .any(|name| name.kind() == "identifier" && text(doc, name) == "path")
            });
            if path_attribute || value.contains("path=") {
                layout.custom = true;
            }
        }
        if node.kind() == "mod_item"
            && node.child_by_field_name("body").is_some()
            && let Some(name) = node.child_by_field_name("name")
        {
            layout.blocked.insert(text(doc, name).to_owned());
        }
        if node.kind() == "use_as_clause"
            && let Some(alias) = node.child_by_field_name("alias")
        {
            layout.blocked.insert(text(doc, alias).to_owned());
        }
        if node.kind() == "use_declaration" {
            let mut leaves = vec![node];
            let mut visits = 0;
            while let Some(leaf) = leaves.pop() {
                check()?;
                visits += 1;
                ensure!(visits <= VISITS, "bounded_work: reference layout");
                if leaf.kind() == "use_wildcard" {
                    layout.wildcard = true;
                }
                if matches!(leaf.kind(), "scoped_identifier" | "scoped_type_identifier") {
                    if let Some(name) = leaf.child_by_field_name("name") {
                        layout.blocked.insert(text(doc, name).to_owned());
                    }
                } else if matches!(leaf.kind(), "identifier" | "type_identifier") {
                    layout.blocked.insert(text(doc, leaf).to_owned());
                } else if leaf.kind() == "self" {
                    let mut ancestor = leaf.parent();
                    while let Some(parent) = ancestor {
                        check()?;
                        if parent.kind() == "scoped_use_list" {
                            if let Some(path) = parent.child_by_field_name("path") {
                                let name = path.child_by_field_name("name").unwrap_or(path);
                                layout.blocked.insert(text(doc, name).to_owned());
                            }
                            break;
                        }
                        if parent.kind() == "use_declaration" {
                            break;
                        }
                        ancestor = parent.parent();
                    }
                } else {
                    let mut cursor = leaf.walk();
                    leaves.extend(leaf.named_children(&mut cursor).filter(|c| {
                        leaf.kind() != "scoped_use_list"
                            || leaf.child_by_field_name("list") == Some(*c)
                    }));
                }
            }
        }
        Ok(opaque(node) && !matches!(node.kind(), "attribute_item" | "inner_attribute_item"))
    })
}
fn raw(
    doc: &DocumentInput,
    node: Node<'_>,
    kind: &'static str,
    destination: std::result::Result<Vec<String>, &'static str>,
) -> RawSite {
    RawSite {
        start: node.start_byte(),
        end: node.end_byte(),
        kind,
        spelling: text(doc, node).to_owned(),
        destination,
    }
}
fn extract(
    doc: &DocumentInput,
    tree: &Tree,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<RawSite>> {
    let mut result = Vec::new();
    walk(tree.root_node(), check, &mut |node, check| {
        ensure!(result.len() < SITES, "bounded_work: reference site count");
        match doc.key.language.as_str() {
            "rust" => {
                if opaque(node) {
                    return Ok(true);
                }
                if node.kind() == "use_declaration" {
                    let public = node.child_by_field_name("visibility").is_some()
                        || text(doc, node).trim_start().starts_with("pub");
                    if let Some(argument) = node.child_by_field_name("argument") {
                        rust_use(doc, argument, "", public, &mut result, check)?;
                    }
                    return Ok(true);
                }
                if matches!(node.kind(), "scoped_identifier" | "scoped_type_identifier") {
                    result.push(raw(doc, node, "reference", rust_parts(text(doc, node))));
                    return Ok(true);
                }
            }
            "javascript" => {
                if matches!(node.kind(), "import_statement" | "export_statement")
                    && let Some(source) = node.child_by_field_name("source")
                {
                    let value = text(doc, source);
                    let specifier = value.get(1..value.len().saturating_sub(1)).unwrap_or("");
                    let destination = if specifier.contains('\\') {
                        Err("import_escape_unsupported")
                    } else if specifier.starts_with('.') {
                        Ok(vec![specifier.into()])
                    } else {
                        Err("absolute_or_bare_import")
                    };
                    result.push(raw(doc, source, "import", destination));
                    return Ok(true);
                }
                if node.kind() == "call_expression"
                    && node
                        .child_by_field_name("function")
                        .is_some_and(|n| matches!(text(doc, n), "import" | "require"))
                {
                    result.push(raw(doc, node, "import", Err("dynamic_import_unsupported")));
                    return Ok(true);
                }
            }
            "python" => {
                if node.kind() == "import_from_statement" {
                    let module = node.child_by_field_name("module_name");
                    let has_alias = text(doc, node).split_whitespace().any(|p| p == "as");
                    let destination = if has_alias {
                        Err("import_alias_unsupported")
                    } else if text(doc, node).contains('*') {
                        Err("import_wildcard_unsupported")
                    } else if let Some(module) = module {
                        let spelling = text(doc, module);
                        if !spelling.starts_with('.') {
                            Err("absolute_import_unsupported")
                        } else if spelling.chars().all(|c| c == '.') {
                            Err("module_or_attribute_ambiguous")
                        } else {
                            Ok(vec![spelling.into()])
                        }
                    } else {
                        Err("module_or_attribute_ambiguous")
                    };
                    result.push(raw(doc, module.unwrap_or(node), "import", destination));
                    return Ok(true);
                }
                if node.kind() == "import_statement" {
                    result.push(raw(doc, node, "import", Err("absolute_import_unsupported")));
                    return Ok(true);
                }
            }
            "java" => {
                if node.kind() == "import_declaration" {
                    result.push(raw(
                        doc,
                        node,
                        "import",
                        Err("java_package_mapping_unavailable"),
                    ));
                    return Ok(true);
                }
                if node.kind() == "scoped_type_identifier" {
                    result.push(raw(
                        doc,
                        node,
                        "reference",
                        Err("java_package_mapping_unavailable"),
                    ));
                    return Ok(true);
                }
            }
            _ => {}
        }
        ensure!(result.len() <= SITES, "bounded_work: reference site count");
        Ok(false)
    })?;
    result.sort_by(|a, b| {
        (a.start, a.end, a.kind, &a.spelling).cmp(&(b.start, b.end, b.kind, &b.spelling))
    });
    Ok(result)
}
fn rust_parts(value: &str) -> std::result::Result<Vec<String>, &'static str> {
    let parts: Vec<_> = value.split("::").map(str::trim).collect();
    if parts.first() != Some(&"crate") {
        return Err("rust_non_crate_path");
    }
    if parts.len() < 2
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c == '_' || c.is_alphanumeric()))
    {
        return Err("rust_path_unsupported");
    }
    Ok(parts[1..].iter().map(|s| (*s).to_owned()).collect())
}
fn rust_use(
    doc: &DocumentInput,
    root: Node<'_>,
    prefix: &str,
    public: bool,
    result: &mut Vec<RawSite>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut stack = vec![(root, prefix.to_owned())];
    let mut visits = 0;
    while let Some((node, prefix)) = stack.pop() {
        check()?;
        visits += 1;
        ensure!(
            visits <= VISITS && result.len() < SITES,
            "bounded_work: reference import work"
        );
        match node.kind() {
            "scoped_use_list" => {
                let path = node
                    .child_by_field_name("path")
                    .map(|n| text(doc, n))
                    .unwrap_or("");
                let combined = if prefix.is_empty() {
                    path.to_owned()
                } else {
                    format!("{prefix}::{path}")
                };
                if let Some(list) = node.child_by_field_name("list") {
                    stack.push((list, combined));
                }
            }
            "use_list" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    stack.push((child, prefix.clone()));
                }
            }
            "use_as_clause" => result.push(raw(
                doc,
                node,
                "import",
                Err("rust_import_alias_unsupported"),
            )),
            "use_wildcard" => result.push(raw(
                doc,
                node,
                "import",
                Err("rust_import_wildcard_unsupported"),
            )),
            _ => {
                let spelling = if prefix.is_empty() {
                    text(doc, node).to_owned()
                } else {
                    format!("{prefix}::{}", text(doc, node))
                };
                result.push(raw(
                    doc,
                    node,
                    "import",
                    if public {
                        Err("rust_reexport_unsupported")
                    } else {
                        rust_parts(&spelling)
                    },
                ));
            }
        }
    }
    Ok(())
}
fn map_target(
    doc: &DocumentInput,
    site: &RawSite,
    documents: &BTreeMap<DocumentKey, usize>,
    layout: Option<&RustLayout>,
) -> std::result::Result<usize, &'static str> {
    let parts = site.destination.as_ref().map_err(|e| *e)?;
    let candidates = match doc.key.language.as_str() {
        "rust" => {
            let layout = layout.ok_or("rust_layout_unsupported")?;
            if layout.custom {
                return Err("rust_custom_path_unsupported");
            }
            if layout.wildcard {
                return Err("rust_import_wildcard_unsupported");
            }
            if !doc.key.path.starts_with("src/")
                || doc.key.path.starts_with("src/bin/")
                || parts.first().is_some_and(|part| part == "bin")
            {
                return Err("rust_custom_root_unsupported");
            }
            if parts
                .first()
                .is_some_and(|part| layout.blocked.contains(part))
            {
                return Err("rust_inline_or_alias_ambiguous");
            }
            let mut candidates = Vec::new();
            for length in (1..=parts.len()).rev() {
                let prefix = parts[..length].join("/");
                candidates = vec![format!("src/{prefix}.rs"), format!("src/{prefix}/mod.rs")];
                let found = find(doc, &candidates, documents);
                if found.len() > 1 {
                    return Err("module_path_ambiguous");
                }
                if let Some(target) = found.first() {
                    if parts[..length]
                        .iter()
                        .any(|part| layout.blocked.contains(part))
                    {
                        return Err("rust_inline_or_alias_ambiguous");
                    }
                    return Ok(*target);
                }
            }
            candidates
        }
        "javascript" => {
            let path =
                relative_path(&doc.key.path, &parts[0]).ok_or("relative_import_outside_source")?;
            if path.rsplit('/').next().is_some_and(|p| p.contains('.')) {
                vec![path]
            } else {
                vec![
                    format!("{path}.js"),
                    format!("{path}.mjs"),
                    format!("{path}.cjs"),
                    format!("{path}/index.js"),
                ]
            }
        }
        "python" => {
            let value = &parts[0];
            let dots = value.chars().take_while(|c| *c == '.').count();
            let tail = value[dots..].replace('.', "/");
            let relative = format!("{}{tail}", "../".repeat(dots.saturating_sub(1)));
            let path =
                relative_path(&doc.key.path, &relative).ok_or("relative_import_outside_source")?;
            vec![format!("{path}.py"), format!("{path}/__init__.py")]
        }
        _ => return Err("module_mapping_unavailable"),
    };
    let found = find(doc, &candidates, documents);
    match found.as_slice() {
        [target] => Ok(*target),
        [] => Err("module_path_not_indexed"),
        _ => Err("module_path_ambiguous"),
    }
}
fn find(
    doc: &DocumentInput,
    candidates: &[String],
    documents: &BTreeMap<DocumentKey, usize>,
) -> Vec<usize> {
    candidates
        .iter()
        .filter_map(|path| {
            documents
                .get(&DocumentKey {
                    source_set_id: doc.key.source_set_id.clone(),
                    language: doc.key.language.clone(),
                    path: path.clone(),
                })
                .copied()
        })
        .collect()
}
fn relative_path(source: &str, relative: &str) -> Option<String> {
    let mut parts: Vec<_> = source.split('/').collect();
    parts.pop();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}
