// The semantic join's build-time inputs digest (Decision 0021 D4, owner ruling
// 2026-11-09). `build.rs` and the guard test in `src/semantic_join_inputs_tests.rs` both
// `include!` this file, so the hashed list and the checked list cannot diverge.

/// Every source file whose code can change the settled rows of a stored semantic payload.
/// An entry ending in `/**` is every non-test `.rs` file below that directory. The digest
/// is part of the payload key (`store/semantic_payload.rs`), so a build that changes join or
/// settle output without bumping the schema cannot keep serving payloads an older build wrote.
///
/// - `semantic_import*`: the join and its settlement, staged rows and provenance IDs.
/// - `semantic_batch*`: document inventory, staging, retained inputs and the production
///   resource limits (`limits.rs`), whose caps shape published reasons.
/// - `semantic_evidence`: the evidence rows' types, canonical forms and validation.
/// - `scip_raw*`: the decoded claims, document caps and module roots the join reads.
/// - `scip_admission`, `scip_vocab`: the trust origins and hash helpers the stored basis
///   records.
/// - `store/native_first*`: staged commit, copies, witnessed settlement and context.
/// - `store/semantic_payload`: the key, dependency set and projection.
/// - `store/captured_witness`: the captured-version reads the witnessed settlement trusts.
/// - `native_ids`: canonical JSON, digests and the rejoined ID derivation.
/// - `skipped_source`, `skipped_config`: placeholder hashes that enter the settled basis.
/// - `indexer_ts`, `native_rust_macros`: the parsers the join uses on source it binds.
///
/// Not listed: `store.rs` (the row writer; its encoding is the schema, which a change
/// bumps, and its size would rotate every payload on unrelated edits), query and read-time
/// modules, and `Cargo.lock` (a decoder change is `DECODER_VERSION`, which blocks batch
/// selection).
pub const JOIN_MODULES: &[&str] = &[
    "src/indexer_ts.rs",
    "src/native_ids.rs",
    "src/native_rust_macros.rs",
    "src/scip_admission.rs",
    "src/scip_raw.rs",
    "src/scip_raw/**",
    "src/scip_vocab.rs",
    "src/semantic_batch.rs",
    "src/semantic_batch/**",
    "src/semantic_evidence.rs",
    "src/semantic_import.rs",
    "src/semantic_import/**",
    "src/skipped_config.rs",
    "src/skipped_source.rs",
    "src/store/captured_witness.rs",
    "src/store/native_first.rs",
    "src/store/native_first/**",
    "src/store/semantic_payload.rs",
];

/// Test scaffolding is compiled only for tests and never writes a stored payload.
fn is_test_file(name: &str) -> bool {
    name == "tests.rs" || name == "testing.rs" || name.ends_with("_tests.rs")
}

/// The files `JOIN_MODULES` names under `root` (the crate directory), as sorted
/// `/`-separated relative paths.
pub fn join_modules(root: &std::path::Path) -> Vec<String> {
    fn walk(root: &std::path::Path, dir: &str, out: &mut Vec<String>) {
        let mut entries: Vec<_> = std::fs::read_dir(root.join(dir))
            .unwrap_or_else(|error| panic!("read {dir}: {error}"))
            .map(|entry| entry.unwrap_or_else(|error| panic!("read {dir}: {error}")))
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = format!("{dir}/{name}");
            if entry.path().is_dir() {
                walk(root, &path, out);
            } else if name.ends_with(".rs") && !is_test_file(&name) {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    for entry in JOIN_MODULES {
        match entry.strip_suffix("/**") {
            Some(dir) => walk(root, dir, &mut files),
            None => files.push((*entry).to_owned()),
        }
    }
    files.sort();
    files.dedup();
    files
}

/// The build-time join-inputs digest: SHA-256 over a domain tag and each listed file's path
/// and exact bytes. `modules` must follow `join_modules`.
pub fn join_inputs_hash(modules: &[(&str, &[u8])]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"trellis.semantic-join-build.v1\0");
    for (path, bytes) in modules {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(bytes.len().to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(bytes);
        hasher.update(b"\0");
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
