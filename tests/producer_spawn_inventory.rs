//! Static dependency-direction checks over production source (#21, AC1 and AC2).
//!
//! "Production code" is every `src/**/*.rs` file except `tests.rs`,
//! `*_tests.rs`, `src/testing.rs` and the stub binary, with `#[cfg(test)]` and
//! `#[test]` items removed and comments stripped. Checks key on file and
//! program or on path tokens, never on line numbers.
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn is_test_file(path: &Path) -> bool {
    let name = path.file_name().unwrap().to_str().unwrap();
    let relative = path
        .strip_prefix(src())
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    name == "tests.rs"
        || name.ends_with("_tests.rs")
        || relative == "testing.rs"
        || relative == "bin/trellis-producer-stub.rs"
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Braces and semicolons inside a kept string literal are swapped for private
/// characters so item matching cannot mistake them for code.
fn shield(c: char) -> char {
    match c {
        '{' => '\u{E000}',
        '}' => '\u{E001}',
        ';' => '\u{E002}',
        other => other,
    }
}

fn unshield(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{E000}' => '{',
            '\u{E001}' => '}',
            '\u{E002}' => ';',
            other => other,
        })
        .collect()
}

/// Drop comments. String literals are kept as written, or blanked to `""`.
fn lex(source: &str, keep_strings: bool) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let starts = |i: usize, text: &str| {
        text.chars()
            .enumerate()
            .all(|(k, c)| chars.get(i + k) == Some(&c))
    };
    while i < chars.len() {
        let after_word = i > 0 && is_word_char(chars[i - 1]);
        if starts(i, "//") {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if starts(i, "/*") {
            let mut depth = 0;
            while i < chars.len() {
                if starts(i, "/*") {
                    depth += 1;
                    i += 2;
                } else if starts(i, "*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if chars[i] == '\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
            }
        } else if !after_word && (chars[i] == 'r' || starts(i, "br")) && {
            let mut at = i + if chars[i] == 'b' { 2 } else { 1 };
            while chars.get(at) == Some(&'#') {
                at += 1;
            }
            chars.get(at) == Some(&'"')
        } {
            let mut at = i + if chars[i] == 'b' { 2 } else { 1 };
            let mut hashes = 0;
            while chars[at] == '#' {
                hashes += 1;
                at += 1;
            }
            let close: String = std::iter::once('"')
                .chain(std::iter::repeat_n('#', hashes))
                .collect();
            let body = at + 1;
            let mut end = body;
            while !starts(end, &close) {
                end += 1;
            }
            if keep_strings {
                out.extend(chars[i..end + close.len()].iter().map(|c| shield(*c)));
            } else {
                out.push_str("\"\"");
            }
            i = end + close.len();
        } else if chars[i] == '"' {
            let mut end = i + 1;
            while chars[end] != '"' {
                end += if chars[end] == '\\' { 2 } else { 1 };
            }
            if keep_strings {
                out.extend(chars[i..=end].iter().map(|c| shield(*c)));
            } else {
                out.push_str("\"\"");
            }
            i = end + 1;
        } else if chars[i] == '\'' {
            // A char literal closes within a few characters; a lifetime does not.
            let literal = if chars.get(i + 1) == Some(&'\\') {
                (i + 2..(i + 12).min(chars.len())).find(|&k| chars[k] == '\'')
            } else {
                (chars.get(i + 2) == Some(&'\'')).then_some(i + 2)
            };
            match literal {
                Some(end) => {
                    out.push_str("' '");
                    i = end + 1;
                }
                None => {
                    out.push('\'');
                    i += 1;
                }
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Remove items annotated `#[cfg(test)]` or `#[test]`. Strings must already be
/// blanked so a brace inside one cannot unbalance the match.
fn without_test_items(code: &str) -> String {
    let mut out = String::new();
    let mut rest = code;
    loop {
        let found = ["#[cfg(test)]", "#[test]"]
            .iter()
            .filter_map(|attribute| rest.find(attribute).map(|at| (at, attribute.len())))
            .min();
        let Some((at, length)) = found else { break };
        out.push_str(&rest[..at]);
        let mut after = &rest[at + length..];
        // The item ends at its matching brace, or at a `;` before any brace.
        let mut depth = 0_usize;
        let mut end = after.len();
        for (offset, c) in after.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = offset + 1;
                        break;
                    }
                }
                ';' if depth == 0 => {
                    end = offset + 1;
                    break;
                }
                _ => {}
            }
        }
        after = &after[end..];
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Production code of the files `select` accepts, as `(relative path, text)`.
/// `keep_strings` keeps string literal contents; otherwise they are blanked.
fn production(select: impl Fn(&str) -> bool, keep_strings: bool) -> Vec<(String, String)> {
    let mut files = Vec::new();
    rust_files(&src(), &mut files);
    let root = src().parent().unwrap().to_owned();
    files
        .into_iter()
        .filter(|path| !is_test_file(path))
        .filter_map(|path| {
            let relative = path
                .strip_prefix(&root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            select(&relative).then(|| {
                let source = fs::read_to_string(&path).unwrap();
                let text = unshield(&without_test_items(&lex(&source, keep_strings)));
                (relative, text)
            })
        })
        .collect()
}

/// Positions of `word` as a whole identifier.
fn whole_words(text: &str, word: &str) -> usize {
    text.match_indices(word)
        .filter(|(at, _)| {
            let before = text[..*at].chars().next_back();
            let after = text[at + word.len()..].chars().next();
            !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
        })
        .count()
}

/// Paths into the producer module: `crate::producer`, `trellis::producer` or
/// any `producer::` that is a whole word (not `semantic_producer::`).
fn producer_paths(text: &str) -> usize {
    text.match_indices("producer")
        .filter(|(at, _)| {
            let before = text[..*at].chars().next_back();
            let after = text[at + "producer".len()..].trim_start();
            !before.is_some_and(is_word_char) && after.starts_with("::")
        })
        .count()
}

fn string_literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('"') {
        let body = &rest[start + 1..];
        let mut end = 0;
        let bytes = body.as_bytes();
        while end < bytes.len() && bytes[end] != b'"' {
            end += if bytes[end] == b'\\' { 2 } else { 1 };
        }
        out.push(body[..end.min(body.len())].to_owned());
        rest = &body[(end + 1).min(body.len())..];
    }
    out
}

const OPERATIONS: [&str; 5] = [
    "producer_plan",
    "producer_run",
    "producer_cancel",
    "producer_status",
    "producer_pickup",
];

fn execution_references(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    if producer_paths(text) > 0 {
        found.push("a producer:: path".to_owned());
    }
    for word in [
        "spawn_producer",
        "start_run",
        "ProducerOwner",
        "PickupOwner",
    ] {
        if whole_words(text, word) > 0 {
            found.push(word.to_owned());
        }
    }
    for literal in string_literals(text) {
        if OPERATIONS.contains(&literal.as_str()) {
            found.push(format!("\"{literal}\""));
        }
    }
    found
}

#[test]
fn scanner_sees_what_it_must_and_ignores_what_it_must() {
    let source = r#"
        // crate::producer::exec in a comment
        use crate::producer::exec;
        fn run() { spawn_producer(); let op = "producer_run"; let brace = "}{;"; }
        /* start_run */
        fn semantic_producer_id() {}
        #[cfg(test)]
        mod tests { fn t() { let c = '{'; start_run(); } }
        #[test]
        fn single() { spawn_producer(); }
        fn tail() {}
    "#;
    let text = unshield(&without_test_items(&lex(source, true)));
    assert_eq!(
        whole_words(&text, "start_run"),
        0,
        "comments and test items drop"
    );
    assert_eq!(whole_words(&text, "spawn_producer"), 1);
    assert_eq!(producer_paths(&text), 1);
    assert!(text.contains("fn tail"));
    assert!(text.contains(r#""}{;""#), "string contents survive");
    assert_eq!(
        execution_references(&text),
        ["a producer:: path", "spawn_producer", "\"producer_run\""]
    );
    assert!(lex(source, false).contains("\"\""));
    assert_eq!(producer_paths("let semantic_producer::x; producer_id"), 0);
}

#[test]
fn production_scope_excludes_test_code() {
    let all = production(|_| true, false);
    let names: Vec<&str> = all.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.contains(&"src/main.rs"));
    assert!(names.contains(&"src/producer/exec.rs"));
    assert!(
        !names
            .iter()
            .any(|n| n.ends_with("_tests.rs") || n.ends_with("/tests.rs"))
    );
    assert!(!names.contains(&"src/testing.rs"));
    assert!(!names.contains(&"src/bin/trellis-producer-stub.rs"));
}

#[test]
fn spawn_sites_are_closed_inventory() {
    // (file, program expression) -> number of `Command::new(` sites.
    let expected: BTreeMap<(&str, &str), usize> = BTreeMap::from([
        (("src/daemon/registry.rs", "executable"), 1),
        (("src/git_status.rs", "&self.executable"), 1),
        (("src/main.rs", "&compiler"), 1),
        (("src/main.rs", "std::env::current_exe()?"), 1),
        (("src/producer/exec.rs", "&spec.argv[0]"), 1),
        (("src/store/topology.rs", "executable"), 1),
    ]);
    let mut actual: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (file, text) in production(|_| true, false) {
        for (at, _) in text.match_indices("Command::new(") {
            let rest = &text[at + "Command::new(".len()..];
            let mut depth = 1;
            let end = rest
                .char_indices()
                .find(|(_, c)| {
                    match c {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    depth == 0
                })
                .map(|(end, _)| end)
                .unwrap();
            let program: String = rest[..end].split_whitespace().collect();
            *actual.entry((file.clone(), program)).or_default() += 1;
        }
    }
    let expected: BTreeMap<(String, String), usize> = expected
        .into_iter()
        .map(|((file, program), n)| ((file.to_owned(), program.to_owned()), n))
        .collect();
    assert_eq!(
        actual, expected,
        "production spawn sites are a closed list; a new site needs an owner decision"
    );
}

#[test]
fn no_download_clients_in_src() {
    for (file, text) in production(|_| true, false) {
        for client in [
            "ureq",
            "isahc",
            "surf::",
            "curl::",
            "hyper_util::client",
            "hyper::client",
            "TcpStream::connect",
        ] {
            assert!(
                !text.contains(client),
                "{file} uses the HTTP/TCP client {client}"
            );
        }
        // reqwest is only a URL parser, in one place.
        for (at, _) in text.match_indices("reqwest") {
            let rest = &text[at..];
            assert!(
                file == "src/mcp/session.rs" && rest.starts_with("reqwest::Url::parse("),
                "{file} uses reqwest beyond URL parsing"
            );
        }
        for forbidden in [
            "npm install",
            "pip install",
            "cargo install",
            "rustup component",
        ] {
            assert!(
                !text.contains(forbidden),
                "{file} installs software: {forbidden}"
            );
        }
    }
}

#[test]
fn store_and_import_never_depend_on_producer_module() {
    let files = production(
        |f| {
            f == "src/store.rs"
                || f.starts_with("src/store/")
                || f == "src/semantic_batch.rs"
                || f.starts_with("src/semantic_batch/")
        },
        false,
    );
    assert!(files.len() > 5);
    for (file, text) in files {
        assert_eq!(
            producer_paths(&text),
            0,
            "{file} reaches into the producer module"
        );
    }
}

#[test]
fn mcp_http_watch_never_reference_producer() {
    let files = production(
        |f| {
            f.starts_with("src/mcp/")
                || f == "src/http.rs"
                || f.starts_with("src/http/")
                || f == "src/watch.rs"
        },
        true,
    );
    assert!(files.len() > 5);
    for (file, text) in files {
        assert_eq!(
            execution_references(&text),
            Vec::<String>::new(),
            "{file} references producer execution"
        );
    }
}
