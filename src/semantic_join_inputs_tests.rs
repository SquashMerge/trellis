//! Guards the build-time semantic join-inputs digest (Decision 0021 D4, owner ruling
//! 2026-11-09). The digest is part of every stored payload's key, so every module that can
//! change a settled payload row must be hashed. These tests fail when join code is reached
//! outside the list.
include!("../build/semantic_join_inputs.rs");

use std::{collections::BTreeSet, path::Path};

/// Crate items the listed join modules may use without being hashed, each with the reason it
/// cannot change a settled payload row for unchanged inputs.
const NON_OUTPUT_ITEMS: &[(&str, &str)] = &[
    (
        "store::Store",
        "the handle commits are issued through; it carries no row content",
    ),
    (
        "store::SemanticPublicationStage",
        "a test observer's stage names",
    ),
    (
        "store::CanonicalInventoryCursor",
        "a paging cursor over the native inventory; the rows it pages are native facts",
    ),
    (
        "store::NativeDocumentRows",
        "native document rows the join reads; native extraction has its own hash",
    ),
    (
        "store::AttestedProfileProducer",
        "an authenticated producer descriptor the join checks, never writes",
    ),
    (
        "store::SelectedRawBatch",
        "the stored raw batch the join reads; its bytes are covered by the batch descriptor hash",
    ),
    (
        "store::InventoryStream",
        "a reader of the stored native inventory",
    ),
    ("store::NativeReader", "a reader of stored native rows"),
    (
        "store::NativeHeader",
        "a reader of the stored native revision header",
    ),
    (
        "store::id_codec",
        "syntax and occurrence ID validity checks, which can only reject",
    ),
    (
        "store::StorageFull",
        "a typed failure; it can only abort a write",
    ),
    (
        "store::typed_storage_full",
        "a typed failure; it can only abort a write",
    ),
    (
        "store::topology",
        "root identity; a different root has different IDs",
    ),
    ("model::IndexPin", "an index pin identifier"),
    (
        "heap::bytes",
        "allocator measurement for budgets, which can only abort",
    ),
    (
        "indexer::native_js_kind",
        "classifies a syntax node as a native kind; the join reads native kinds back as stored \
         facts, and a change is a native extraction change (`TRELLIS_EXTRACTOR_HASH`)",
    ),
    (
        "model::CancelFlag",
        "cooperative cancellation; it can only abort",
    ),
    (
        "model::SemanticProjection",
        "the read-time view type, never stored",
    ),
    (
        "graph_query::BoundaryReason",
        "a field of the read-only query view's bindings, never stored",
    ),
    (
        "native_evidence",
        "native facts the join reads; native extraction has its own hash",
    ),
    (
        "capture",
        "the admitted inputs; source bytes enter facts only through content hashes",
    ),
    (
        "indexer::IndexOptions",
        "capture configuration; its effect is recorded per revision",
    ),
    (
        "index_coordinator",
        "scheduling and copy streams around the join; they move bytes and never shape rows",
    ),
    (
        "producer",
        "producer profiles gate context validity (pending or failed), never a row's content",
    ),
    ("test_alloc", "test-only allocation measurement"),
];

/// Rust source with comments, string and char literals blanked out, so
/// braces and paths inside them are not code.
fn code_only(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < bytes.len() {
        let rest = &source[i..];
        let after_ident = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if rest.starts_with("//") {
            let end = rest.find('\n').map_or(bytes.len(), |n| i + n);
            i = end;
        } else if rest.starts_with("/*") {
            let mut depth = 0;
            while i < bytes.len() {
                if source[i..].starts_with("/*") {
                    depth += 1;
                    i += 2;
                } else if source[i..].starts_with("*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if let Some(hashes) = raw_string_start(rest).filter(|_| !after_ident) {
            let open = 1 + rest[1..].find('"').unwrap() + 1;
            let close = format!("\"{}", "#".repeat(hashes));
            let end = rest[open..].find(&close).unwrap() + open + close.len();
            out.push_str("\"\"");
            i += end;
        } else if bytes[i] == b'"' || (rest.starts_with("b\"") && !after_ident) {
            i += if bytes[i] == b'b' { 2 } else { 1 };
            while bytes[i] != b'"' {
                i += if bytes[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
            out.push_str("\"\"");
        } else if bytes[i] == b'\'' {
            // A char literal closes within a few bytes; a lifetime does not.
            let literal = if rest[1..].starts_with('\\') {
                rest[2..].find('\'').map(|n| n + 3)
            } else {
                let c = rest[1..].chars().next().unwrap();
                rest[1 + c.len_utf8()..]
                    .starts_with('\'')
                    .then_some(2 + c.len_utf8())
            };
            match literal {
                Some(length) => {
                    out.push_str("' '");
                    i += length;
                }
                None => {
                    out.push('\'');
                    i += 1;
                }
            }
        } else {
            let c = rest.chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

fn raw_string_start(rest: &str) -> Option<usize> {
    let after = rest.strip_prefix("br").or_else(|| rest.strip_prefix('r'))?;
    let hashes = after.len() - after.trim_start_matches('#').len();
    after[hashes..].starts_with('"').then_some(hashes)
}

/// Drop `#[cfg(test)] mod name { ... }` blocks: test code is hashed with its
/// file but may use any module.
fn without_test_modules(code: &str) -> String {
    let mut out = String::new();
    let mut rest = code;
    while let Some(at) = rest.find("#[cfg(test)]") {
        let after = rest[at + "#[cfg(test)]".len()..].trim_start();
        let is_module = after.starts_with("mod ")
            && after
                .find('{')
                .is_some_and(|brace| !after[..brace].contains(';'));
        if !is_module {
            out.push_str(&rest[..at + 1]);
            rest = &rest[at + 1..];
            continue;
        }
        out.push_str(&rest[..at]);
        let start = rest.len() - after.len() + after.find('{').unwrap();
        let mut depth = 0;
        let mut end = start;
        for (offset, c) in rest[start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + offset + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn tokens(code: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = code.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if c.is_alphanumeric() || c == '_' {
            let mut end = at + c.len_utf8();
            while let Some(&(next, n)) = chars.peek() {
                if !(n.is_alphanumeric() || n == '_') {
                    break;
                }
                end = next + n.len_utf8();
                chars.next();
            }
            tokens.push(code[at..end].to_owned());
        } else if c == ':' && chars.peek().is_some_and(|&(_, n)| n == ':') {
            chars.next();
            tokens.push("::".into());
        } else if !c.is_whitespace() {
            tokens.push(c.to_string());
        }
    }
    tokens
}

/// Every `crate::...` path, with `use` trees expanded to their leaves.
fn crate_paths(code: &str) -> BTreeSet<String> {
    fn tree(tokens: &[String], at: &mut usize, prefix: &str, out: &mut BTreeSet<String>) {
        match tokens.get(*at).map(String::as_str) {
            Some("{") => {
                *at += 1;
                while tokens.get(*at).is_some_and(|t| t != "}") {
                    tree(tokens, at, prefix, out);
                    if tokens.get(*at).is_some_and(|t| t == ",") {
                        *at += 1;
                    }
                }
                *at += 1;
            }
            Some("*") => {
                *at += 1;
                out.insert(format!("{prefix}*"));
            }
            Some("self") => {
                *at += 1;
                out.insert(prefix.trim_end_matches("::").to_owned());
            }
            Some(ident) if ident.chars().all(|c| c.is_alphanumeric() || c == '_') => {
                let path = format!("{prefix}{ident}");
                *at += 1;
                if tokens.get(*at).is_some_and(|t| t == "::")
                    && tokens.get(*at + 1).is_some_and(|t| t != "<")
                {
                    *at += 1;
                    tree(tokens, at, &format!("{path}::"), out);
                } else {
                    if tokens.get(*at).is_some_and(|t| t == "as") {
                        *at += 2;
                    }
                    out.insert(path);
                }
            }
            _ => *at += 1,
        }
    }
    let tokens = tokens(code);
    let mut out = BTreeSet::new();
    for start in 0..tokens.len() {
        if tokens[start] == "crate" && tokens.get(start + 1).is_some_and(|t| t == "::") {
            let mut at = start + 2;
            tree(&tokens, &mut at, "", &mut out);
        }
    }
    out
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &str) -> Vec<u8> {
    std::fs::read(root().join(path)).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

fn hash_with(replace: Option<(&str, &[u8])>) -> String {
    let files: Vec<(String, Vec<u8>)> = join_modules(root())
        .into_iter()
        .map(|path| match replace {
            Some((replaced, bytes)) if replaced == path => (path, bytes.to_vec()),
            _ => {
                let bytes = read(&path);
                (path, bytes)
            }
        })
        .collect();
    let modules: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect();
    join_inputs_hash(&modules)
}

#[test]
fn the_built_digest_covers_exactly_the_listed_inputs() {
    assert_eq!(
        hash_with(None),
        env!("TRELLIS_SEMANTIC_JOIN_INPUTS_HASH"),
        "build.rs must hash exactly build/semantic_join_inputs.rs's modules"
    );
}

#[test]
fn a_listed_module_change_changes_the_digest_and_an_unlisted_one_does_not() {
    let base = hash_with(None);
    let mut edited = read("src/semantic_import.rs");
    edited.extend_from_slice(b"\n// any edit\n");
    assert_ne!(hash_with(Some(("src/semantic_import.rs", &edited))), base);
    let mut limits = read("src/semantic_batch/limits.rs");
    limits.extend_from_slice(b"\n// any edit\n");
    assert_ne!(
        hash_with(Some(("src/semantic_batch/limits.rs", &limits))),
        base
    );
    let files = join_modules(root());
    assert!(
        files
            .iter()
            .any(|f| f == "src/semantic_import/staged/settle.rs")
    );
    assert!(files.iter().any(|f| f == "src/store/native_first/copy.rs"));
    // The row writer's encoding is the schema, and test scaffolding writes nothing.
    for unlisted in [
        "src/store.rs",
        "src/store/native_first/tests.rs",
        "src/semantic_batch/staging_tests.rs",
        "src/semantic_batch/testing.rs",
    ] {
        assert!(!files.iter().any(|f| f == unlisted), "{unlisted}");
    }
}

/// Whether the crate path `used` (`a::b::item`) is inside a listed module file.
fn listed_use(files: &[String], used: &str) -> bool {
    let segments: Vec<&str> = used.split("::").collect();
    (1..=segments.len()).any(|n| {
        let module = segments[..n].join("/");
        files
            .iter()
            .any(|f| *f == format!("src/{module}.rs") || *f == format!("src/{module}/mod.rs"))
    })
}

/// Drops `(in crate::path)` visibility restrictions, which name a module without using it.
fn without_visibility_paths(code: &str) -> String {
    let mut out = String::new();
    let mut rest = code;
    while let Some(at) = rest.find("(in crate::") {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        rest = &rest[rest.find(')').map_or(rest.len(), |close| close + 1)..];
    }
    out.push_str(rest);
    out
}

/// Crate modules used by non-test code in the listed join modules that are neither listed nor
/// an allowed non-output item.
fn unlisted_uses(files: &[String], path: &str, source: &str) -> Vec<String> {
    crate_paths(&without_visibility_paths(&without_test_modules(
        &code_only(source),
    )))
    .into_iter()
    .filter(|used| {
        !listed_use(files, used)
            && !NON_OUTPUT_ITEMS
                .iter()
                .any(|(item, _)| used == item || used.starts_with(&format!("{item}::")))
    })
    .map(|used| format!("{path} uses crate::{used}"))
    .collect()
}

#[test]
fn join_modules_use_only_listed_modules_or_declared_non_output_items() {
    let files = join_modules(root());
    let mut unlisted = Vec::new();
    for path in &files {
        let source = String::from_utf8(read(path)).unwrap();
        unlisted.extend(unlisted_uses(&files, path, &source));
    }
    assert!(
        unlisted.is_empty(),
        "join code reaches modules outside build/semantic_join_inputs.rs; list them \
         (and justify the addition) or name a non-output item: {unlisted:#?}"
    );
}

#[test]
fn the_module_guard_catches_join_code_in_an_unlisted_module() {
    let files = join_modules(root());
    let source = "use crate::{semantic_evidence::Target, behavior_rust::walk as w};\n\
        fn f() { let s = \"crate::store::Store\"; crate::store::optional_hint_check(None); \
        crate::store::native_first::copy::copy_source(); crate::metrics::compute::<u8>(); }\n\
        #[cfg(test)]\nmod tests { use crate::store::Other; }\n";
    assert_eq!(
        unlisted_uses(&files, "src/semantic_import.rs", source),
        [
            "src/semantic_import.rs uses crate::behavior_rust::walk",
            "src/semantic_import.rs uses crate::metrics::compute",
            "src/semantic_import.rs uses crate::store::optional_hint_check",
        ]
    );
}
