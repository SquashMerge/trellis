//! What a view-time read over a parsed source could not read (#554, #563).
//!
//! Repository content is untrusted, so syntax walks stay bounded, but one bad file must not
//! refuse a whole-checkout result. A walk skips what lies beyond these bounds, and what the
//! parser could not recognize. [`scan`] reports that file once as a [`PartialFile`] row of the
//! result. Skipped syntax proves nothing: resolution keeps every name it could have bound
//! unknown.
use crate::native_evidence::{DocumentKey, Range};
use anyhow::Result;
use serde::Serialize;
use tree_sitter::Node;

/// Syntax levels a walk enters. Native extraction stops one level sooner, so a [`scan`]
/// reports from that level on, and a file it does not name lost nothing to either.
pub(crate) const DEPTH: usize = 128;
/// Syntax nodes one walk visits.
pub(crate) const VISITS: usize = 1_000_000;

/// What one document's syntax lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyntaxGap {
    reason: &'static str,
    /// The first skipped subtree.
    range: Range,
    /// Subtrees skipped over the depth bound before the walk ended, or the file's syntax
    /// errors.
    subtrees: usize,
}
impl SyntaxGap {
    /// A document with no evidence but its file module, over the evidence budget (#564) or
    /// not read (#576): see [`module_only_reason`]. This is the whole file. An over-budget
    /// file's syntax can still be read, so it never makes a binding unknown by itself; a skipped
    /// file's placeholder is a parser error, so it proves nothing.
    pub(crate) fn module_only(reason: &'static str, bytes: usize) -> Self {
        Self {
            reason,
            range: Range {
                start: 0,
                end: bytes,
            },
            subtrees: 0,
        }
    }

    pub(crate) fn file(&self, key: &DocumentKey) -> PartialFile {
        PartialFile {
            source_set_id: key.source_set_id.clone(),
            language: key.language.clone(),
            path: key.path.clone(),
            reason: self.reason,
            range: self.range.clone(),
            subtrees: self.subtrees,
        }
    }
}

/// One result row naming a file whose syntax was only partly read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialFile {
    pub source_set_id: String,
    pub language: String,
    pub path: String,
    /// `syntax_error`, `depth_limit`, `visit_limit`, `evidence_budget`, `oversized` or
    /// `not_utf8`.
    pub reason: &'static str,
    pub range: Range,
    pub subtrees: usize,
}

/// The `partialFiles` reason of a document published with only its file module, from its
/// stored coverage diagnostic.
pub(crate) fn module_only_reason(diagnostic: Option<&str>) -> Option<&'static str> {
    use crate::skipped_source::Skip;
    let diagnostic = diagnostic?;
    if diagnostic == crate::evidence_budget::DIAGNOSTIC {
        return Some("evidence_budget");
    }
    [Skip::Oversized, Skip::NotUtf8]
        .into_iter()
        .find(|skip| skip.diagnostic() == diagnostic)
        .map(|skip| skip.reason())
}

/// The limitation a result carries when it has partial files.
pub(crate) fn limitation(files: usize) -> String {
    format!(
        "{files} source file(s) could only be partly read: they have syntax errors, exceed the syntax bounds ({DEPTH} levels, {VISITS} nodes), exceed the per-file evidence budget, or were not read because they exceed --max-file-bytes or aren't UTF-8. Evidence inside partialFiles is missing, and resolution that depends on a partial file stays unknown."
    )
}

/// Whether and how a tree lost syntax: its parser errors, otherwise the bounds it exceeds.
/// Iterative, so any depth is safe.
pub(crate) fn scan(
    root: Node<'_>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<SyntaxGap>> {
    if root.has_error() {
        return syntax_errors(root, check).map(Some);
    }
    let mut stack = vec![(root, 0usize)];
    let mut visits = 0usize;
    let mut first = None;
    let mut subtrees = 0usize;
    while let Some((node, depth)) = stack.pop() {
        check()?;
        if depth >= DEPTH {
            subtrees += 1;
            first.get_or_insert(range(node));
            continue;
        }
        visits += 1;
        if visits > VISITS {
            return Ok(Some(SyntaxGap {
                reason: "visit_limit",
                range: first.unwrap_or_else(|| range(node)),
                subtrees,
            }));
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev().map(|child| (child, depth + 1)));
    }
    Ok(first.map(|range| SyntaxGap {
        reason: "depth_limit",
        range,
        subtrees,
    }))
}

/// The errors of a tree with `has_error`, entering only the subtrees that hold one.
fn syntax_errors(root: Node<'_>, check: &mut impl FnMut() -> Result<()>) -> Result<SyntaxGap> {
    let mut stack = vec![root];
    let mut visits = 0usize;
    let mut first = None;
    let mut subtrees = 0usize;
    while let Some(node) = stack.pop() {
        check()?;
        visits += 1;
        if visits > VISITS {
            break;
        }
        if node.is_error() || node.is_missing() {
            subtrees += 1;
            first.get_or_insert(range(node));
        } else if node.has_error() {
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
    }
    Ok(SyntaxGap {
        reason: "syntax_error",
        range: first.unwrap_or_else(|| range(root)),
        subtrees,
    })
}

fn range(node: Node<'_>) -> Range {
    Range {
        start: node.start_byte(),
        end: node.end_byte(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gap(source: &str) -> Option<SyntaxGap> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        scan(tree.root_node(), &mut || Ok(())).unwrap()
    }

    #[test]
    fn reports_the_first_subtree_past_the_depth_bound() {
        assert_eq!(gap("[[[1]]];"), None);
        let source = format!("{}{};", "[".repeat(200), "]".repeat(200));
        let gap = gap(&source).unwrap();
        assert_eq!(gap.reason, "depth_limit");
        assert!(gap.range.start > 0 && gap.range.end < source.len());
        assert!(gap.subtrees >= 1);
    }

    #[test]
    fn a_tree_of_any_depth_scans_without_recursion() {
        let source = format!("{}{};", "[".repeat(200_000), "]".repeat(200_000));
        assert_eq!(gap(&source).unwrap().reason, "depth_limit");
    }

    #[test]
    fn stops_at_the_visit_bound() {
        let gap = gap(&format!("[{}];", "0,".repeat(VISITS))).unwrap();
        assert_eq!(gap.reason, "visit_limit");
    }

    #[test]
    fn a_syntax_error_is_reported_with_its_first_range_and_count() {
        let source = "let a = 1;\nlet = ;\nlet b = 2;\nclass { } )\n";
        let gap = gap(source).unwrap();
        assert_eq!(gap.reason, "syntax_error");
        assert!(gap.subtrees >= 2, "{gap:?}");
        assert!(gap.range.start < source.find("class").unwrap());
    }

    #[test]
    fn a_missing_token_is_a_syntax_error() {
        let gap = gap("function f() {").unwrap();
        assert_eq!(gap.reason, "syntax_error");
        assert_eq!(gap.subtrees, 1);
    }

    #[test]
    fn a_syntax_error_in_syntax_of_any_depth_scans_without_recursion() {
        let source = format!("{}{};", "[".repeat(200_000), ")".repeat(3));
        assert_eq!(gap(&source).unwrap().reason, "syntax_error");
    }

    #[test]
    fn scan_observes_cancellation() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("1;", None).unwrap();
        let error = scan(tree.root_node(), &mut || anyhow::bail!("cancelled")).unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
    }
}
