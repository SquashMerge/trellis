//! View-time, source-only complexity metrics. No index or revision identity is cached here.
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use tree_sitter::{Node, Parser};

use crate::native_evidence::{Call, ControlRegion, Declaration, DeclarationRole, Range};
use crate::native_rust_macros::Expansions;
use crate::store::SelectedIntegrity;
use crate::syntax_bounds::SyntaxGap;

// The memo and the score records it holds are in the shared `metrics_records`.
pub use crate::metrics_records::{
    Exits, Halstead, MetricsMemo, Score, SourceInput, Unavailable, VERSION,
};
use crate::metrics_records::{MemoKey, SourceScores, memo_key};

#[cfg(feature = "test-support")]
thread_local! {
    static IN_PARSER_PROGRESS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(feature = "test-support")]
struct ParserProgressMarker(bool);
#[cfg(feature = "test-support")]
impl ParserProgressMarker {
    fn enter() -> Self {
        Self(IN_PARSER_PROGRESS.with(|value| value.replace(true)))
    }
}
#[cfg(feature = "test-support")]
impl Drop for ParserProgressMarker {
    fn drop(&mut self) {
        IN_PARSER_PROGRESS.with(|value| value.set(self.0));
    }
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn in_parser_progress_for_tests() -> bool {
    IN_PARSER_PROGRESS.with(std::cell::Cell::get)
}

/// Bounds of one declaration's score walk; past them it is partial, never refused (#554).
const MAX_DEPTH: usize = 512;
const MAX_VISITS: usize = 2_000_000;
pub const FAN_IN_REASON: &str = "call resolution not yet produced (#40/#68)";
pub const METRIC_ORDER: [&str; 12] = [
    "loc",
    "sloc",
    "commentLines",
    "cyclomatic",
    "cognitive",
    "maxNesting",
    "halstead",
    "maintainabilityIndex",
    "fanOut",
    "fanIn",
    "parameters",
    "exits",
];

/// The caller's deadline and cancellation signal are checked during parsing and every walk.
/// An error aborts the entire selected document. Syntax past the traversal bounds is not an
/// error: it goes unmeasured and the document reports a [`SyntaxGap`] (#554).
pub fn compute(
    language: &str,
    source: &str,
    declarations: &[Declaration],
    calls: &[Call],
    regions: &[ControlRegion],
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<DeclarationMetrics>> {
    let source_only = compute_source(language, source, declarations, check)?;
    bind(
        source,
        declarations,
        calls,
        regions,
        source_only.scores,
        check,
    )
}

/// A memo hit still binds the caller's current declarations, calls and regions.
pub fn compute_memoized(
    memo: &mut MetricsMemo,
    source: SourceInput<'_>,
    declarations: &[Declaration],
    calls: &[Call],
    regions: &[ControlRegion],
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<DeclarationMetrics>> {
    let cached = memo.lookup(&source);
    compute_with_cached(cached, source, declarations, calls, regions, check)?.commit(memo)
}

/// One document's metrics rows, and its source-only scores when they were
/// computed rather than taken from the memo.
pub(crate) struct Computed {
    rows: Vec<DeclarationMetrics>,
    gap: Option<SyntaxGap>,
    uncached: Option<(MemoKey, SourceScores)>,
}

impl Computed {
    /// Rows computed earlier, with nothing left to record.
    pub(crate) fn ready(rows: Vec<DeclarationMetrics>, gap: Option<SyntaxGap>) -> Self {
        Self {
            rows,
            uncached: None,
            gap,
        }
    }
    /// How this document's syntax exceeded the traversal bounds, if it did.
    pub(crate) fn gap(&self) -> Option<&SyntaxGap> {
        self.gap.as_ref()
    }
    /// Accept this document: record its new scores in `memo`, return its rows.
    pub(crate) fn commit(self, memo: &mut MetricsMemo) -> Result<Vec<DeclarationMetrics>> {
        memo.commit(self.uncached)?;
        Ok(self.rows)
    }
}

/// Compute one document's rows from `cached` source scores (a memo hit) or
/// from its source. The memo itself is untouched: `MetricsMemo::commit`
/// records new scores once the caller accepts this document.
pub(crate) fn compute_with_cached(
    cached: Option<SourceScores>,
    source: SourceInput<'_>,
    declarations: &[Declaration],
    calls: &[Call],
    regions: &[ControlRegion],
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Computed> {
    let was_cached = cached.is_some();
    let scores = match cached {
        Some(scores) => scores,
        None => compute_source(source.language, source.text, declarations, check)?,
    };
    let rows = bind(
        source.text,
        declarations,
        calls,
        regions,
        scores.scores.clone(),
        check,
    )?;
    check()?;
    Ok(Computed {
        rows,
        gap: scores.gap.clone(),
        uncached: (!was_cached).then(|| (memo_key(&source), scores)),
    })
}

// Memo scores follow source positions, never path-dependent native ID order.
fn callable_declarations(declarations: &[Declaration]) -> Vec<&Declaration> {
    let mut callables: Vec<_> = declarations.iter().filter(|d| callable(&d.kind)).collect();
    callables.sort_by(|a, b| {
        (a.range.start, a.range.end, &a.kind, &a.name).cmp(&(
            b.range.start,
            b.range.end,
            &b.kind,
            &b.name,
        ))
    });
    callables
}

fn bind(
    source: &str,
    declarations: &[Declaration],
    calls: &[Call],
    regions: &[ControlRegion],
    source_only: Vec<Score>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<DeclarationMetrics>> {
    // One pass over the rows per document: a generated or large file has thousands of
    // declarations and tens of thousands of calls, so a scan per declaration is too slow.
    let mut calls_by_owner: HashMap<&str, Vec<&Call>> = HashMap::new();
    for call in calls {
        calls_by_owner
            .entry(&call.owner_syntax_id)
            .or_default()
            .push(call);
    }
    let mut regions_by_owner: HashMap<&str, Vec<&ControlRegion>> = HashMap::new();
    for region in regions {
        regions_by_owner
            .entry(&region.owner_syntax_id)
            .or_default()
            .push(region);
    }
    let owned_calls = |d: &Declaration| {
        calls_by_owner
            .get(d.syntax_id.as_str())
            .map_or(&[][..], Vec::as_slice)
    };
    let self_call = |d: &Declaration, call: &Call| d.name.as_deref() == call.spelling.as_deref();
    let syntax = if declarations
        .iter()
        .any(|d| callable(&d.kind) && owned_calls(d).iter().any(|call| self_call(d, call)))
    {
        check()?;
        let document = &declarations
            .first()
            .context(SelectedIntegrity(
                "corrupt: call without declaration".into(),
            ))?
            .document;
        let language = document.language.as_str();
        let mut parser = Parser::new();
        parser.set_language(&grammar(language, &document.path)?)?;
        let tree = parser
            .parse(source, None)
            .context("bounded_work: call syntax parse")?;
        check()?;
        Some((tree, language == "rust"))
    } else {
        None
    };
    let mut expansions = Expansions::new(source);
    let mut rows = Vec::new();
    for (d, mut metrics) in callable_declarations(declarations)
        .into_iter()
        .zip(source_only)
    {
        check()?;
        let owned = owned_calls(d);
        metrics.fan_out = owned.len();
        // Exact unqualified self-call spelling is syntactic evidence, not call resolution.
        if let Some(name) = &d.name {
            let mut self_calls = owned
                .iter()
                .copied()
                .filter(|c| c.spelling.as_deref() == Some(name.as_str()))
                .peekable();
            if self_calls.peek().is_some()
                && let Some((tree, rust)) = &syntax
            {
                let root = declaration_root(&mut expansions, tree.root_node(), *rust, &d.range);
                metrics.cognitive += self_calls
                    .filter(|c| unqualified_call(root, c, &d.document.language))
                    .count();
            }
        }
        let owned_regions = regions_by_owner
            .get(d.syntax_id.as_str())
            .map_or(&[][..], Vec::as_slice);
        metrics.max_nesting = max_nesting(source, &d.document.language, owned_regions);
        metrics.parameters = d.header.parameters.len();
        rows.push(DeclarationMetrics {
            role: d.role,
            source_set_id: d.document.source_set_id.clone(),
            language: d.document.language.clone(),
            path: d.document.path.clone(),
            sid: d.syntax_id.clone(),
            range: d.range.clone(),
            name: d.name.clone(),
            kind: d.kind.clone(),
            metrics,
        });
    }
    Ok(rows)
}
/// The deepest of one callable's stored control regions, counting the levels on each
/// region's parent chain (#987). An arm is as deep as its statement and an `else if` as deep
/// as the first `if` of its chain, so neither is a level.
///
/// Every statement that owns a stored arm kind is stored itself (#1120), so an arm's stored
/// parent is its own statement.
fn max_nesting(source: &str, language: &str, owned: &[&ControlRegion]) -> usize {
    let by_id: HashMap<&str, &ControlRegion> = owned.iter().map(|r| (r.id.as_str(), *r)).collect();
    let parent_of =
        |r: &ControlRegion| r.parent_id.as_deref().and_then(|id| by_id.get(id).copied());
    let level = |r: &ControlRegion| {
        let parent = parent_of(r);
        let parent_kind = parent.map_or("", |parent| parent.kind.as_str());
        let arm = match r.kind.as_str() {
            "else_clause" | "elif_clause" => matches!(
                parent_kind,
                "if_statement"
                    | "if_expression"
                    | "for_statement"
                    | "while_statement"
                    | "try_statement"
            ),
            "switch_case" | "switch_default" => parent_kind == "switch_statement",
            "catch_clause" | "except_clause" | "finally_clause" => matches!(
                parent_kind,
                "try_statement" | "try_with_resources_statement"
            ),
            _ => false,
        };
        if arm {
            return false;
        }
        // The `if` of an `else if` ends where its parent does and follows the `else`: under
        // an `else_clause`, or directly under the first `if` in Java, which stores no clause.
        // An `if` in an `else` block ends before the block does, and an unbraced `if (a) if (b)`
        // follows a `)`. Python spells the continuation `elif`, and its `else:` block has no
        // closing token, so an `if` ending it would look like one.
        let continuation = language != "python"
            && matches!(r.kind.as_str(), "if_statement" | "if_expression")
            && matches!(parent_kind, "else_clause" | "if_statement")
            && parent.is_some_and(|parent| parent.range.end == r.range.end)
            && source
                .get(..r.range.start)
                .is_some_and(|before| before.trim_end().ends_with("else"));
        !continuation
    };
    owned
        .iter()
        .map(|r| {
            // Stored parents form a chain; the bound only stops a corrupt cycle.
            std::iter::successors(Some(*r), |r| parent_of(r))
                .take(owned.len())
                .filter(|r| level(r))
                .count()
        })
        .max()
        .unwrap_or(0)
}
/// The tree holding a declaration: the plain tree, unless it sits in a measured Rust macro,
/// whose token tree has no syntax of its own (#551).
fn declaration_root<'a>(
    expansions: &'a mut Expansions<'_>,
    plain: Node<'a>,
    rust: bool,
    range: &Range,
) -> Node<'a> {
    if rust {
        expansions.root_for(plain, range.start, range.end)
    } else {
        plain
    }
}
fn unqualified_call(root: Node<'_>, call: &Call, language: &str) -> bool {
    let Some(callee) = &call.callee_range else {
        return false;
    };
    if callee.start >= callee.end || call.range.start > callee.start || callee.end > call.range.end
    {
        return false;
    }
    let Some(mut node) = root.descendant_for_byte_range(callee.start, callee.end) else {
        return false;
    };
    while let Some(parent) = node.parent() {
        // The `await f<T>(x)` misparse is measured from its awaited callee (#958).
        let awaited = matches!(language, "javascript" | "typescript")
            .then(|| crate::native_evidence::awaited_callee(node))
            .flatten();
        let start = awaited.map_or(node.start_byte(), |callee| callee.start_byte());
        if start == call.range.start && node.end_byte() == call.range.end {
            let field = match language {
                "java" => "name",
                "javascript" | "typescript" if node.kind() == "new_expression" => "constructor",
                _ => "function",
            };
            let expression = awaited.or_else(|| node.child_by_field_name(field));
            return expression.is_some_and(|expression| {
                matches!(expression.kind(), "identifier" | "type_identifier")
                    && expression.start_byte() == callee.start
                    && expression.end_byte() == callee.end
                    && (language != "java" || node.child_by_field_name("object").is_none())
            });
        }
        node = parent;
    }
    false
}

pub fn callable(kind: &str) -> bool {
    matches!(
        kind,
        "function" | "method" | "constructor" | "anonymousFunction"
    )
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeclarationMetrics {
    pub role: DeclarationRole,
    pub source_set_id: String,
    pub language: String,
    pub path: String,
    pub sid: String,
    pub range: Range,
    #[serde(skip)]
    pub name: Option<String>,
    #[serde(skip)]
    pub kind: String,
    pub metrics: Score,
}

fn round(n: f64) -> Result<f64> {
    ensure!(n.is_finite(), "bounded_work: nonfinite metric");
    Ok(if n == 0.0 {
        0.0
    } else {
        (n * 1_000_000.0).round() / 1_000_000.0
    })
}
fn empty() -> Score {
    Score {
        loc: 0,
        sloc: 0,
        comment_lines: 0,
        cyclomatic: 1,
        cognitive: 0,
        max_nesting: 0,
        halstead: Halstead {
            n1: 0,
            n2: 0,
            total_operators: 0,
            total_operands: 0,
            vocabulary: 0,
            length: 0,
            volume: 0.0,
            difficulty: 0.0,
            effort: 0.0,
            estimated_bugs: 0.0,
        },
        maintainability_index: 0.0,
        fan_out: 0,
        fan_in: Unavailable {
            value: None,
            unavailable: FAN_IN_REASON,
        },
        parameters: 0,
        exits: Exits {
            returns: 0,
            throws: 0,
        },
    }
}

/// The path of the document `declarations` belong to; the TypeScript grammar depends on it.
fn document_path(declarations: &[Declaration]) -> &str {
    declarations
        .first()
        .map_or("", |d| d.document.path.as_str())
}
fn grammar(language: &str, path: &str) -> Result<tree_sitter::Language> {
    Ok(match language {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => crate::indexer_ts::grammar(path),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        _ => return Err(SelectedIntegrity("corrupt: unknown stored language".into()).into()),
    })
}
fn compute_source(
    language: &str,
    source: &str,
    declarations: &[Declaration],
    check: &mut impl FnMut() -> Result<()>,
) -> Result<SourceScores> {
    check()?;
    let mut parser = Parser::new();
    parser.set_language(&grammar(language, document_path(declarations))?)?;
    let mut aborted = None;
    let tree = parser.parse_with_options(
        &mut |offset, _| &source.as_bytes()[offset..],
        None,
        Some(
            tree_sitter::ParseOptions::new().progress_callback(&mut |_| {
                #[cfg(feature = "test-support")]
                let _parser_progress = ParserProgressMarker::enter();
                if let Err(error) = check() {
                    aborted = Some(error);
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            }),
        ),
    );
    if let Some(error) = aborted {
        return Err(error);
    }
    check()?;
    let tree = tree.context("bounded_work: parser stopped")?;
    let gap = crate::syntax_bounds::scan(tree.root_node(), check)?;
    let newline_offsets: Vec<usize> = source
        .bytes()
        .enumerate()
        .filter_map(|(i, b)| (b == b'\n').then_some(i))
        .collect();
    let mut expansions = Expansions::new(source);
    let mut result = Vec::new();
    for d in callable_declarations(declarations) {
        check()?;
        ensure!(
            d.range.start <= d.range.end
                && d.range.end <= source.len()
                && source.is_char_boundary(d.range.start)
                && source.is_char_boundary(d.range.end),
            SelectedIntegrity("corrupt: invalid declaration range".into())
        );
        let nested: Vec<_> = declarations
            .iter()
            .filter(|other| {
                callable(&other.kind)
                    && other.syntax_id != d.syntax_id
                    && other.range.start >= d.range.start
                    && other.range.end <= d.range.end
                    && other.range != d.range
            })
            .map(|other| &other.range)
            .collect();
        let rust = language == "rust";
        let root = declaration_root(&mut expansions, tree.root_node(), rust, &d.range);
        result.push(score(
            language,
            source,
            root,
            d,
            &nested,
            &newline_offsets,
            gap.is_some(),
            check,
        )?);
    }
    Ok(SourceScores {
        scores: result,
        gap,
    })
}
// Native callable ranges are declaration ranges. Find the executable body within that
// declaration before counting Halstead leaves; the full range still supplies line counts.
fn callable_body(
    root: Node<'_>,
    source: &str,
    d: &Declaration,
    partial: bool,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Range> {
    let unmeasured = Range {
        start: d.range.end,
        end: d.range.end,
    };
    let mut stack = vec![root];
    let mut selected = None;
    let mut visits = 0;
    while let Some(node) = stack.pop() {
        check()?;
        visits += 1;
        if visits > MAX_VISITS {
            // The scan already reported this file; the body goes unmeasured.
            return Ok(unmeasured);
        }
        if node.end_byte() <= d.range.start || node.start_byte() >= d.range.end {
            continue;
        }
        if node.start_byte() == d.range.start
            && node.end_byte() == d.range.end
            && node.child_by_field_name("body").is_none()
            && node
                .child_by_field_name("name")
                .is_some_and(|name| d.name.as_deref() == Some(&source[name.byte_range()]))
        {
            return Ok(unmeasured);
        }
        if let Some(body) = node.child_by_field_name("body")
            && node.start_byte() >= d.range.start
            && node.end_byte() <= d.range.end
            && node
                .child_by_field_name("name")
                .is_none_or(|name| d.name.as_deref() == Some(&source[name.byte_range()]))
        {
            let exact = node.start_byte() == d.range.start && node.end_byte() == d.range.end;
            if exact {
                return Ok(Range {
                    start: body.start_byte(),
                    end: body.end_byte(),
                });
            }
            if selected.is_none() {
                selected = Some(Range {
                    start: body.start_byte(),
                    end: body.end_byte(),
                });
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    // Recovered syntax can lose the body of a declaration measured around it.
    if partial && selected.is_none() {
        return Ok(unmeasured);
    }
    selected.with_context(|| {
        SelectedIntegrity(format!(
            "corrupt: callable body not found: {} {} {:?} {:?}",
            d.document.language, d.document.path, d.name, d.range
        ))
    })
}

#[allow(clippy::too_many_arguments)] // One callable's measured context stays explicit.
fn score(
    language: &str,
    source: &str,
    root: Node<'_>,
    d: &Declaration,
    nested: &[&Range],
    newline_offsets: &[usize],
    partial: bool,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Score> {
    let body = callable_body(root, source, d, partial, check)?;
    let mut scorer = Scorer {
        language,
        source,
        d,
        body,
        nested,
        newline_offsets,
        s: empty(),
        code_lines: std::collections::BTreeSet::new(),
        comments: std::collections::BTreeSet::new(),
        operators: BTreeMap::new(),
        operands: BTreeMap::new(),
    };
    let mut stack = vec![(root, 0usize)];
    let mut visits = 0usize;
    while let Some((node, depth)) = stack.pop() {
        check()?;
        visits += 1;
        if visits > MAX_VISITS {
            break;
        }
        if depth > MAX_DEPTH {
            continue;
        }
        if scorer.pruned(node) {
            continue;
        }
        if scorer.string_literal(node) {
            continue;
        }
        if node.is_named() {
            if scorer.named(node, depth, &mut stack) {
                continue;
            }
        } else if node.child_count() > 0 {
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev().map(|child| (child, depth)));
            continue;
        }
        scorer.leaf(node);
    }
    scorer.finish()
}

/// One callable's measurement: its context, and what the walk of `score` has counted so far.
struct Scorer<'a> {
    language: &'a str,
    source: &'a str,
    d: &'a Declaration,
    /// The executable body: Halstead leaves are counted inside it only.
    body: Range,
    nested: &'a [&'a Range],
    newline_offsets: &'a [usize],
    s: Score,
    code_lines: std::collections::BTreeSet<usize>,
    comments: std::collections::BTreeSet<usize>,
    operators: BTreeMap<String, usize>,
    operands: BTreeMap<String, usize>,
}

impl Scorer<'_> {
    /// A node outside the declaration, inside a nested callable, or an annotation: neither it
    /// nor anything under it is measured.
    fn pruned(&self, node: Node<'_>) -> bool {
        let (d, nested) = (self.d, self.nested);
        if node.end_byte() <= d.range.start
            || node.start_byte() >= d.range.end
            || nested
                .iter()
                .any(|r| node.start_byte() >= r.start && node.end_byte() <= r.end)
        {
            return true;
        }
        let kind = node.kind();
        if matches!(
            kind,
            "decorator"
                | "annotation"
                | "marker_annotation"
                | "attribute_item"
                | "outer_attribute_item"
                | "inner_attribute_item"
        ) {
            return true;
        }
        false
    }

    /// A string literal without interpolation is one operand and its lines are code; its
    /// children are not walked. Returns whether `node` was one.
    fn string_literal(&mut self, node: Node<'_>) -> bool {
        let (source, d, newline_offsets) = (self.source, self.d, self.newline_offsets);
        let kind = node.kind();
        if matches!(
            kind,
            "string" | "string_literal" | "character_literal" | "raw_string_literal"
        ) && !node.children(&mut node.walk()).any(|child| {
            matches!(
                child.kind(),
                "interpolation" | "template_substitution" | "interpolated_string_expression"
            )
        }) {
            let token = &source[node.byte_range()];
            if node.start_byte() >= self.body.start && node.end_byte() <= self.body.end {
                *self.operands.entry(token.to_owned()).or_default() += 1;
            }
            let lo = node.start_byte().max(d.range.start);
            let hi = node.end_byte().min(d.range.end);
            if lo < hi {
                let first = newline_offsets.partition_point(|&offset| offset < lo);
                let last = newline_offsets.partition_point(|&offset| offset < hi)
                    - usize::from(source.as_bytes()[hi - 1] == b'\n');
                self.code_lines.extend(first..=last);
            }
            return true;
        }
        false
    }

    /// Scores a named node and pushes its children, last first, so that they are visited in
    /// source order. Returns whether it had children; a named node without any is a leaf.
    fn named<'t>(
        &mut self,
        node: Node<'t>,
        depth: usize,
        stack: &mut Vec<(Node<'t>, usize)>,
    ) -> bool {
        let kind = node.kind();
        let structural = matches!(
            kind,
            "if_statement"
                | "for_statement"
                | "enhanced_for_statement"
                | "while_statement"
                | "do_statement"
                | "switch_expression"
                | "switch_statement"
                | "catch_clause"
                | "except_clause"
                | "for_in_statement"
                | "if_expression"
                | "for_expression"
                | "while_expression"
                | "loop_expression"
                | "match_expression"
                | "match_statement"
                | "ternary_expression"
                | "conditional_expression"
        );
        self.branch(node, kind, depth, structural);
        self.match_arm(node, kind);
        self.exit(node, kind);
        let lambda = matches!(
            kind,
            "lambda_expression"
                | "lambda"
                | "arrow_function"
                | "function_expression"
                | "closure_expression"
        );
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        if !children.is_empty() {
            for child in children.into_iter().rev() {
                let continuation = matches!(kind, "if_statement" | "if_expression")
                    && matches!(
                        child.kind(),
                        "if_statement" | "if_expression" | "else_clause"
                    );
                let else_continuation = kind == "else_clause"
                    && matches!(child.kind(), "if_statement" | "if_expression");
                stack.push((
                    child,
                    depth
                        + usize::from(
                            (structural && !continuation)
                                || lambda
                                || (kind == "else_clause" && !else_continuation),
                        ),
                ));
            }
            return true;
        }
        false
    }

    /// Branching structures, and the `elif` and `else` clauses.
    fn branch(&mut self, node: Node<'_>, kind: &str, depth: usize, structural: bool) {
        if structural {
            if !matches!(
                kind,
                "match_statement" | "match_expression" | "switch_expression" | "switch_statement"
            ) {
                self.s.cyclomatic += 1;
            }
            self.s.cognitive += 1 + depth;
        }
        if kind == "elif_clause" {
            self.s.cognitive += 1;
            self.s.cyclomatic += 1;
        } else if kind == "else_clause"
            && !node
                .named_children(&mut node.walk())
                .any(|child| matches!(child.kind(), "if_statement" | "if_expression"))
        {
            self.s.cognitive += 1;
        }
    }

    /// An arm of a `match` or a `switch` is one path, unless it is the default or an unguarded
    /// wildcard.
    fn match_arm(&mut self, node: Node<'_>, kind: &str) {
        let source = self.source;
        if matches!(
            kind,
            "case_clause"
                | "switch_case"
                | "switch_rule"
                | "switch_block_statement_group"
                | "match_arm"
        ) {
            let arm = source[node.byte_range()].trim_start();
            let wildcard = (kind == "case_clause" && arm.starts_with("case _"))
                || (kind == "match_arm" && arm.starts_with('_'));
            let guarded = wildcard
                && (node.child_by_field_name("guard").is_some()
                    || if kind == "case_clause" {
                        arm.split_once(':')
                            .is_some_and(|(head, _)| head.contains(" if "))
                    } else {
                        arm.split_once("=>")
                            .is_some_and(|(head, _)| head.contains(" if "))
                    });
            if !arm.starts_with("default") && (!wildcard || guarded) {
                self.s.cyclomatic += 1;
            }
        }
    }

    /// Returns, throws, and labeled jumps.
    fn exit(&mut self, node: Node<'_>, kind: &str) {
        let (language, source) = (self.language, self.source);
        if matches!(kind, "return_statement" | "return_expression") {
            self.s.exits.returns += 1;
        }
        if matches!(kind, "throw_statement" | "raise_statement") {
            self.s.exits.throws += 1;
        }
        if matches!(
            kind,
            "break_expression" | "continue_expression" | "break_statement" | "continue_statement"
        ) {
            let labeled = if language == "rust" {
                source[node.byte_range()]
                    .trim_start_matches(|c: char| c.is_alphabetic() || c.is_whitespace())
                    .starts_with('\'')
            } else {
                matches!(language, "java" | "javascript" | "typescript")
                    && node.named_child_count() > 0
            };
            if labeled {
                self.s.cognitive += 1;
            }
        }
    }

    /// A leaf: a comment, or a token that is a line of code and, inside the body, a Halstead
    /// operator or operand.
    fn leaf(&mut self, node: Node<'_>) {
        let (language, source, d, newline_offsets) =
            (self.language, self.source, self.d, self.newline_offsets);
        let kind = node.kind();
        let lo = node.start_byte().max(d.range.start);
        let hi = node.end_byte().min(d.range.end);
        if lo >= hi {
            return;
        }
        let token = &source[lo..hi];
        let first_line = newline_offsets.partition_point(|&offset| offset < lo);
        let last_line = newline_offsets.partition_point(|&offset| offset < hi)
            - usize::from(source.as_bytes()[hi - 1] == b'\n');
        if kind == "comment" || kind.contains("comment") {
            for line in first_line..=last_line {
                self.comments.insert(line);
            }
            return;
        }
        if token.trim().is_empty() {
            return;
        }
        if matches!(kind, "string_start" | "string_end")
            || (matches!(token, "`" | "${")
                && node.parent().is_some_and(|parent| {
                    matches!(parent.kind(), "template_string" | "template_substitution")
                }))
        {
            self.code_lines.extend(first_line..=last_line);
            return;
        }
        for line in first_line..=last_line {
            self.code_lines.insert(line);
        }
        if token == "else"
            && language != "python"
            && node.parent().is_none_or(|parent| {
                parent.kind() != "else_clause"
                    && !parent
                        .named_children(&mut parent.walk())
                        .any(|child| matches!(child.kind(), "if_statement" | "if_expression"))
            })
        {
            self.s.cognitive += 1;
        }
        if matches!(token, "(" | ")" | "[" | "]" | "{" | "}" | "," | ";" | ":") {
            return;
        }
        let operand = matches!(
            kind,
            "string_fragment" | "template_chars" | "string_content" | "escape_sequence"
        ) || kind.contains("identifier")
            || kind.contains("literal")
            || kind.contains("string")
            || kind.contains("number")
            || kind.contains("integer")
            || kind.contains("float")
            || kind.contains("character")
            || matches!(kind, "true" | "false" | "null" | "True" | "False" | "None");
        if lo >= self.body.start && hi <= self.body.end {
            if operand {
                *self.operands.entry(token.to_owned()).or_default() += 1;
            } else {
                *self.operators.entry(token.to_owned()).or_default() += 1;
            }
        }
        if matches!(token, "&&" | "||") || (language == "python" && matches!(token, "and" | "or")) {
            self.boolean_operator(node, token);
        }
    }

    /// `&&`, `||`, `and`, `or`: each is one path; a run of the same operator is one increment
    /// of cognitive complexity.
    fn boolean_operator(&mut self, node: Node<'_>, token: &str) {
        let source = self.source;
        self.s.cyclomatic += 1;
        let parent = node.parent();
        let same_run = parent.is_some_and(|parent| {
            let text = &source[parent.byte_range()];
            let op = token;
            (parent.kind() == "binary_expression" || parent.kind() == "boolean_operator")
                && parent.parent().is_some_and(|ancestor| {
                    (ancestor.kind() == "binary_expression"
                        || ancestor.kind() == "boolean_operator")
                        && ancestor
                            .child_by_field_name("operator")
                            .is_some_and(|operator| &source[operator.byte_range()] == op)
                        && text.contains(op)
                })
        });
        if !same_run {
            self.s.cognitive += 1;
        }
    }

    /// Lines, Halstead figures and the maintainability index, from what was counted.
    fn finish(mut self) -> Result<Score> {
        let (source, d, newline_offsets) = (self.source, self.d, self.newline_offsets);
        if d.range.start < d.range.end {
            let lo = newline_offsets.partition_point(|&offset| offset < d.range.start);
            let mut hi = newline_offsets.partition_point(|&offset| offset < d.range.end);
            if source.as_bytes()[d.range.end - 1] == b'\n' {
                hi -= 1;
            }
            self.s.loc = hi - lo + 1;
        }
        self.s.sloc = self.code_lines.len();
        self.s.comment_lines = self.comments.len();
        let h = &mut self.s.halstead;
        h.n1 = self.operators.len();
        h.n2 = self.operands.len();
        h.total_operators = self.operators.values().sum();
        h.total_operands = self.operands.values().sum();
        h.vocabulary = h.n1 + h.n2;
        h.length = h.total_operators + h.total_operands;
        let volume = if h.length == 0 {
            0.0
        } else {
            h.length as f64 * (h.vocabulary.max(1) as f64).log2()
        };
        let difficulty = if h.n2 == 0 {
            0.0
        } else {
            (h.n1 as f64 / 2.0) * (h.total_operands as f64 / h.n2 as f64)
        };
        h.volume = round(volume)?;
        h.difficulty = round(difficulty)?;
        h.effort = round(difficulty * volume)?;
        h.estimated_bugs = round(volume / 3000.0)?;
        self.s.maintainability_index = round(
            ((171.0
                - 5.2 * volume.max(1.0).ln()
                - 0.23 * self.s.cyclomatic as f64
                - 16.2 * (self.s.loc.max(1) as f64).ln())
                * 100.0
                / 171.0)
                .clamp(0.0, 100.0),
        )?;
        Ok(self.s)
    }
}

struct CheckedWriter<'a, C> {
    bytes: Vec<u8>,
    cap: usize,
    check: &'a mut C,
    failed: Option<anyhow::Error>,
}
impl<C: FnMut() -> Result<()>> Write for CheckedWriter<'_, C> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Err(error) = (self.check)() {
            self.failed = Some(error);
            return Err(io::Error::other("cancelled serialization"));
        }
        if buf.len() > self.cap.saturating_sub(self.bytes.len()) {
            self.failed = Some(anyhow::anyhow!("response_too_large"));
            return Err(io::Error::other("response_too_large"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
/// Preflight the entire envelope before publishing any bytes to a transport.
pub fn serialize_checked<T: Serialize>(
    value: &T,
    cap: usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<u8>> {
    let mut writer = CheckedWriter {
        bytes: Vec::new(),
        cap,
        check,
        failed: None,
    };
    let result = serde_json::to_writer(&mut writer, value);
    if let Some(reason) = writer.failed {
        return Err(reason);
    }
    result?;
    (writer.check)()?;
    Ok(writer.bytes)
}
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
pub fn deadline_check(deadline: Instant, cancelled: bool) -> Result<()> {
    ensure!(!cancelled, "cancelled");
    ensure!(Instant::now() < deadline, "deadline_exceeded");
    Ok(())
}
pub fn fan_in() -> Value {
    json!({"value":null,"unavailable":FAN_IN_REASON})
}

/// Aggregate numeric leaves over declarations, not over already-aggregated files.
/// Empty groups retain unavailable fan-in and null numeric leaves.
/// Incremental per-declaration fold. Directory callers need not retain callable rows.
#[derive(Default)]
pub struct Accumulator {
    count: usize,
    leaves: BTreeMap<String, NumericFold>,
}
#[derive(Default, Clone)]
struct NumericFold {
    integer: bool,
    sum_integer: i128,
    max_integer: i128,
    sum_float: f64,
    max: f64,
}
impl Accumulator {
    pub fn count(&self) -> usize {
        self.count
    }
    pub fn push(&mut self, score: &Score) -> Result<()> {
        let value = serde_json::to_value(score)?;
        let mut next = self.leaves.clone();
        fold_numeric(&value, "", &mut next)?;
        self.count = self
            .count
            .checked_add(1)
            .context("bounded_work: declaration count overflow")?;
        self.leaves = next;
        Ok(())
    }
    pub fn project(&self, mode: &str, selected: &[&str]) -> Result<Value> {
        ensure!(matches!(mode, "sum" | "max" | "mean"), "invalid aggregate");
        ensure!(
            selected.iter().all(|name| METRIC_ORDER.contains(name)),
            "unknown metric"
        );
        let template = serde_json::to_value(empty())?;
        let mut result = serde_json::Map::new();
        for name in METRIC_ORDER {
            if !selected.is_empty() && !selected.contains(&name) {
                continue;
            }
            if name == "fanIn" {
                result.insert(name.into(), fan_in());
                continue;
            }
            result.insert(
                name.into(),
                project_leaf(
                    template.get(name).context("missing metric")?,
                    name,
                    &self.leaves,
                    self.count,
                    mode,
                )?,
            );
        }
        Ok(Value::Object(result))
    }
}
fn fold_numeric(
    value: &Value,
    path: &str,
    leaves: &mut BTreeMap<String, NumericFold>,
) -> Result<()> {
    if let Some(map) = value.as_object() {
        for (key, item) in map {
            if path.is_empty() && key == "fanIn" {
                continue;
            }
            let child = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}/{key}")
            };
            fold_numeric(item, &child, leaves)?;
        }
    } else if let Some(n) = value.as_number() {
        let fold = leaves
            .entry(path.to_owned())
            .or_insert_with(|| NumericFold {
                integer: n.is_u64() || n.is_i64(),
                max: f64::NEG_INFINITY,
                ..Default::default()
            });
        if fold.integer {
            let integer = n
                .as_i64()
                .map(i128::from)
                .or_else(|| n.as_u64().map(i128::from))
                .context("invalid integer metric")?;
            fold.sum_integer = fold
                .sum_integer
                .checked_add(integer)
                .context("bounded_work: aggregate overflow")?;
            fold.max_integer = fold.max_integer.max(integer);
        } else {
            fold.sum_float += n.as_f64().context("invalid metric")?;
        }
        fold.max = fold.max.max(n.as_f64().context("invalid metric")?);
        ensure!(
            fold.max.is_finite() && fold.sum_float.is_finite(),
            "bounded_work: nonfinite aggregate"
        );
    }
    Ok(())
}
fn project_leaf(
    template: &Value,
    path: &str,
    leaves: &BTreeMap<String, NumericFold>,
    count: usize,
    mode: &str,
) -> Result<Value> {
    if let Some(map) = template.as_object() {
        let mut output = serde_json::Map::new();
        for (key, value) in map {
            output.insert(
                key.clone(),
                project_leaf(value, &format!("{path}/{key}"), leaves, count, mode)?,
            );
        }
        return Ok(Value::Object(output));
    }
    if count == 0 {
        return Ok(Value::Null);
    }
    let fold = leaves.get(path).context("missing aggregate leaf")?;
    if fold.integer && mode == "sum" {
        return Ok(json!(
            i64::try_from(fold.sum_integer).context("bounded_work: aggregate overflow")?
        ));
    }
    if fold.integer && mode == "max" {
        return Ok(json!(
            i64::try_from(fold.max_integer).context("bounded_work: aggregate overflow")?
        ));
    }
    let total = if fold.integer {
        fold.sum_integer as f64
    } else {
        fold.sum_float
    };
    let result = if mode == "max" {
        fold.max
    } else if mode == "mean" {
        total / count as f64
    } else {
        total
    };
    Ok(json!(round(result)?))
}
/// Convenience fold for a single file. Use `Accumulator` to stream whole revisions.
pub fn aggregate(scores: &[Score], mode: &str, selected: &[&str]) -> Result<Value> {
    let mut accumulator = Accumulator::default();
    for score in scores {
        accumulator.push(score)?;
    }
    accumulator.project(mode, selected)
}
