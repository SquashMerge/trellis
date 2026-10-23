//! View-time, source-only complexity metrics. No index or revision identity is cached here.
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Write};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use tree_sitter::{Node, Parser};

use crate::native_evidence::{Call, ControlRegion, Declaration, Range};
use crate::store::SelectedIntegrity;

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

pub const VERSION: &str = "metrics:v1";
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
/// An error aborts the entire selected document, never returning partial scores.
pub fn compute(
    language: &str,
    source: &str,
    declarations: &[Declaration],
    calls: &[Call],
    regions: &[ControlRegion],
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<DeclarationMetrics>> {
    let source_only = compute_source(language, source, declarations, check)?;
    bind(source, declarations, calls, regions, source_only, check)
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
    uncached: Option<(MemoKey, Vec<Score>)>,
}

impl Computed {
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
    cached: Option<Vec<Score>>,
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
        scores.clone(),
        check,
    )?;
    check()?;
    Ok(Computed {
        rows,
        uncached: (!was_cached).then(|| (memo_key(&source), scores)),
    })
}

type MemoKey = (String, String, String);
fn memo_key(source: &SourceInput<'_>) -> MemoKey {
    (
        VERSION.to_owned(),
        source.language.to_owned(),
        source.content_hash.to_owned(),
    )
}
/// The selected pinned source; only its language, hash and bytes enter the memo key.
pub struct SourceInput<'a> {
    pub language: &'a str,
    pub content_hash: &'a str,
    pub text: &'a str,
}
fn bind(
    source: &str,
    declarations: &[Declaration],
    calls: &[Call],
    regions: &[ControlRegion],
    source_only: Vec<Score>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<DeclarationMetrics>> {
    let syntax = if calls.iter().any(|call| {
        declarations.iter().any(|d| {
            callable(&d.kind)
                && d.name.as_deref() == call.spelling.as_deref()
                && call.owner_syntax_id == d.syntax_id
        })
    }) {
        check()?;
        let language = declarations
            .first()
            .context(SelectedIntegrity(
                "corrupt: call without declaration".into(),
            ))?
            .document
            .language
            .as_str();
        let mut parser = Parser::new();
        parser.set_language(&grammar(language)?)?;
        let tree = parser
            .parse(source, None)
            .context("bounded_work: call syntax parse")?;
        check()?;
        Some(tree)
    } else {
        None
    };
    let mut rows = Vec::new();
    for (d, mut metrics) in declarations
        .iter()
        .filter(|d| callable(&d.kind))
        .zip(source_only)
    {
        check()?;
        metrics.fan_out = calls
            .iter()
            .filter(|c| c.owner_syntax_id == d.syntax_id)
            .count();
        // Exact unqualified self-call spelling is syntactic evidence, not call resolution.
        if let Some(name) = &d.name {
            metrics.cognitive += calls
                .iter()
                .filter(|c| {
                    c.owner_syntax_id == d.syntax_id
                        && c.spelling.as_deref() == Some(name.as_str())
                        && syntax.as_ref().is_some_and(|tree| {
                            unqualified_call(tree.root_node(), c, &d.document.language)
                        })
                })
                .count();
        }
        metrics.max_nesting = regions
            .iter()
            .filter(|r| r.owner_syntax_id == d.syntax_id)
            .map(|r| {
                let mut depth = 1;
                let mut parent = r.parent_id.as_deref();
                while let Some(id) = parent {
                    depth += 1;
                    parent = regions
                        .iter()
                        .find(|other| other.id == id && other.owner_syntax_id == d.syntax_id)
                        .and_then(|other| other.parent_id.as_deref());
                    if depth > regions.len() {
                        break;
                    }
                }
                depth
            })
            .max()
            .unwrap_or(0);
        metrics.parameters = d.header.parameters.len();
        rows.push(DeclarationMetrics {
            source_set_id: d.document.source_set_id.clone(),
            language: d.document.language.clone(),
            path: d.document.path.clone(),
            sid: d.syntax_id.clone(),
            range: d.range.clone(),
            metrics,
        });
    }
    Ok(rows)
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
        if node.start_byte() == call.range.start && node.end_byte() == call.range.end {
            let field = match language {
                "java" => "name",
                "javascript" if node.kind() == "new_expression" => "constructor",
                _ => "function",
            };
            let expression = node.child_by_field_name(field);
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
    pub source_set_id: String,
    pub language: String,
    pub path: String,
    pub sid: String,
    pub range: Range,
    pub metrics: Score,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Score {
    pub loc: usize,
    pub sloc: usize,
    pub comment_lines: usize,
    pub cyclomatic: usize,
    pub cognitive: usize,
    pub max_nesting: usize,
    pub halstead: Halstead,
    pub maintainability_index: f64,
    pub fan_out: usize,
    pub fan_in: Unavailable,
    pub parameters: usize,
    pub exits: Exits,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Unavailable {
    pub value: Option<f64>,
    pub unavailable: &'static str,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Exits {
    pub returns: usize,
    pub throws: usize,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Halstead {
    pub n1: usize,
    pub n2: usize,
    #[serde(rename = "N1")]
    pub total_operators: usize,
    #[serde(rename = "N2")]
    pub total_operands: usize,
    pub vocabulary: usize,
    pub length: usize,
    pub volume: f64,
    pub difficulty: f64,
    pub effort: f64,
    pub estimated_bugs: f64,
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

fn grammar(language: &str) -> Result<tree_sitter::Language> {
    Ok(match language {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        _ => return Err(SelectedIntegrity("corrupt: unknown stored language".into()).into()),
    })
}
fn syntax_ok(root: Node<'_>, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        check()?;
        ensure!(
            !node.is_error() && !node.is_missing(),
            "invalid_source_syntax"
        );
        let mut c = node.walk();
        stack.extend(node.children(&mut c));
    }
    Ok(())
}
fn compute_source(
    language: &str,
    source: &str,
    declarations: &[Declaration],
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<Score>> {
    check()?;
    let mut parser = Parser::new();
    parser.set_language(&grammar(language)?)?;
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
    check()?;
    let tree = tree.context("bounded_work: parser stopped")?;
    syntax_ok(tree.root_node(), check)?;
    let newline_offsets: Vec<usize> = source
        .bytes()
        .enumerate()
        .filter_map(|(i, b)| (b == b'\n').then_some(i))
        .collect();
    let mut result = Vec::new();
    for d in declarations.iter().filter(|d| callable(&d.kind)) {
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
        result.push(score(
            language,
            source,
            tree.root_node(),
            d,
            &nested,
            &newline_offsets,
            check,
        )?);
    }
    Ok(result)
}
// Native callable ranges are declaration ranges. Find the executable body within that
// declaration before counting Halstead leaves; the full range still supplies line counts.
fn callable_body(
    root: Node<'_>,
    source: &str,
    d: &Declaration,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Range> {
    let mut stack = vec![root];
    let mut selected = None;
    let mut visits = 0;
    while let Some(node) = stack.pop() {
        check()?;
        visits += 1;
        ensure!(visits <= 2_000_000, "bounded_work: body traversal");
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
            return Ok(Range {
                start: d.range.end,
                end: d.range.end,
            });
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
    selected.with_context(|| {
        SelectedIntegrity(format!(
            "corrupt: callable body not found: {} {} {:?} {:?}",
            d.document.language, d.document.path, d.name, d.range
        ))
    })
}

fn score(
    language: &str,
    source: &str,
    root: Node<'_>,
    d: &Declaration,
    nested: &[&Range],
    newline_offsets: &[usize],
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Score> {
    let body = callable_body(root, source, d, check)?;
    let mut s = empty();
    let mut code_lines = std::collections::BTreeSet::new();
    let mut comments = std::collections::BTreeSet::new();
    let mut operators = BTreeMap::<String, usize>::new();
    let mut operands = BTreeMap::<String, usize>::new();
    let mut stack = vec![(root, 0usize)];
    let mut visits = 0usize;
    while let Some((node, depth)) = stack.pop() {
        check()?;
        visits += 1;
        ensure!(
            visits <= 2_000_000 && depth <= 512,
            "bounded_work: syntax traversal"
        );
        if node.end_byte() <= d.range.start
            || node.start_byte() >= d.range.end
            || nested
                .iter()
                .any(|r| node.start_byte() >= r.start && node.end_byte() <= r.end)
        {
            continue;
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
            continue;
        }
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
            if node.start_byte() >= body.start && node.end_byte() <= body.end {
                *operands.entry(token.to_owned()).or_default() += 1;
            }
            let lo = node.start_byte().max(d.range.start);
            let hi = node.end_byte().min(d.range.end);
            if lo < hi {
                let first = newline_offsets.partition_point(|&offset| offset < lo);
                let last = newline_offsets.partition_point(|&offset| offset < hi)
                    - usize::from(source.as_bytes()[hi - 1] == b'\n');
                code_lines.extend(first..=last);
            }
            continue;
        }
        if node.is_named() {
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
            if structural {
                if !matches!(
                    kind,
                    "match_statement"
                        | "match_expression"
                        | "switch_expression"
                        | "switch_statement"
                ) {
                    s.cyclomatic += 1;
                }
                s.cognitive += 1 + depth;
            }
            if kind == "elif_clause" {
                s.cognitive += 1;
                s.cyclomatic += 1;
            } else if kind == "else_clause"
                && !node
                    .named_children(&mut node.walk())
                    .any(|child| matches!(child.kind(), "if_statement" | "if_expression"))
            {
                s.cognitive += 1;
            }
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
                    s.cyclomatic += 1;
                }
            }
            if matches!(kind, "return_statement" | "return_expression") {
                s.exits.returns += 1;
            }
            if matches!(kind, "throw_statement" | "raise_statement") {
                s.exits.throws += 1;
            }
            if matches!(
                kind,
                "break_expression"
                    | "continue_expression"
                    | "break_statement"
                    | "continue_statement"
            ) {
                let labeled = if language == "rust" {
                    source[node.byte_range()]
                        .trim_start_matches(|c: char| c.is_alphabetic() || c.is_whitespace())
                        .starts_with('\'')
                } else {
                    matches!(language, "java" | "javascript") && node.named_child_count() > 0
                };
                if labeled {
                    s.cognitive += 1;
                }
            }
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
                continue;
            }
        } else if node.child_count() > 0 {
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev().map(|child| (child, depth)));
            continue;
        }
        let lo = node.start_byte().max(d.range.start);
        let hi = node.end_byte().min(d.range.end);
        if lo >= hi {
            continue;
        }
        let token = &source[lo..hi];
        let first_line = newline_offsets.partition_point(|&offset| offset < lo);
        let last_line = newline_offsets.partition_point(|&offset| offset < hi)
            - usize::from(source.as_bytes()[hi - 1] == b'\n');
        if kind == "comment" || kind.contains("comment") {
            for line in first_line..=last_line {
                comments.insert(line);
            }
            continue;
        }
        if token.trim().is_empty() {
            continue;
        }
        if matches!(kind, "string_start" | "string_end")
            || (matches!(token, "`" | "${")
                && node.parent().is_some_and(|parent| {
                    matches!(parent.kind(), "template_string" | "template_substitution")
                }))
        {
            code_lines.extend(first_line..=last_line);
            continue;
        }
        for line in first_line..=last_line {
            code_lines.insert(line);
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
            s.cognitive += 1;
        }
        if matches!(token, "(" | ")" | "[" | "]" | "{" | "}" | "," | ";" | ":") {
            continue;
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
        if lo >= body.start && hi <= body.end {
            if operand {
                *operands.entry(token.to_owned()).or_default() += 1;
            } else {
                *operators.entry(token.to_owned()).or_default() += 1;
            }
        }
        if matches!(token, "&&" | "||") || (language == "python" && matches!(token, "and" | "or")) {
            s.cyclomatic += 1;
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
                s.cognitive += 1;
            }
        }
    }
    if d.range.start < d.range.end {
        let lo = newline_offsets.partition_point(|&offset| offset < d.range.start);
        let mut hi = newline_offsets.partition_point(|&offset| offset < d.range.end);
        if source.as_bytes()[d.range.end - 1] == b'\n' {
            hi -= 1;
        }
        s.loc = hi - lo + 1;
    }
    s.sloc = code_lines.len();
    s.comment_lines = comments.len();
    let h = &mut s.halstead;
    h.n1 = operators.len();
    h.n2 = operands.len();
    h.total_operators = operators.values().sum();
    h.total_operands = operands.values().sum();
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
    s.maintainability_index = round(
        ((171.0
            - 5.2 * volume.max(1.0).ln()
            - 0.23 * s.cyclomatic as f64
            - 16.2 * (s.loc.max(1) as f64).ln())
            * 100.0
            / 171.0)
            .clamp(0.0, 100.0),
    )?;
    Ok(s)
}

/// FIFO memo contains only complete source-derived scores, never native identity or calls.
type MemoEntry = ((String, String, String), Vec<Score>, usize);
pub struct MetricsMemo {
    entries: VecDeque<MemoEntry>,
    bytes: usize,
    limit: usize,
}
impl MetricsMemo {
    pub fn new(limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            limit,
        }
    }
    fn insert(&mut self, key: (String, String, String), value: Vec<Score>) -> Result<()> {
        let size = key
            .0
            .capacity()
            .saturating_add(key.1.capacity())
            .saturating_add(key.2.capacity())
            .saturating_add(
                value
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Score>()),
            )
            .saturating_add(std::mem::size_of::<(
                String,
                String,
                String,
                Vec<Score>,
                usize,
            )>());
        if size <= self.limit.min(64 * 1024 * 1024) {
            while self.bytes.saturating_add(size) > self.limit.min(64 * 1024 * 1024) {
                if let Some((_, _, n)) = self.entries.pop_front() {
                    self.bytes -= n;
                } else {
                    break;
                }
            }
            self.entries.push_back((key, value, size));
            self.bytes += size;
        }
        Ok(())
    }
    /// Source-only scores already recorded for this source, if any.
    pub(crate) fn lookup(&self, source: &SourceInput<'_>) -> Option<Vec<Score>> {
        let key = memo_key(source);
        self.entries
            .iter()
            .find(|(k, _, _)| *k == key)
            .map(|(_, scores, _)| scores.clone())
    }
    /// Record a document's newly computed scores, unless an equal source was
    /// recorded since they were computed.
    pub(crate) fn commit(&mut self, computed: Option<(MemoKey, Vec<Score>)>) -> Result<()> {
        match computed {
            Some((key, scores)) if !self.entries.iter().any(|(k, _, _)| *k == key) => {
                self.insert(key, scores)
            }
            _ => Ok(()),
        }
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
