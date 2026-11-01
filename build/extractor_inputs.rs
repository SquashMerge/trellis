// The native extractor's build-time producer hash inputs (Decision 0005,
// 2026-10 amendment). `build.rs` and the guard test in
// `src/extractor_inputs_tests.rs` both `include!` this file, so the hashed
// list and the checked list cannot diverge.

/// Every source file whose code can change a stored native fact, graph or
/// class projection, or their canonical bytes. The guard test fails if one of
/// these modules uses a crate module that is neither listed here nor named in
/// its explicit non-extraction allowlist.
///
/// - `native_evidence`: tree-sitter measurement, witnesses, revision headers.
/// - `native_ids`: canonical JSON, digests and ID minting.
/// - `indexer*`: graph projection and per-language native kinds.
/// - `classes`: class extraction and the class catalog.
/// - `model` (addition): the graph `Symbol`/`CallSite`/`ControlRegion` types
///   whose serde shape is the stored graph payload and projection hash input.
/// - `parallel` (addition): the ordered worker map whose ordering contract
///   keeps extraction output byte-identical to a sequential run.
pub const EXTRACTION_MODULES: &[&str] = &[
    "src/classes.rs",
    "src/indexer.rs",
    "src/indexer_java.rs",
    "src/indexer_python.rs",
    "src/indexer_rust.rs",
    "src/evidence_budget.rs",
    "src/model.rs",
    "src/native_evidence.rs",
    "src/native_ids.rs",
    "src/native_rust_macros.rs",
    "src/parallel.rs",
    "src/search_card.rs",
    "src/search_capture.rs",
    "src/skipped_source.rs",
];

/// Fixture conversion is compiled only for test-support captures. It must not
/// invalidate release indexes when the adapter changes.
const TEST_SUPPORT_EXTRACTION_MODULES: &[&str] = &["src/native_capture.rs"];

pub fn extraction_modules(test_support: bool) -> impl Iterator<Item = &'static str> {
    EXTRACTION_MODULES
        .iter()
        .chain(if test_support {
            TEST_SUPPORT_EXTRACTION_MODULES
        } else {
            &[]
        })
        .copied()
}

/// `Cargo.lock` packages whose code runs during extraction and reaches stored
/// facts: the tree-sitter runtime and grammars, Unicode normalization of
/// lookup keys, the JSON writer behind payloads and canonical bytes, and the
/// SCIP presentation decoder used by graph projection. An entry ending in `*`
/// is a name prefix.
pub const EXTRACTION_CRATES: &[&str] = &[
    "tree-sitter*",
    "icu_normalizer",
    "icu_normalizer_data",
    "serde_json",
    "protobuf",
    "scip",
];

/// The build-time producer hash: SHA-256 over a domain tag, each listed module
/// path and its exact bytes, the selected `Cargo.lock` package lines and the
/// `test-support` feature flag. `modules` must follow `extraction_modules(test_support)`.
pub fn extractor_hash(modules: &[(&str, &[u8])], cargo_lock: &str, test_support: bool) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"trellis.native-extractor-build.v1\0");
    for (path, bytes) in modules {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(bytes.len().to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(bytes);
        hasher.update(b"\0");
    }
    hasher.update(b"crates\0");
    for line in extraction_crate_lines(cargo_lock) {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(if test_support {
        b"test-support:1".as_slice()
    } else {
        b"test-support:0".as_slice()
    });
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `name version checksum` for each selected `Cargo.lock` package, sorted.
/// Panics if a listed crate is absent, so a renamed dependency cannot drop
/// out of the hash unnoticed.
pub fn extraction_crate_lines(cargo_lock: &str) -> Vec<String> {
    let selected = |name: &str| {
        EXTRACTION_CRATES.iter().any(|entry| match entry.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == *entry,
        })
    };
    let field = |block: &str, key: &str| -> String {
        block
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(" = \""))
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or("")
            .to_owned()
    };
    let mut lines = Vec::new();
    let mut found = Vec::new();
    for block in cargo_lock.split("[[package]]").skip(1) {
        let name = field(block, "name");
        if selected(&name) {
            lines.push(format!(
                "{name} {} {}",
                field(block, "version"),
                field(block, "checksum")
            ));
            found.push(name);
        }
    }
    for entry in EXTRACTION_CRATES {
        let present = match entry.strip_suffix('*') {
            Some(prefix) => found.iter().any(|name| name.starts_with(prefix)),
            None => found.iter().any(|name| name == entry),
        };
        assert!(present, "extraction crate {entry} is missing from Cargo.lock");
    }
    lines.sort();
    lines
}
