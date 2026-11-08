//! HTTP interface evidence (#788): the server routes a checkout registers and the client calls
//! it makes. Extraction reads parsed source and the stored declaration roles, knows nothing
//! about apps, and runs at read time; nothing here is stored.
#![cfg_attr(not(test), allow(dead_code))]

mod routes;

use crate::native_evidence::{Declaration, DeclarationRole, DocumentKey, Range};
use crate::references::{DocumentInput, work::Work};
use anyhow::{Context, Result};
use serde::Serialize;
use tree_sitter::Node;

/// How firmly a client call is tied to a server route. Client calls (a3) are its only users.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Confidence {
    Exact,
    Template,
    WrapperInferred,
    Unresolved,
}

/// What the handler anchor points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum HandlerKind {
    /// A named function declaration.
    Declaration,
    /// An inline closure, anchored to the named callable around it.
    Closure,
    /// A match arm of a dispatch table's answering `match`, anchored to the callable around it.
    DispatchArm,
}

/// 1-based, inclusive line span.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Lines {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Handler {
    pub(crate) kind: HandlerKind,
    /// The declaration, or the innermost named callable around the span.
    pub(crate) syntax_id: Option<String>,
    pub(crate) stable_lookup_key: Option<String>,
    pub(crate) document: DocumentKey,
    pub(crate) range: Range,
    pub(crate) lines: Lines,
    /// The dispatch table variant that answers the route.
    pub(crate) variant: Option<String>,
}

/// One path segment. A substitution is `{name}`, a rest capture is `{*name}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Seg {
    Literal(String),
    Param,
    Rest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Route {
    pub(crate) method: String,
    pub(crate) path_template: String,
    pub(crate) segments: Vec<Seg>,
    /// Expanded from a dispatch table, not registered literally.
    pub(crate) from_table: bool,
    pub(crate) handler: Handler,
}

/// Splits a `/`-rooted template into segments. A trailing empty segment is dropped.
pub(crate) fn segments(template: &str) -> Vec<Seg> {
    let mut parts: Vec<&str> = template.trim_start_matches('/').split('/').collect();
    if parts.last().is_some_and(|last| last.is_empty()) {
        parts.pop();
    }
    parts
        .into_iter()
        .map(|part| {
            if part.starts_with("{*") {
                Seg::Rest
            } else if part.contains('{') {
                Seg::Param
            } else {
                Seg::Literal(part.to_owned())
            }
        })
        .collect()
}

pub(crate) const BUDGET: &str = "bounded_work: interface extraction budget";

pub(crate) fn charge(work: &mut Work, amount: usize) -> Result<()> {
    anyhow::ensure!(work.charge(amount), BUDGET);
    Ok(())
}

/// Whether the document's file module is test code (Decision 0011). The file module is the
/// single module declaration with no ancestors.
pub(crate) fn test_file(doc: &DocumentInput) -> bool {
    let mut roots = doc
        .declarations
        .iter()
        .filter(|d| d.kind == "module" && d.ancestors.is_empty());
    match (roots.next(), roots.next()) {
        (Some(root), None) => root.role == DeclarationRole::Test,
        _ => false,
    }
}

/// Whether the innermost declaration containing the span is test code.
pub(crate) fn test_span(doc: &DocumentInput, span: &Range) -> bool {
    let size = |d: &&Declaration| d.range.end - d.range.start;
    let containing = || {
        doc.declarations
            .iter()
            .filter(|d| d.range.start <= span.start && span.end <= d.range.end)
    };
    let Some(smallest) = containing().map(|d| size(&d)).min() else {
        return false;
    };
    containing()
        .filter(|d| size(d) == smallest)
        .any(|d| d.role == DeclarationRole::Test)
}

/// The innermost named callable containing the span.
pub(crate) fn named_callable<'d>(doc: &'d DocumentInput, span: &Range) -> Option<&'d Declaration> {
    doc.declarations
        .iter()
        .filter(|d| {
            crate::metrics::callable(&d.kind)
                && d.name.is_some()
                && d.range.start <= span.start
                && span.end <= d.range.end
        })
        .min_by_key(|d| d.range.end - d.range.start)
}

const TEST_SUPPORT_ATTRIBUTE: &str = "#[cfg(feature=\"test-support\")]";

pub(crate) fn comment(node: Node<'_>) -> bool {
    matches!(node.kind(), "line_comment" | "block_comment")
}

fn test_support_attribute(node: Node<'_>, source: &str) -> bool {
    node.kind() == "attribute_item"
        && source.get(node.byte_range()).is_some_and(|text| {
            text.split_whitespace().collect::<String>() == TEST_SUPPORT_ATTRIBUTE
        })
}

/// Whether the node, or an ancestor, carries `#[cfg(feature = "test-support")]` exactly,
/// whitespace aside. Attributes are the node's preceding siblings, or its leading children for
/// a match arm, which owns its own.
pub(crate) fn under_test_support(node: Node<'_>, source: &str) -> bool {
    let mut current = Some(node);
    while let Some(node) = current {
        let mut previous = node.prev_named_sibling();
        while let Some(sibling) = previous {
            if test_support_attribute(sibling, source) {
                return true;
            }
            if !(sibling.kind() == "attribute_item" || comment(sibling)) {
                break;
            }
            previous = sibling.prev_named_sibling();
        }
        if node.kind() == "match_arm" {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if test_support_attribute(child, source) {
                    return true;
                }
                if !(child.kind() == "attribute_item" || comment(child)) {
                    break;
                }
            }
        }
        current = node.parent();
    }
    false
}

/// Byte offsets to 1-based lines.
pub(crate) struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(at, _)| at + 1))
            .collect();
        Self { starts }
    }

    fn line(&self, offset: usize) -> usize {
        self.starts.partition_point(|&start| start <= offset)
    }

    pub(crate) fn lines(&self, range: &Range) -> Lines {
        let last = range.end.saturating_sub(1).max(range.start);
        Lines {
            start: self.line(range.start),
            end: self.line(last),
        }
    }
}

/// Parses one document. An abort from `check` is returned as is.
pub(crate) fn parse(
    doc: &DocumentInput,
    language: &tree_sitter::Language,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<tree_sitter::Tree> {
    check()?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(language)?;
    let source = &doc.file.text;
    let mut aborted = None;
    let tree = parser.parse_with_options(
        &mut |offset, _| &source.as_bytes()[offset..],
        None,
        Some(
            tree_sitter::ParseOptions::new().progress_callback(&mut |_| {
                if let Err(e) = check() {
                    aborted = Some(e);
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            }),
        ),
    );
    if let Some(e) = aborted {
        return Err(e);
    }
    tree.context("bounded_work: interface extraction parser stopped")
}
