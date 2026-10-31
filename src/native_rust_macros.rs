//! Allowlisted Rust expression macros measured as expressions (Decision 0019, #519).
//!
//! The native walk otherwise treats a Rust macro token tree as opaque source. For the
//! allowlisted standard and `anyhow` macros below, it re-parses the token tree's own bytes as
//! a call argument list (or, for `vec!`, an array expression) and measures the arguments with
//! the ordinary rules. The re-parse reads only the token tree's bytes, at their exact offsets in
//! the original document, through tree-sitter included ranges. A fixed wrapper occupies bytes
//! `0..prefix.len()` and one byte after the token tree, so every measured node's byte range is
//! already a range of the original document. Anything that does not parse cleanly stays opaque.
use tree_sitter::{Node, Parser, Point, Range as TsRange, Tree};

/// Where an allowlisted macro's format string sits, if it takes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MacroForm {
    /// Comma-separated expressions; `format` is the argument index of the format string.
    Arguments { format: Option<usize> },
    /// `vec![a, b]` or `vec![x; n]`.
    Array,
}

const ARGUMENTS_PREFIX: &[u8] = b"fn a(){b";
const ARRAY_PREFIX: &[u8] = b"fn a(){";
const SUFFIX: &[u8] = b"}";

/// The allowlist. A bare name, or a `std`/`core`/`alloc` (`anyhow` for its macros) path.
pub(crate) fn expression_macro(n: Node<'_>, text: &str) -> Option<MacroForm> {
    let path = n.child_by_field_name("macro")?;
    if !matches!(path.kind(), "identifier" | "scoped_identifier") {
        return None;
    }
    let spelled = text.get(path.byte_range())?;
    let (prefix, name) = match spelled.rsplit_once("::") {
        Some((prefix, name)) => (Some(prefix), name),
        None => (None, spelled),
    };
    let std = prefix
        .is_none_or(|p| matches!(p, "std" | "core" | "alloc" | "::std" | "::core" | "::alloc"));
    let anyhow = prefix.is_none_or(|p| matches!(p, "anyhow" | "::anyhow"));
    let format = |index| Some(MacroForm::Arguments { format: index });
    match name {
        "vec" if std => Some(MacroForm::Array),
        "dbg" if std => format(None),
        "format" | "format_args" | "print" | "println" | "eprint" | "eprintln" | "panic"
        | "unreachable" | "todo"
            if std =>
        {
            format(Some(0))
        }
        "write" | "writeln" | "assert" | "debug_assert" if std => format(Some(1)),
        "assert_eq" | "assert_ne" | "debug_assert_eq" | "debug_assert_ne" if std => format(Some(2)),
        "ensure" if anyhow => format(Some(1)),
        "bail" | "anyhow" if anyhow => format(Some(0)),
        _ => None,
    }
}

/// The macro's delimited token tree.
pub(crate) fn token_tree(n: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = n.walk();
    n.named_children(&mut cursor)
        .filter(|child| child.kind() == "token_tree")
        .last()
}

/// Re-parse `token_tree` (original byte range `start..end`) as expressions. Returns `None`
/// when the bytes cannot be wrapped (too close to the document start) or the parser fails.
pub(crate) fn parse(
    parser: &mut Parser,
    text: &[u8],
    form: MacroForm,
    start: usize,
    end: usize,
) -> Option<Tree> {
    let (prefix, open, close): (&[u8], &[u8], &[u8]) = match form {
        MacroForm::Arguments { .. } => (ARGUMENTS_PREFIX, b"(", b")"),
        MacroForm::Array => (ARRAY_PREFIX, b"[", b"]"),
    };
    if end < start + 2 || start < prefix.len() || end > text.len() {
        return None;
    }
    let range = |start_byte: usize, end_byte: usize| TsRange {
        start_byte,
        end_byte,
        start_point: Point::new(0, start_byte),
        end_point: Point::new(0, end_byte),
    };
    parser
        .set_included_ranges(&[
            range(0, prefix.len()),
            range(start, end),
            range(end, end + SUFFIX.len()),
        ])
        .ok()?;
    let mut chunk = |offset: usize, _: Point| -> &[u8] {
        if offset < prefix.len() {
            &prefix[offset..]
        } else if offset == start {
            open
        } else if offset > start && offset + 1 < end {
            &text[offset..end - 1]
        } else if offset + 1 == end {
            close
        } else if offset >= end && offset < end + SUFFIX.len() {
            &SUFFIX[offset - end..]
        } else {
            &[]
        }
    };
    parser.parse_with_options(&mut chunk, None, None)
}

/// The re-parsed argument list or array, exactly at the token tree's range, from an
/// error-free tree. Any recovery anywhere keeps the macro opaque, with no partial guessing.
pub(crate) fn arguments(
    tree: &Tree,
    form: MacroForm,
    start: usize,
    end: usize,
) -> Option<Node<'_>> {
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let node = root.descendant_for_byte_range(start, end)?;
    let expected = match form {
        MacroForm::Arguments { .. } => "arguments",
        MacroForm::Array => "array_expression",
    };
    (node.kind() == expected && node.start_byte() == start && node.end_byte() == end)
        .then_some(node)
}

/// `(nodes, height)` of `n`'s subtree, or `None` once it holds more than `limit` nodes
/// (named or not). Iterative, so hostile nesting cannot exhaust the stack.
pub(crate) fn extent(n: Node<'_>, limit: usize) -> Option<(usize, usize)> {
    let mut cursor = n.walk();
    let (mut nodes, mut depth, mut height) = (1usize, 0usize, 0usize);
    loop {
        if cursor.goto_first_child() {
            depth += 1;
        } else {
            while !cursor.goto_next_sibling() {
                if depth == 0 || !cursor.goto_parent() {
                    return Some((nodes, height));
                }
                depth -= 1;
            }
        }
        if depth == 0 {
            return Some((nodes, height));
        }
        nodes += 1;
        height = height.max(depth);
        if nodes > limit {
            return None;
        }
    }
}

/// Whether a format-string literal names an argument: an implicit `{name}` capture, a
/// `{name}` named argument, or a `name$` width or precision. Such names stay opaque.
pub(crate) fn names_arguments(literal: &str, raw: bool) -> bool {
    // An escaped `{` (`\x7b`, `\u{7b}`) is a format brace after unescaping; stay opaque.
    let lower = literal.to_ascii_lowercase();
    if !raw
        && ["\\x7b", "\\u{7b}", "\\u{007b}"]
            .iter()
            .any(|e| lower.contains(e))
    {
        return true;
    }
    let bytes = literal.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // `\u{...}` is an escape, never a placeholder.
            b'\\' if !raw && bytes.get(i + 1) == Some(&b'u') && bytes.get(i + 2) == Some(&b'{') => {
                i = bytes[i..]
                    .iter()
                    .position(|&b| b == b'}')
                    .map_or(bytes.len(), |p| i + p + 1);
            }
            b'\\' if !raw => i += 2,
            b'{' if bytes.get(i + 1) == Some(&b'{') => i += 2,
            b'{' => {
                let close = bytes[i + 1..]
                    .iter()
                    .position(|&b| b == b'}')
                    .map_or(bytes.len(), |p| i + 1 + p);
                if placeholder_names_argument(&literal[i + 1..close]) {
                    return true;
                }
                i = close + 1;
            }
            _ => i += 1,
        }
    }
    false
}

fn identifier_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn placeholder_names_argument(placeholder: &str) -> bool {
    let (argument, spec) = placeholder.split_once(':').unwrap_or((placeholder, ""));
    if argument.trim().chars().next().is_some_and(identifier_start) {
        return true;
    }
    spec.match_indices('$').any(|(at, _)| {
        spec[..at]
            .chars()
            .rev()
            .take_while(|c| *c == '_' || c.is_alphanumeric())
            .last()
            .is_some_and(identifier_start)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_names_cover_captures_named_and_dollar_arguments_only() {
        for literal in [
            r#""{x}""#,
            r#""{ x }""#,
            r#""{x:?}""#,
            r#""{:>width$}""#,
            r#""{:.prec$}""#,
            r#""a {{}} {é}""#,
        ] {
            assert!(names_arguments(literal, false), "{literal}");
        }
        for literal in [
            r#""{}""#,
            r#""{0} {1:?}""#,
            r#""{:x} {:#08b} {:e}""#,
            r#""{{x}}""#,
            r#""{:1$}""#,
            r#""\u{a}""#,
            r#""plain""#,
        ] {
            assert!(!names_arguments(literal, false), "{literal}");
        }
        assert!(names_arguments(r#"r"\u{a}""#, true));
        assert!(names_arguments(r#""\x7bx}""#, false));
        assert!(names_arguments(r#""\u{7B}x}""#, false));
    }
}
