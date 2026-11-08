//! TypeScript grammar choice and syntax-category policy for the native extractor.
use tree_sitter::{Language, Node};

/// The TSX grammar for a `.tsx` path, otherwise the TypeScript grammar. The language is
/// always `typescript`; only the parser depends on the path.
pub(crate) fn grammar(path: &str) -> Language {
    if path.ends_with(".tsx") {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    }
}

/// A declaration file holds ambient declarations only (`.d.ts`, `.d.mts`, `.d.cts`). The
/// match is case-sensitive, like the extension match that admits the document.
/// Only tests read it until declaration-only extraction gating lands.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn declaration_only(path: &str) -> bool {
    path.ends_with(".d.ts") || path.ends_with(".d.mts") || path.ends_with(".d.cts")
}

/// TypeScript shares every JavaScript construct; TypeScript-only kinds are matched first.
pub(crate) fn native_ts_kind(n: Node<'_>) -> Option<&'static str> {
    crate::indexer::native_js_kind(n)
}
