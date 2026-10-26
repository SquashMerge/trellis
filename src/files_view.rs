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
