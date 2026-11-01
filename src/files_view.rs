//! Pinned Files outline and same-spelling syntax evidence. No binding is inferred.
use crate::{
    model::{IndexPin, SourceFile, SourceRange, Symbol},
    native_evidence::Header,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub const RESPONSE_LIMIT: usize = 512 * 1024;
pub const FILE_INPUT_LIMIT: usize = 2 * 1024 * 1024;
pub const TOTAL_INPUT_LIMIT: usize = 16 * 1024 * 1024;
pub const FILE_TIME_LIMIT: Duration = Duration::from_millis(250);
pub const TOTAL_TIME_LIMIT: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct InvalidRequest;
impl std::fmt::Display for InvalidRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid Files request")
    }
}
impl std::error::Error for InvalidRequest {}

pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 8192
        && !path.contains(['\0', '\\', ':'])
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutlineItem {
    pub symbol: Symbol,
    pub native_kind: String,
    pub header: Header,
    /// A measured binding whose whole initializer is a function, from the cached file's
    /// syntax (#526). Absent when false or when the file could not be parsed in budget.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub callable_initializer: bool,
    /// The type a Rust `impl` block implements, by its last path segment, from the cached
    /// file's syntax (#526). Absent for other declarations or without a parse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impl_target: Option<String>,
}

/// Outline facts read from the parsed cached file, keyed by a declaration's native byte range.
#[derive(Debug, Default)]
pub(crate) struct OutlineSyntax {
    /// Bindings whose initializer is a function: a JavaScript `variable_declarator` whose value
    /// is a function or arrow expression, or a Python assignment whose right side is a `lambda`.
    pub callable_initializers: std::collections::BTreeSet<(usize, usize)>,
    /// Rust `impl` blocks and the last path segment of the type each implements.
    pub impl_targets: std::collections::BTreeMap<(usize, usize), String>,
}

/// `None` when the language has no such facts or the cached file cannot be parsed within the
/// Files budget; callers then list no binding as callable and leave impl blocks unnamed.
/// `impl_blocks` are the native `impl` declarations asked about. One measured inside a macro has
/// syntax only in that macro's expansion (#551), so it alone is looked up there, within the
/// same budget; running out leaves those blocks unnamed.
pub(crate) fn outline_syntax(
    file: &SourceFile,
    impl_blocks: &[(usize, usize)],
) -> Option<OutlineSyntax> {
    let language: tree_sitter::Language = match file.language.as_str() {
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        _ => return None,
    };
    if file.text.len() > FILE_INPUT_LIMIT {
        return None;
    }
    let started = Instant::now();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    let mut progress = |_: &tree_sitter::ParseState| {
        if started.elapsed() >= FILE_TIME_LIMIT {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let tree = parser.parse_with_options(
        &mut |offset, _| &file.text.as_bytes()[offset..],
        None,
        Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
    )?;
    let mut found = OutlineSyntax::default();
    let in_budget = || started.elapsed() < FILE_TIME_LIMIT;
    collect_outline(
        tree.root_node(),
        file.text.as_bytes(),
        &mut found,
        &in_budget,
    )?;
    if file.language == "rust" {
        let mut expansions = crate::native_rust_macros::Expansions::new(&file.text)
            .with_deadline(started + FILE_TIME_LIMIT);
        for &(start, end) in impl_blocks {
            if !in_budget() {
                break;
            }
            if found.impl_targets.contains_key(&(start, end)) {
                continue;
            }
            let block = expansions
                .root_for(tree.root_node(), start, end)
                .descendant_for_byte_range(start, end)
                .filter(|node| node.start_byte() == start && node.end_byte() == end);
            if let Some(name) = block.and_then(|node| impl_target(node, file.text.as_bytes())) {
                found.impl_targets.insert((start, end), name);
            }
        }
    }
    Some(found)
}

/// Add the outline facts under `root`. `None` once `in_budget` fails.
fn collect_outline(
    root: tree_sitter::Node<'_>,
    text: &[u8],
    found: &mut OutlineSyntax,
    in_budget: &impl Fn() -> bool,
) -> Option<()> {
    let mut cursor = root.walk();
    loop {
        if !in_budget() {
            return None;
        }
        let node = cursor.node();
        let range = (node.start_byte(), node.end_byte());
        let value = match node.kind() {
            "variable_declarator" => node.child_by_field_name("value"),
            "assignment" => node.child_by_field_name("right"),
            _ => None,
        };
        if value.is_some_and(|value| {
            matches!(
                value.kind(),
                "function_expression"
                    | "function"
                    | "arrow_function"
                    | "generator_function"
                    | "lambda"
            )
        }) {
            found.callable_initializers.insert(range);
        }
        if let Some(name) = impl_target(node, text) {
            found.impl_targets.insert(range, name);
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.node() == root {
                return Some(());
            }
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Some(());
            }
        }
    }
}
/// The type an `impl` block implements, by its last path segment.
fn impl_target(node: tree_sitter::Node<'_>, text: &[u8]) -> Option<String> {
    if node.kind() != "impl_item" {
        return None;
    }
    let name = type_name(node.child_by_field_name("type")?)?;
    Some(name.utf8_text(text).ok()?.to_owned())
}
/// The type one `impl` block implements (#558), from its header alone: an `impl` can be most
/// of a very large file. A header with a brace of its own does not parse, and names nothing.
pub(crate) fn impl_block_target(block: &str) -> Option<String> {
    let header = format!("{}{{}}", &block[..block.find('{')?]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(&header, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut cursor = root.walk();
    let item = root
        .named_children(&mut cursor)
        .find(|node| node.kind() == "impl_item")?;
    impl_target(item, header.as_bytes())
}
/// The identifier naming a Rust type: `Foo`, `a::Foo`, `Foo<T>`, `&Foo`, `&mut a::Foo<T>`.
fn type_name(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut node = node;
    for _ in 0..16 {
        node = match node.kind() {
            "type_identifier" => return Some(node),
            "generic_type" | "reference_type" => node.child_by_field_name("type")?,
            "scoped_type_identifier" => node.child_by_field_name("name")?,
            _ => return None,
        };
    }
    None
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outline {
    pub revision: IndexPin,
    pub path: String,
    pub items: Vec<OutlineItem>,
    pub next_offset: Option<usize>,
    pub truncated: bool,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Continuation {
    pub revision: IndexPin,
    pub symbol_id: String,
    pub path: String,
    pub offset: usize,
    pub partial: bool,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SyntaxMatchesRequest {
    pub expected_revision: IndexPin,
    pub symbol_id: String,
    #[serde(default = "match_limit")]
    pub limit: usize,
    pub continuation: Option<Continuation>,
}
fn match_limit() -> usize {
    50
}
impl SyntaxMatchesRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.symbol_id.is_empty()
                && self.symbol_id.len() <= 8192
                && !self.symbol_id.contains('\0')
                && (1..=200).contains(&self.limit),
            InvalidRequest
        );
        if let Some(c) = &self.continuation {
            ensure!(
                c.revision == self.expected_revision
                    && c.symbol_id == self.symbol_id
                    && valid_path(&c.path)
                    && c.offset <= FILE_INPUT_LIMIT,
                InvalidRequest
            );
        }
        Ok(())
    }
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchScope {
    pub source_set_id: String,
    pub language: String,
    pub spelling: Option<String>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyntaxMatch {
    pub path: String,
    pub source_hash: String,
    pub range: SourceRange,
    pub snippet: String,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyntaxMatches {
    pub revision: IndexPin,
    pub symbol_id: String,
    pub evidence_kind: &'static str,
    pub scope: MatchScope,
    pub items: Vec<SyntaxMatch>,
    pub continuation: Option<Continuation>,
    pub partial: bool,
    pub truncated: bool,
    pub warnings: Vec<String>,
}
impl SyntaxMatches {
    pub(crate) fn new(request: &SyntaxMatchesRequest, scope: MatchScope) -> Self {
        let partial = request.continuation.as_ref().is_some_and(|c| c.partial);
        let mut value = Self {
            revision: request.expected_revision, symbol_id: request.symbol_id.clone(),
            evidence_kind: "syntaxMatches", scope, items: vec![], continuation: None,
            partial, truncated: false,
            warnings: vec!["Same spelling, not compiler-resolved; homonyms, shadowing and unrelated properties or declarations may be included.".into()],
        };
        if partial {
            value.warn("Earlier pages skipped evidence; coverage remains incomplete.");
        }
        value
    }
    pub(crate) fn warn(&mut self, warning: &str) {
        self.partial = true;
        if !self.warnings.iter().any(|w| w == warning) {
            self.warnings.push(warning.into());
        }
    }
    pub(crate) fn resume(&mut self, path: &str, offset: usize) {
        self.truncated = true;
        self.continuation = Some(Continuation {
            revision: self.revision,
            symbol_id: self.symbol_id.clone(),
            path: path.into(),
            offset,
            partial: self.partial,
        });
    }
    pub(crate) fn bounded(&mut self) -> Result<()> {
        // Later files may require a larger cursor or add coverage warnings.
        // Roll back returned locations until the complete final envelope fits.
        // Resume at the earliest removed match, never after an unreturned one.
        while serde_json::to_vec(self)?.len() > RESPONSE_LIMIT {
            let Some(item) = self.items.pop() else {
                anyhow::bail!("response_too_large: Files response envelope");
            };
            self.resume(&item.path, item.range.start_byte);
        }
        Ok(())
    }
}

/// Scan one complete cached file with an interruptible parser and a constant-space cursor.
/// A result/page boundary resumes *at* the first unreturned token, never beyond it.
pub(crate) fn scan_file(
    result: &mut SyntaxMatches,
    request: &SyntaxMatchesRequest,
    file: &SourceFile,
    start: usize,
    excluded: Option<(usize, usize)>,
    total_started: Instant,
) -> Result<bool> {
    let language = match file.language.as_str() {
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        _ => {
            result.warn("Unsupported cached language skipped; coverage incomplete.");
            return Ok(false);
        }
    };
    let started = Instant::now();
    let expired =
        || started.elapsed() >= FILE_TIME_LIMIT || total_started.elapsed() >= TOTAL_TIME_LIMIT;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language)?;
    let mut progress = |_: &tree_sitter::ParseState| {
        if expired() {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let tree = parser.parse_with_options(
        &mut |offset, _| &file.text.as_bytes()[offset..],
        None,
        Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
    );
    let Some(tree) = tree else {
        result.warn("Cached file parse budget exceeded; file skipped and coverage incomplete.");
        return Ok(false);
    };
    if tree.root_node().has_error() {
        result.warn("Cached syntax contains parse errors; coverage incomplete.");
    }
    let mut cursor = tree.walk();
    loop {
        if expired() {
            result.warn("Cached file syntax walk budget exceeded; remaining file skipped and coverage incomplete.");
            return Ok(false);
        }
        let node = cursor.node();
        // Traverse interpolation expressions, but never literal text or comments.
        let prune = matches!(
            node.kind(),
            "comment"
                | "line_comment"
                | "block_comment"
                | "string_content"
                | "string_fragment"
                | "escape_sequence"
                | "regex_pattern"
        );
        if !prune
            && node.child_count() == 0
            && node.start_byte() >= start
            && matches!(
                node.kind(),
                "identifier"
                    | "type_identifier"
                    | "field_identifier"
                    | "shorthand_field_identifier"
                    | "property_identifier"
                    | "shorthand_property_identifier"
                    | "shorthand_property_identifier_pattern"
            )
            && excluded != Some((node.start_byte(), node.end_byte()))
            && file.text.get(node.byte_range()) == result.scope.spelling.as_deref()
        {
            if result.items.len() == request.limit {
                result.resume(&file.path, node.start_byte());
                return Ok(true);
            }
            let range = SourceRange {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                start_line: node.start_position().row + 1,
                start_column: node.start_position().column + 1,
                end_line: node.end_position().row + 1,
                end_column: node.end_position().column + 1,
            };
            result.items.push(SyntaxMatch {
                path: file.path.clone(),
                source_hash: file.hash.clone(),
                range,
                snippet: snippet(&file.text, node.start_byte(), node.end_byte()),
            });
            // Reserve the actual cursor envelope while admitting this item.
            result.resume(&file.path, node.end_byte());
            if serde_json::to_vec(result)?.len() > RESPONSE_LIMIT {
                result.items.pop();
                ensure!(
                    !result.items.is_empty(),
                    "response_too_large: Files match envelope"
                );
                result.resume(&file.path, node.start_byte());
                return Ok(true);
            }
            result.continuation = None;
            result.truncated = false;
        }
        if !prune && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(false);
            }
            if expired() {
                result.warn("Cached file syntax walk budget exceeded; remaining file skipped and coverage incomplete.");
                return Ok(false);
            }
        }
    }
}

fn snippet(text: &str, start: usize, end: usize) -> String {
    let line_start = text[..start].rfind('\n').map_or(0, |n| n + 1);
    let line_end = text[end..].find('\n').map_or(text.len(), |n| end + n);
    let mut from = line_start.max(start.saturating_sub(120));
    let mut to = line_end.min(start.saturating_add(240));
    while !text.is_char_boundary(from) {
        from += 1;
    }
    while !text.is_char_boundary(to) {
        to -= 1;
    }
    format!(
        "{}{}{}",
        if from > line_start { "…" } else { "" },
        &text[from..to],
        if to < line_end { "…" } else { "" }
    )
}

#[cfg(test)]
mod outline_syntax_tests {
    use super::*;

    fn outline_syntax_of(file: &SourceFile) -> Option<OutlineSyntax> {
        outline_syntax(file, &[])
    }

    fn syntax(language: &str, text: &str) -> Option<OutlineSyntax> {
        outline_syntax_of(&SourceFile {
            path: "fixture".into(),
            hash: String::new(),
            language: language.into(),
            text: text.into(),
        })
    }
    fn bindings(language: &str, text: &str) -> Vec<String> {
        syntax(language, text)
            .expect("parsed in budget")
            .callable_initializers
            .into_iter()
            .map(|(start, end)| text[start..end].split([' ', '=']).next().unwrap().into())
            .collect()
    }

    #[test]
    fn only_bindings_whose_whole_initializer_is_a_function_are_marked() {
        let javascript = "const anon = function () { return /[}]/.test(1); };\n\
             const arrow = (value = load()) => value;\n\
             const named = function inner() { return `}${x}{`; };\n\
             const result = function () { return 2; }();\n\
             const callback = wrap(() => 4);\n\
             const ratio = 4 / 2 / 1;\n";
        assert_eq!(
            bindings("javascript", javascript),
            ["anon", "arrow", "named"]
        );
        assert_eq!(
            bindings(
                "python",
                "f = lambda value: value\ng = wrap(lambda: 1)\nh = 3\n"
            ),
            ["f"]
        );
        assert!(syntax("java", "class A {}").is_none());
    }

    #[test]
    fn rust_impl_blocks_name_the_type_they_implement() {
        let text = "impl Budget { fn a() {} }\n\
             impl<T> fmt::Display for crate::other::Remote<T> { fn b() {} }\n\
             impl Trait for &mut Holder { fn c() {} }\n";
        let targets: Vec<_> = syntax("rust", text)
            .expect("parsed in budget")
            .impl_targets
            .into_values()
            .collect();
        assert_eq!(targets, ["Budget", "Remote", "Holder"]);
    }

    #[test]
    fn rust_impl_blocks_inside_measured_macros_name_their_type() {
        let text = include_str!("../tests/fixtures/rust/macro_callables.rs");
        let start = text.find("impl Meter").unwrap();
        let end = start + text[start..].find("\n        }\n").unwrap() + "\n        }".len();
        let file = SourceFile {
            path: "fixture.rs".into(),
            hash: String::new(),
            language: "rust".into(),
            text: text.into(),
        };
        let targets: Vec<_> = outline_syntax(&file, &[(start, end)])
            .expect("parsed in budget")
            .impl_targets
            .into_values()
            .collect();
        assert_eq!(targets, ["Meter"]);
    }
}

#[cfg(test)]
mod response_budget_tests {
    use super::*;

    #[test]
    fn final_cursor_and_warning_growth_roll_back_to_first_unreturned_match() {
        let pin = IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 1,
        };
        let request = SyntaxMatchesRequest {
            expected_revision: pin,
            symbol_id: "selected".into(),
            limit: 200,
            continuation: None,
        };
        let mut result = SyntaxMatches::new(
            &request,
            MatchScope {
                source_set_id: "source-set:v1:fixture".into(),
                language: "javascript".into(),
                spelling: Some("chosen".into()),
            },
        );
        let path = format!("a/{}.js", "x/".repeat(1100));
        // Long manifest paths and escaped clipped snippets consume the real
        // encoded response budget without large source or parser timing.
        for offset in 0..200 {
            result.items.push(SyntaxMatch {
                path: path.clone(),
                source_hash: "h".repeat(64),
                range: SourceRange {
                    start_byte: offset * 10,
                    end_byte: offset * 10 + 6,
                    start_line: 1,
                    start_column: offset * 10 + 1,
                    end_line: 1,
                    end_column: offset * 10 + 7,
                },
                snippet: "\u{1}".repeat(240),
            });
            if serde_json::to_vec(&result).unwrap().len() > RESPONSE_LIMIT - 500 {
                result.items.pop();
                break;
            }
        }
        assert!(result.items.len() > 1 && result.items.len() < 200);
        let previous_count = result.items.len();
        let last_start = result.items.last().unwrap().range.start_byte;
        let longer_next_path = format!("z/{}.js", "y/".repeat(3000));
        result.warn("Cached file syntax walk budget exceeded; remaining file skipped and coverage incomplete.");
        result.resume(&longer_next_path, 0);
        assert!(serde_json::to_vec(&result).unwrap().len() > RESPONSE_LIMIT);
        result.bounded().unwrap();
        assert!(serde_json::to_vec(&result).unwrap().len() <= RESPONSE_LIMIT);
        assert!(result.items.len() < previous_count);
        let cursor = result.continuation.as_ref().unwrap();
        assert_eq!(cursor.path, path);
        assert_eq!(cursor.offset, result.items.len() * 10);
        assert!(cursor.offset <= last_start);
        assert!(cursor.partial && result.partial && result.truncated);
    }
}
