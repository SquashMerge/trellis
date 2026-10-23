//! Computes the native extractor's build-time producer hash (Decision 0005,
//! 2026-10 amendment) and exposes it as `TRELLIS_EXTRACTOR_HASH`. An ordinary
//! rebuild whose hash is unchanged keeps the selected head's validated facts;
//! any change to a listed module or crate forces full re-extraction.
include!("build/extractor_inputs.rs");

fn main() {
    let read = |path: &str| {
        println!("cargo:rerun-if-changed={path}");
        std::fs::read(path).unwrap_or_else(|error| panic!("read {path}: {error}"))
    };
    let test_support = std::env::var_os("CARGO_FEATURE_TEST_SUPPORT").is_some();
    let modules: Vec<(&str, Vec<u8>)> = extraction_modules(test_support)
        .map(|path| (path, read(path)))
        .collect();
    let modules: Vec<(&str, &[u8])> = modules
        .iter()
        .map(|(path, bytes)| (*path, bytes.as_slice()))
        .collect();
    let lock = String::from_utf8(read("Cargo.lock")).expect("Cargo.lock is UTF-8");
    println!("cargo:rerun-if-changed=build/extractor_inputs.rs");
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rustc-env=TRELLIS_EXTRACTOR_HASH={}",
        extractor_hash(&modules, &lock, test_support)
    );
}
