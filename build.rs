//! Computes the native extractor's build-time producer hash (Decision 0005,
//! 2026-10 amendment) and exposes it as `TRELLIS_EXTRACTOR_HASH`. An ordinary
//! rebuild whose hash is unchanged keeps the selected head's validated facts;
//! any change to a listed module or crate forces full re-extraction.
include!("build/extractor_inputs.rs");

fn main() {
    build_identity();
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

/// Build metadata is independent of the extractor and never enters index identity.
fn build_identity() {
    use std::{env, process::Command};

    println!("cargo:rerun-if-env-changed=TRELLIS_BUILD_COMMIT");
    let commit = match env::var("TRELLIS_BUILD_COMMIT") {
        Ok(commit) => {
            assert!(
                !commit.trim().is_empty(),
                "TRELLIS_BUILD_COMMIT must not be empty"
            );
            commit
        }
        Err(env::VarError::NotPresent) => git_commit().unwrap_or_else(|| "unknown".into()),
        Err(error) => panic!("TRELLIS_BUILD_COMMIT: {error}"),
    };
    let mut features: Vec<_> = env::vars()
        .filter_map(|(key, _)| {
            key.strip_prefix("CARGO_FEATURE_")
                .map(|feature| feature.to_ascii_lowercase().replace('_', "-"))
        })
        .collect();
    features.sort();
    let rustc = Command::new(env::var_os("RUSTC").expect("Cargo provides RUSTC"))
        .arg("--version")
        .output()
        .expect("read rustc version");
    assert!(rustc.status.success(), "rustc --version failed");
    let toolchain = String::from_utf8(rustc.stdout).expect("rustc version is UTF-8");
    println!("cargo:rustc-env=TRELLIS_BUILD_COMMIT={commit}");
    println!(
        "cargo:rustc-env=TRELLIS_BUILD_TARGET={}",
        env::var("TARGET").expect("Cargo provides TARGET")
    );
    println!(
        "cargo:rustc-env=TRELLIS_BUILD_FEATURES={}",
        features.join(",")
    );
    println!(
        "cargo:rustc-env=TRELLIS_BUILD_TOOLCHAIN={}",
        toolchain.trim()
    );
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

fn git_commit() -> Option<String> {
    // HEAD is per-worktree; branch refs and packed-refs can live in the common
    // directory. Track both so branch switches, commits and ref packing refresh
    // the descriptor even when no source file changed.
    let mut paths = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    if let Some(reference) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        paths.push(reference);
    }
    for path in paths {
        if let Some(path) = git_output(&["rev-parse", "--git-path", &path]) {
            // An absent loose ref can appear after a commit on a packed branch.
            // Watching it even while absent avoids retaining the previous commit.
            println!("cargo:rerun-if-changed={path}");
        }
    }
    git_output(&["rev-parse", "--verify", "HEAD"])
}
