//! Sources a capture admits without reading their text (#576).
//!
//! A source over `--max-file-bytes`, or one that isn't UTF-8, doesn't fail the index. The
//! capture keeps it in the inventory, with a short placeholder as its text, and the rest of
//! the checkout indexes. Its document then publishes like one past the evidence budget: its file
//! module, `partial` coverage and a whole-file gap. It recovers when the file changes back.
//!
//! The placeholder is the stored meaning of a skipped file, so everything downstream reads it
//! from the text alone: the content hash changes when a file starts or stops being skipped, and
//! a document version can never be shared by a skipped file and a read one. A repository file
//! whose bytes equal a placeholder is indistinguishable from a skipped one and is treated the
//! same way, which loses its evidence but never invents any.
//!
//! A skipped file could declare anything, so it must never prove a competing binding absent
//! (#562, #575). Each placeholder is one comment and a character no grammar accepts, so every
//! reader treats the file as one whose parser error could hide anything: in Java its package is
//! unknown, since what follows the comment is neither an import nor a type. Rust module
//! ownership instead reads the names a broken file spells, and a placeholder spells none, so it
//! asks [`skipped`].

/// Why a source was admitted without its text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Skip {
    /// Larger than the capture's `--max-file-bytes`.
    Oversized,
    /// Not valid UTF-8.
    NotUtf8,
}

const LANGUAGES: [&str; 4] = ["java", "javascript", "python", "rust"];
const SKIPS: [Skip; 2] = [Skip::Oversized, Skip::NotUtf8];

impl Skip {
    /// The `partialFiles` reason.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Skip::Oversized => "oversized",
            Skip::NotUtf8 => "not_utf8",
        }
    }
    /// The coverage diagnostic of the document.
    pub(crate) fn diagnostic(self) -> &'static str {
        match self {
            Skip::Oversized => {
                "source is larger than --max-file-bytes and was not read; only the file module was kept"
            }
            Skip::NotUtf8 => "source is not UTF-8 and was not read; only the file module was kept",
        }
    }
}

/// The text a capture stores for a skipped source of `language`.
pub(crate) fn placeholder(language: &str, skip: Skip) -> String {
    let comment = if language == "python" { "#" } else { "//" };
    let why = match skip {
        Skip::Oversized => "it is larger than --max-file-bytes",
        Skip::NotUtf8 => "it is not UTF-8",
    };
    // U+0001 is a token in no grammar here, so the tree always has a parser error.
    format!("{comment} Trellis did not read this file: {why}.\n\u{1}\n")
}

/// Why the document with this stored text was skipped, if it was.
pub(crate) fn skipped(language: &str, text: &str) -> Option<Skip> {
    // Every placeholder is short and ends the same way, so ordinary sources fail fast.
    if text.len() > 128 || !text.ends_with("\u{1}\n") || !LANGUAGES.contains(&language) {
        return None;
    }
    SKIPS
        .into_iter()
        .find(|&skip| text == placeholder(language, skip))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(language: &str, text: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&match language {
                "java" => tree_sitter_java::LANGUAGE.into(),
                "rust" => tree_sitter_rust::LANGUAGE.into(),
                "python" => tree_sitter_python::LANGUAGE.into(),
                _ => tree_sitter_javascript::LANGUAGE.into(),
            })
            .unwrap();
        parser.parse(text, None).unwrap()
    }

    #[test]
    fn every_placeholder_is_a_parser_error_and_recognized() {
        for language in LANGUAGES {
            for skip in SKIPS {
                let text = placeholder(language, skip);
                assert_eq!(skipped(language, &text), Some(skip), "{language} {skip:?}");
                assert!(
                    parse(language, &text).root_node().has_error(),
                    "{language} {skip:?} placeholder must not parse"
                );
                // Its only other syntax is one comment, so nothing in it can be a fact.
                let tree = parse(language, &text);
                let mut cursor = tree.root_node().walk();
                let kinds: Vec<_> = tree
                    .root_node()
                    .named_children(&mut cursor)
                    .map(|node| node.kind())
                    .collect();
                assert!(
                    kinds
                        .iter()
                        .all(|kind| kind.contains("comment") || *kind == "ERROR"),
                    "{language}: {kinds:?}"
                );
            }
        }
        assert_ne!(
            placeholder("rust", Skip::Oversized),
            placeholder("rust", Skip::NotUtf8)
        );
    }

    #[test]
    fn ordinary_sources_are_not_skipped() {
        assert_eq!(skipped("rust", ""), None);
        assert_eq!(skipped("rust", "fn f() {}\n"), None);
        // A placeholder of another language is an ordinary file.
        assert_eq!(
            skipped("python", &placeholder("rust", Skip::Oversized)),
            None
        );
        assert_eq!(skipped("go", &placeholder("rust", Skip::Oversized)), None);
    }
}
