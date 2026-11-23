//! What a view-time read over a parsed source could not read (#554, #563).
//!
//! Repository content is untrusted, so syntax walks stay bounded, but one bad file must not
//! refuse a whole-checkout result. A walk skips what lies beyond these bounds, and what the
//! parser could not recognize. [`scan`] reports that file once as a [`PartialFile`] row of the
//! result. Skipped syntax proves nothing: resolution keeps every name it could have bound
//! unknown.
use crate::native_records::{DocumentKey, Range};
use anyhow::Result;
use serde::Serialize;
use tree_sitter::Node;

/// Syntax levels a walk enters. Native extraction stops one level sooner, so a [`scan`]
/// reports from that level on, and a file it does not name lost nothing to either.
pub(crate) const DEPTH: usize = 128;
/// Syntax nodes one walk visits.
pub(crate) const VISITS: usize = 1_000_000;
/// Parse work, in tree-sitter progress ticks, that the MCP lookup-key derivation and the Files
/// outline may spend on one document: its parse and its macro re-parses share it (owner ruling
/// 2026-11-19, #1220). The pinned tree-sitter fires the progress callback roughly every 100
/// units of the parser's operation counter, which also takes weighted balance and compression
/// increments, so a tick is not a measure of CPU work. The tick count is deterministic for a
/// given input and grammar, never dependent on the clock. A parse whose count exceeds the bound
/// is stopped, so a pathological document is refused before its parse ends, however loaded the
/// machine. The largest real documents measured spend at most 52,330 ticks, and a Rust array
/// just under the visit bound 58,842. The document's own parse counts each tick at
/// [`parse_work`].
pub(crate) const PARSE_TICKS: usize = 80_000;
/// What one progress tick of a document's own parse counts toward [`PARSE_TICKS`] while
/// tree-sitter's parse state reports an error (owner ruling 2026-11-20, #1227). On 2 MiB of
/// syntax errors, such a tick takes on average about 1 to 25 times a clean tick's time: 25 for
/// random JavaScript tokens, 7 to 10 for random punctuation, near 1 for one long unbalanced run.
/// 25 also keeps the highest count measured on 1,840 large real files with an injected syntax
/// error (49,330) as far under the bound as the largest real file (52,330). A document that is
/// mostly syntax errors is then refused by the bound well before its parse ends.
pub(crate) const ERROR_TICK_WEIGHT: usize = 25;

/// The parse work one progress tick counts toward [`PARSE_TICKS`] (#1227): one, or
/// [`ERROR_TICK_WEIGHT`] while tree-sitter's parse state reports an error. That is the only
/// error signal the progress callback gets, and it costs nothing; the parser's logger would
/// name every recovery step, but slows every parse three to five times. The flag turns on when
/// the parser skips a token with every parse version in error recovery, and stays on until a
/// later skip finds a version that recovered. So it is a property of the input alone and the
/// count stays deterministic. A document without syntax errors never sets it, so its count is
/// its tick count, as before. The flag misses recovery that always keeps one version out of
/// error, and it can stay on after the parser has recovered, so it only approximates recovery
/// work. Macro re-parses count one per tick: each one on the way to a measured impl is a
/// re-parse that measurement found free of errors.
pub(crate) fn parse_work(state: &tree_sitter::ParseState) -> usize {
    if state.has_error() {
        ERROR_TICK_WEIGHT
    } else {
        1
    }
}

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
    /// measurement's parse-work budget (#1282), or not read (#576): see
    /// [`module_only_reason`]. This is the whole file. An over-budget file's syntax can still be
    /// read, so it never makes a binding unknown by itself; a skipped file's placeholder is a
    /// parser error, so it proves nothing.
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
    // A parse past measurement's parse-work budget (#1282) is one more per-file budget.
    if diagnostic == crate::evidence_limits::DIAGNOSTIC
        || diagnostic == crate::measured_parse::DIAGNOSTIC
    {
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

/// Inputs and a counter for the parse-work weight's tests (#1227).
#[cfg(test)]
pub(crate) mod parse_work_inputs {
    use super::parse_work;

    /// 1 MiB of unbalanced closing brackets: tree-sitter spends almost its whole parse in error
    /// recovery, in every grammar, and the parse state says so from its first tick. Its ticks
    /// (about 16,000) are well under the parse-work bound.
    pub(crate) fn syntax_errors() -> String {
        ") ] } ".repeat((1 << 20) / 6)
    }

    /// Clean source in `language` of at most `bytes`, as close to it as whole items allow.
    pub(crate) fn clean_of_size(language: &str, bytes: usize) -> String {
        let item = match language {
            "rust" => "impl Saaaaaaaa { fn fbbbbbbbb() {} }\n",
            "python" => "def faaaaaaaa(b):\n    return b + 1\n",
            _ => "function faaaaaaaa(b) { return b + 1; }\n",
        };
        item.repeat(bytes / item.len())
    }

    pub(crate) fn grammar(language: &str) -> tree_sitter::Language {
        match language {
            "rust" => tree_sitter_rust::LANGUAGE.into(),
            "javascript" => tree_sitter_javascript::LANGUAGE.into(),
            "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "python" => tree_sitter_python::LANGUAGE.into(),
            _ => unreachable!("{language}"),
        }
    }

    /// A whole parse's progress ticks, and its parse work as the bounded readers count it.
    pub(crate) fn ticks_and_work(language: &str, text: &str) -> (usize, usize) {
        let mut parser = crate::measured_parse::parser();
        parser.set_language(&grammar(language)).unwrap();
        let (mut ticks, mut work) = (0, 0);
        let mut progress = |state: &tree_sitter::ParseState| {
            ticks += 1;
            work += parse_work(state);
            std::ops::ControlFlow::Continue(())
        };
        parser
            .parse_with_options(
                &mut |offset, _| &text.as_bytes()[offset..],
                None,
                Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
            )
            .unwrap();
        (ticks, work)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1282: a document past measurement's parse-work budget is one more `evidence_budget`
    /// document in reads, beside one past the stored budgets and the two kinds never read.
    #[test]
    fn module_only_reasons_name_every_module_only_diagnostic() {
        use crate::skipped_source::Skip;
        for (diagnostic, reason) in [
            (crate::evidence_limits::DIAGNOSTIC, "evidence_budget"),
            (crate::measured_parse::DIAGNOSTIC, "evidence_budget"),
            (Skip::Oversized.diagnostic(), "oversized"),
            (Skip::NotUtf8.diagnostic(), "not_utf8"),
        ] {
            assert_eq!(module_only_reason(Some(diagnostic)), Some(reason));
        }
        assert_eq!(
            module_only_reason(Some("parser recovered from invalid source")),
            None
        );
        assert_eq!(module_only_reason(None), None);
    }

    fn gap(source: &str) -> Option<SyntaxGap> {
        let mut parser = crate::measured_parse::parser();
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

    /// Owner ruling 2026-11-20 (#1227): a document that is mostly syntax errors counts its
    /// error-recovery ticks at [`ERROR_TICK_WEIGHT`], so its parse work is over the bound though
    /// its ticks are not, in every language the bound covers. A clean document of the same size
    /// counts exactly its ticks, as before the weight. Each count is the same on every run.
    #[test]
    fn error_recovery_counts_more_parse_work_and_a_clean_parse_counts_its_ticks() {
        use parse_work_inputs::*;
        let garbage = syntax_errors();
        for language in ["rust", "javascript", "typescript", "python"] {
            let count = |text: &str| {
                let runs: Vec<_> = (0..3).map(|_| ticks_and_work(language, text)).collect();
                assert!(runs.windows(2).all(|w| w[0] == w[1]), "{language} {runs:?}");
                runs[0]
            };
            let (ticks, work) = count(&garbage);
            assert!(
                ticks < PARSE_TICKS && work > PARSE_TICKS,
                "{language} {ticks} {work}"
            );
            let (ticks, work) = count(&clean_of_size(language, garbage.len()));
            assert_eq!(work, ticks, "{language}");
        }
    }

    #[test]
    fn scan_observes_cancellation() {
        let mut parser = crate::measured_parse::parser();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("1;", None).unwrap();
        let error = scan(tree.root_node(), &mut || anyhow::bail!("cancelled")).unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
    }
}
