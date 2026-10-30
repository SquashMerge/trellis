//! Guards the build-time extractor producer hash (Decision 0005, 2026-10
//! amendment). An executable rebuild with an equal hash keeps the selected
//! head's facts, so every module that can change extraction output must be
//! hashed. These tests fail when extraction code is reached outside the list.
include!("../build/extractor_inputs.rs");

use std::{collections::BTreeSet, path::Path};

/// Crate modules the listed extraction modules may use without being hashed,
/// each limited to the exact items named, with the reason it cannot change a
/// stored fact for an unchanged capture.
const NON_EXTRACTION_ITEMS: &[(&str, &str)] = &[
    (
        "capture::Capture",
        "the admitted inputs; source bytes enter facts only through per-document content hashes",
    ),
    (
        "capture::ROOT_INPUTS",
        "revision-header config inputs, captured and compared per revision",
    ),
    (
        "capture::nested_package_manifest",
        "selects nested package.json config inputs (#443), captured and compared per revision",
    ),
    (
        "capture::CaptureInputObservation",
        "persisted change classification, not extraction",
    ),
    (
        "capture::EXTRACTOR_INPUT_KEY",
        "the key that records this hash",
    ),
    (
        "store::optional_hint_check",
        "an optional-hint budget check; it can only abort",
    ),
    (
        "store::topology::WorkspaceIdentity",
        "root identity; a different root has different IDs",
    ),
    (
        "watch::WatcherWitness",
        "a watcher report type in `model`, never stored as a fact",
    ),
];

/// Modules outside native extraction that parse source. They compute read-time
/// views or post-native semantic evidence, never native facts, graph or class projections.
const NON_NATIVE_SOURCE_PARSERS: &[(&str, &str)] = &[
    ("src/behavior_bindings.rs", "read-time sequence views"),
    ("src/behavior_java.rs", "read-time sequence views"),
    ("src/behavior_js.rs", "read-time sequence views"),
    ("src/behavior_python.rs", "read-time sequence views"),
    ("src/behavior_rust.rs", "read-time sequence views"),
    (
        "src/dependency_rust_symbols.rs",
        "dependency source browsing, outside the workspace index",
    ),
    ("src/metrics.rs", "read-time code metrics"),
    (
        "src/semantic_import.rs",
        "post-native semantic relationships from authenticated source",
    ),
    (
        "src/semantic_import/staged/group_size.rs",
        "post-native semantic normalization scratch accounting",
    ),
    (
        "src/semantic_import/join_store.rs",
        "post-native semantic join: hands one role-site syntax tree to type relationships",
    ),
    (
        "src/semantic_import/staged.rs",
        "post-native semantic join: keeps one role-site syntax tree for type relationships",
    ),
    (
        "src/semantic_import_complexity_tests.rs",
        "post-native semantic parser reservation tests",
    ),
    (
        "src/files_view.rs",
        "read-time Files outline and syntax matches",
    ),
    ("src/references.rs", "read-time structural references"),
    ("src/rules.rs", "read-time architecture rules"),
];

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &str) -> Vec<u8> {
    std::fs::read(root().join(path)).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

fn hash_with(replace: Option<(&str, &[u8])>, lock: &str) -> String {
    hash_with_mode(replace, lock, cfg!(feature = "test-support"))
}

fn hash_with_mode(replace: Option<(&str, &[u8])>, lock: &str, test_support: bool) -> String {
    let files: Vec<(&str, Vec<u8>)> = extraction_modules(test_support)
        .map(|path| match replace {
            Some((replaced, bytes)) if replaced == path => (path, bytes.to_vec()),
            _ => (path, read(path)),
        })
        .collect();
    let modules: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(path, bytes)| (*path, bytes.as_slice()))
        .collect();
    extractor_hash(&modules, lock, test_support)
}

fn cargo_lock() -> String {
    String::from_utf8(read("Cargo.lock")).unwrap()
}

#[test]
fn the_built_producer_hash_covers_exactly_the_listed_inputs() {
    assert_eq!(
        hash_with(None, &cargo_lock()),
        env!("TRELLIS_EXTRACTOR_HASH"),
        "build.rs must hash exactly build/extractor_inputs.rs's modules and crates"
    );
}

#[test]
fn a_listed_module_or_crate_change_changes_the_producer_hash() {
    let lock = cargo_lock();
    let base = hash_with(None, &lock);
    let mut edited = read("src/indexer_rust.rs");
    edited.extend_from_slice(b"\n// any edit\n");
    assert_ne!(
        hash_with(Some(("src/indexer_rust.rs", &edited)), &lock),
        base
    );
    let grammar = lock.replacen(
        "name = \"tree-sitter-java\"\nversion = \"",
        "name = \"tree-sitter-java\"\nversion = \"9",
        1,
    );
    assert_ne!(grammar, lock, "Cargo.lock lists tree-sitter-java");
    assert_ne!(hash_with(None, &grammar), base);
    // A crate outside the extraction list does not force re-extraction.
    let unrelated = lock.replacen(
        "name = \"axum\"\nversion = \"",
        "name = \"axum\"\nversion = \"9",
        1,
    );
    assert_ne!(unrelated, lock, "Cargo.lock lists axum");
    assert_eq!(hash_with(None, &unrelated), base);
}

#[test]
fn capture_adapter_affects_only_the_test_support_hash() {
    assert_eq!(
        extraction_modules(false).collect::<Vec<_>>(),
        EXTRACTION_MODULES
    );
    assert!(!extraction_modules(false).any(|path| path == "src/native_capture.rs"));
    assert!(extraction_modules(true).any(|path| path == "src/native_capture.rs"));
    let lock = cargo_lock();
    let mut adapter = read("src/native_capture.rs");
    adapter.extend_from_slice(b"\n// capture adapter edit\n");
    for test_support in [false, true] {
        let base = hash_with_mode(None, &lock, test_support);
        let edited = hash_with_mode(
            Some(("src/native_capture.rs", &adapter)),
            &lock,
            test_support,
        );
        assert_eq!(base == edited, !test_support);
    }
}

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

fn listed_module(name: &str) -> bool {
    extraction_modules(true).any(|path| path == format!("src/{name}.rs"))
}

/// Crate modules used by non-test code in the listed extraction modules that
/// are neither listed nor an allowed non-extraction item.
fn unlisted_uses(path: &str, source: &str) -> Vec<String> {
    crate_paths(&without_test_modules(&code_only(source)))
        .into_iter()
        .filter(|used| {
            let module = used.split("::").next().unwrap();
            !listed_module(module)
                && !NON_EXTRACTION_ITEMS
                    .iter()
                    .any(|(item, _)| used == item || used.starts_with(&format!("{item}::")))
        })
        .map(|used| format!("{path} uses crate::{used}"))
        .collect()
}

#[test]
fn extraction_modules_use_only_listed_modules_or_declared_non_extraction_items() {
    let mut unlisted = Vec::new();
    for path in extraction_modules(true) {
        let source = String::from_utf8(read(path)).unwrap();
        unlisted.extend(unlisted_uses(path, &source));
    }
    assert!(
        unlisted.is_empty(),
        "extraction code reaches modules outside build/extractor_inputs.rs; list them \
         (and justify the addition) or name a non-extraction item: {unlisted:#?}"
    );
}

#[test]
fn the_module_guard_catches_extraction_code_in_an_unlisted_module() {
    let source = "use crate::{model::Symbol, behavior_rust::walk as w};\n\
        fn f() { let s = \"crate::store::Store\"; crate::store::optional_hint_check(None); \
        crate::metrics::compute::<u8>(); }\n\
        #[cfg(test)]\nmod tests { use crate::store::Store; }\n";
    assert_eq!(
        unlisted_uses("src/indexer.rs", source),
        [
            "src/indexer.rs uses crate::behavior_rust::walk",
            "src/indexer.rs uses crate::metrics::compute",
        ]
    );
}

#[test]
fn source_parsers_outside_native_extraction_are_declared() {
    fn walk(dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let relative = path.strip_prefix(root()).unwrap();
                out.push(relative.to_str().unwrap().replace('\\', "/"));
            }
        }
    }
    let mut files = Vec::new();
    walk(&root().join("src"), &mut files);
    let mut undeclared = Vec::new();
    for path in files {
        let code = code_only(&String::from_utf8(read(&path)).unwrap());
        let parses = code.contains("tree_sitter") || code.contains("icu_normalizer");
        if parses
            && !extraction_modules(true).any(|listed| listed == path)
            && !NON_NATIVE_SOURCE_PARSERS
                .iter()
                .any(|(file, _)| *file == path)
        {
            undeclared.push(path);
        }
    }
    assert!(
        undeclared.is_empty(),
        "these modules parse source but are neither hashed extraction modules nor \
         declared non-native parsers: {undeclared:?}"
    );
}
