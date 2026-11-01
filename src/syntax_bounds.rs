//! The traversal bounds of view-time reads over a parsed source (#554).
//!
//! Repository content is untrusted, so syntax walks stay bounded, but one pathological file
//! must not refuse a whole-checkout result. A walk skips what lies beyond these bounds, and
//! [`scan`] reports that file once as a [`PartialFile`] row of the result.
use crate::native_evidence::{DocumentKey, Range};
use anyhow::Result;
use serde::Serialize;
use tree_sitter::Node;

/// Syntax levels a walk enters. Native extraction stops one level sooner, so a [`scan`]
/// reports from that level on, and a file it does not name lost nothing to either.
pub(crate) const DEPTH: usize = 128;
/// Syntax nodes one walk visits.
pub(crate) const VISITS: usize = 1_000_000;

/// How one document's syntax exceeds the bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyntaxGap {
    reason: &'static str,
    /// The first skipped subtree.
    range: Range,
    /// Subtrees skipped over the depth bound before the walk ended.
    subtrees: usize,
}
impl SyntaxGap {
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
    /// `depth_limit` or `visit_limit`.
    pub reason: &'static str,
    pub range: Range,
    pub subtrees: usize,
}

/// The limitation a result carries when it has partial files.
pub(crate) fn limitation(files: usize) -> String {
    format!(
        "{files} source file(s) exceed the syntax bounds ({DEPTH} levels, {VISITS} nodes); evidence inside partialFiles is missing, and resolution that depends on a partial file stays unknown."
    )
}

/// Whether and how a tree exceeds the bounds. Iterative, so any depth is safe.
pub(crate) fn scan(
    root: Node<'_>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<SyntaxGap>> {
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
