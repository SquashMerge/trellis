//! Search-only measurements from the native extractor's existing tree and captured bytes.
use crate::{
    model::SourceFile,
    native_evidence::{Call, Declaration},
    search_card::{self, Ancestor, CardInput, MeasuredFacts, SearchCard},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct CapturedCard {
    pub path: String,
    pub syntax_id: String,
    pub card: SearchCard,
    pub ancestors: Vec<Ancestor>,
    pub facts: search_card::PreparedFacts,
}

/// Re-render owned captured facts against the declaration's current measured header.
pub fn render_card(declaration: &Declaration, captured: &CapturedCard) -> SearchCard {
    search_card::serialize(&CardInput {
        kind: &declaration.kind,
        name: declaration.name.as_deref().unwrap_or(""),
        ancestors: &captured.ancestors,
        path: &captured.path,
        header: &declaration.header,
        facts: &captured.facts,
    })
}

#[derive(Default)]
struct Facts {
    doc: String,
    calls: Vec<String>,
    types: Vec<String>,
    strings: Vec<String>,
    members: Vec<String>,
    ancestors: Vec<Ancestor>,
    body: Option<std::ops::Range<usize>>,
}

struct Collector<'a> {
    file: &'a SourceFile,
    declarations: &'a [Declaration],
    nodes: &'a BTreeMap<usize, String>,
    indices: BTreeMap<&'a str, usize>,
    facts: BTreeMap<usize, Facts>,
    owners: BTreeMap<String, Option<usize>>,
    docstrings: BTreeSet<usize>,
    visits: usize,
}

/// This walks the same tree after native calls and declarations have been measured.
/// No parser, filesystem access, display key or evidence serialization is involved.
pub(crate) fn capture(
    root: Node<'_>,
    file: &SourceFile,
    declarations: &[Declaration],
    calls: &[Call],
    nodes: &BTreeMap<usize, String>,
) -> Result<Vec<CapturedCard>> {
    let mut collector = Collector {
        file,
        declarations,
        nodes,
        indices: declarations
            .iter()
            .enumerate()
            .map(|(i, d)| (d.syntax_id.as_str(), i))
            .collect(),
        facts: BTreeMap::new(),
        owners: BTreeMap::new(),
        docstrings: BTreeSet::new(),
        visits: 0,
    };
    collector.walk(root, &mut Vec::new(), None, 0, false)?;
    let mut ordered_calls: Vec<_> = calls.iter().collect();
    ordered_calls.sort_by_key(|call| (call.range.start, call.range.end, call.ordinal));
    for call in ordered_calls {
        if let Some(Some(owner)) = collector.owners.get(&call.owner_syntax_id)
            && let Some(spelling) = &call.spelling
        {
            push_unique(
                &mut collector.facts.get_mut(owner).unwrap().calls,
                search_card::space(spelling),
                search_card::CALL_COUNT,
            );
        }
    }
    let mut cards = Vec::new();
    for (index, facts) in collector.facts {
        let declaration = &declarations[index];
        let prepared = search_card::prepare_facts(
            &declaration.kind,
            &declaration.header,
            MeasuredFacts {
                doc: &facts.doc,
                calls: &facts.calls,
                body_types: &facts.types,
                strings: &facts.strings,
                members: &facts.members,
                head: &file.text[declaration.range.start..declaration.range.end],
            },
        );
        let card = search_card::serialize(&CardInput {
            kind: &declaration.kind,
            name: declaration.name.as_deref().unwrap(),
            ancestors: &facts.ancestors,
            path: &file.path,
            header: &declaration.header,
            facts: &prepared,
        });
        cards.push(CapturedCard {
            path: file.path.clone(),
            syntax_id: declaration.syntax_id.clone(),
            card,
            ancestors: facts.ancestors,
            facts: prepared,
        });
    }
    Ok(cards)
}

fn push_unique(values: &mut Vec<String>, value: String, cap: usize) {
    if values.len() < cap && !value.is_empty() && !values.contains(&value) {
        values.push(value);
    }
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}
fn text<'a>(file: &'a SourceFile, node: Node<'_>) -> &'a str {
    &file.text[node.byte_range()]
}
fn opaque(language: &str, node: Node<'_>) -> bool {
    language == "rust"
        && matches!(
            node.kind(),
            "macro_invocation"
                | "macro_definition"
                | "token_tree"
                | "attribute_item"
                | "inner_attribute_item"
                | "lifetime"
                | "label"
                | "metavariable"
        )
}
fn string_node(language: &str, node: Node<'_>) -> bool {
    match language {
        "rust" => matches!(node.kind(), "string_literal" | "raw_string_literal"),
        "java" => node.kind() == "string_literal",
        "python" => node.kind() == "string",
        "javascript" => matches!(node.kind(), "string" | "template_string"),
        _ => false,
    }
}

/// Delimiters are syntax, while escapes and interpolation remain source spelling.
fn string_payload<'a>(source: &'a str, language: &str) -> &'a str {
    let Some(start) = source.find(['\'', '"', '`']) else {
        return "";
    };
    let delimiter = &source[start..start + 1];
    let width = if matches!(language, "python" | "java")
        && source[start..].starts_with(&delimiter.repeat(3))
    {
        3
    } else {
        1
    };
    let Some(end) = source.rfind(&delimiter.repeat(width)) else {
        return "";
    };
    if end < start + width {
        return "";
    }
    &source[start + width..end]
}

fn python_doc<'tree>(node: Node<'tree>, file: &SourceFile) -> Option<Node<'tree>> {
    if file.language != "python"
        || !matches!(node.kind(), "function_definition" | "class_definition")
    {
        return None;
    }
    let body = node.child_by_field_name("body")?;
    let mut expression = body.named_child(0)?;
    while expression.kind() == "comment" {
        expression = expression.next_named_sibling()?;
    }
    if expression.kind() != "expression_statement" {
        return None;
    }
    let values = children(expression);
    (values.len() == 1 && values[0].kind() == "string").then_some(values[0])
}

fn doc_comment(language: &str, source: &str, inner: bool) -> Option<String> {
    if language == "rust"
        && (source.starts_with("///") && !source.starts_with("////")
            || inner && source.starts_with("//!"))
    {
        return Some(source[3..].trim_end_matches(['\r', '\n']).to_owned());
    }
    let opening = if inner && language == "rust" {
        "/*!"
    } else {
        "/**"
    };
    if !source.starts_with(opening) || source.starts_with("/***") || !source.ends_with("*/") {
        return None;
    }
    // The legal empty comment `/**/` has overlapping opening/closing markers.
    if source.len() < 5 {
        return Some(String::new());
    }
    Some(
        source[3..source.len() - 2]
            .split('\n')
            .enumerate()
            .map(|(i, line)| {
                if i == 0 {
                    return line.to_owned();
                }
                let trimmed = line.trim_start_matches([' ', '\t', '\r']);
                trimmed
                    .strip_prefix('*')
                    .map(|s| s.strip_prefix(' ').unwrap_or(s))
                    .unwrap_or(line)
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn blank_line(gap: &str) -> bool {
    gap.bytes().filter(|b| *b == b'\n').count() > 1
}
fn attached_doc(node: Node<'_>, file: &SourceFile) -> String {
    let mut parts = Vec::new();
    let mut current = if file.language == "javascript"
        && node
            .parent()
            .is_some_and(|parent| parent.kind() == "export_statement")
    {
        node.parent().unwrap()
    } else {
        node
    };
    // Python decorators are handled by its suite docstring, not preceding comments.
    if file.language == "python" {
        return String::new();
    }
    while let Some(previous) = current.prev_named_sibling() {
        let gap = &file.text[previous.end_byte()..current.start_byte()];
        let consumed_newline = text(file, previous).ends_with('\n');
        if !gap.bytes().all(|b| b.is_ascii_whitespace())
            || blank_line(gap)
            || consumed_newline && gap.contains('\n')
        {
            break;
        }
        if file.language == "rust" && previous.kind() == "attribute_item" {
            current = previous;
            continue;
        }
        if let Some(payload) = doc_comment(&file.language, text(file, previous), false) {
            parts.push(payload);
            current = previous;
        } else {
            break;
        }
    }
    parts.reverse();
    parts.join("\n")
}

impl Collector<'_> {
    fn walk(
        &mut self,
        node: Node<'_>,
        stack: &mut Vec<(usize, Ancestor)>,
        mut owner: Option<usize>,
        depth: usize,
        in_string: bool,
    ) -> Result<()> {
        ensure!(
            depth < 128 && self.visits < 1_000_000,
            "native extraction work budget exceeded"
        );
        self.visits += 1;
        if opaque(&self.file.language, node) {
            return Ok(());
        }
        let declaration = self
            .nodes
            .get(&node.id())
            .map(|id| self.indices[id.as_str()]);
        let selected_doc = python_doc(node, self.file);
        if let Some(docstring) = selected_doc {
            self.docstrings.insert(docstring.id());
        }
        if let Some(index) = declaration {
            let d = &self.declarations[index];
            if let Some((parent, _)) = stack.last()
                && search_card::is_container(&self.declarations[*parent].kind)
                && self.declarations[index].kind != "parameter"
                && let Some(name) = &d.name
                && let Some(facts) = self.facts.get_mut(parent)
            {
                facts.members.push(name.clone());
            }
            if search_card::is_eligible(&d.kind, d.name.as_deref()) {
                owner = Some(index);
                let mut facts = Facts {
                    ancestors: stack
                        .iter()
                        .filter_map(|(_, a)| {
                            (a.name.as_ref().is_some_and(|n| !n.is_empty())
                                || a.self_type.is_some())
                            .then_some(a.clone())
                        })
                        .collect(),
                    doc: selected_doc
                        .map(|doc| {
                            string_payload(text(self.file, doc), &self.file.language).to_owned()
                        })
                        .unwrap_or_else(|| attached_doc(node, self.file)),
                    body: node
                        .child_by_field_name("body")
                        .map(|body| body.byte_range()),
                    ..Facts::default()
                };
                for type_name in d
                    .header
                    .parameters
                    .iter()
                    .filter_map(|p| p.type_name.as_deref())
                    .chain(d.header.result_type.as_deref())
                {
                    if facts.types.len() == search_card::TYPE_COUNT {
                        break;
                    }
                    push_unique(
                        &mut facts.types,
                        search_card::space(type_name),
                        search_card::TYPE_COUNT,
                    );
                }
                if self.file.language == "rust"
                    && search_card::is_container(&d.kind)
                    && let Some(body) = node.child_by_field_name("body")
                {
                    let mut inner = Vec::new();
                    let mut previous_end = body.start_byte() + 1;
                    let mut consumed_newline = false;
                    for child in children(body) {
                        let gap = &self.file.text[previous_end..child.start_byte()];
                        if !gap.bytes().all(|b| b.is_ascii_whitespace())
                            || blank_line(gap)
                            || consumed_newline && gap.contains('\n')
                        {
                            break;
                        }
                        previous_end = child.end_byte();
                        consumed_newline = text(self.file, child).ends_with('\n');
                        if let Some(payload) = doc_comment("rust", text(self.file, child), true) {
                            // Inner docs only start with the inner marker.
                            if text(self.file, child).starts_with("//!")
                                || text(self.file, child).starts_with("/*!")
                            {
                                inner.push(payload);
                                continue;
                            }
                        }
                        break;
                    }
                    if !inner.is_empty() {
                        if !facts.doc.is_empty() {
                            facts.doc.push('\n');
                        }
                        facts.doc.push_str(&inner.join("\n"));
                    }
                }
                self.facts.insert(index, facts);
            }
            self.owners.insert(d.syntax_id.clone(), owner);
            let implementation = self.file.language == "rust" && d.kind == "implementation";
            stack.push((
                index,
                Ancestor {
                    kind: d.kind.clone(),
                    name: d.name.clone(),
                    self_type: implementation
                        .then(|| {
                            node.child_by_field_name("type")
                                .map(|n| text(self.file, n).to_owned())
                        })
                        .flatten(),
                    trait_name: implementation
                        .then(|| {
                            node.child_by_field_name("trait")
                                .map(|n| text(self.file, n).to_owned())
                        })
                        .flatten(),
                },
            ));
        }
        let literal = string_node(&self.file.language, node);
        if let Some(index) = owner {
            let facts = self.facts.get_mut(&index).unwrap();
            // A Python docstring is the first expression in its owning declaration suite.
            let doc_literal = self.docstrings.contains(&node.id());
            if literal
                && !in_string
                && !doc_literal
                && facts.strings.len() < search_card::LITERAL_COUNT
            {
                push_unique(
                    &mut facts.strings,
                    search_card::prefix(
                        string_payload(text(self.file, node), &self.file.language),
                        search_card::LITERAL_BYTES,
                    )
                    .to_owned(),
                    search_card::LITERAL_COUNT,
                );
            }
            let in_body = facts
                .body
                .as_ref()
                .is_some_and(|body| body.start <= node.start_byte() && node.end_byte() <= body.end);
            if in_body
                && matches!(self.file.language.as_str(), "rust" | "java")
                && node.kind() == "type_identifier"
                && facts.types.len() < search_card::TYPE_COUNT
            {
                push_unique(
                    &mut facts.types,
                    search_card::space(text(self.file, node)),
                    search_card::TYPE_COUNT,
                );
            }
            if in_body
                && self.file.language == "python"
                && matches!(
                    node.kind(),
                    "assignment" | "typed_parameter" | "typed_default_parameter"
                )
                && facts.types.len() < search_card::TYPE_COUNT
                && let Some(type_node) = node.child_by_field_name("type")
            {
                push_unique(
                    &mut facts.types,
                    search_card::space(text(self.file, type_node)),
                    search_card::TYPE_COUNT,
                );
            }
        }
        for child in children(node) {
            self.walk(child, stack, owner, depth + 1, in_string || literal)?;
        }
        if declaration.is_some() {
            stack.pop();
        }
        Ok(())
    }
}
