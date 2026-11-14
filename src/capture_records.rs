//! The capture vocabulary that storage persists and compares: the stat and input
//! observation records, the root, nested-config and source-extension input tables with
//! their lookups, the extractor input key and the per-source read counters.
//!
//! This file references no layer. Admission, the filesystem walk and `Capture` itself stay
//! in `capture.rs`, which re-exports every name here at its old path.
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Root project inputs: dependency manifests, lockfiles and project config. Each is a
/// `config:` input and, in this order, a revision header dependency. New inputs are appended.
pub const ROOT_CONFIG_INPUTS: &[&str] = &[
    "package.json",
    "tsconfig.json",
    "jsconfig.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "Cargo.toml",
    "Cargo.lock",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "gradle.properties",
    "pyproject.toml",
    "requirements.txt",
    "uv.lock",
    "poetry.lock",
    "Pipfile",
    "Pipfile.lock",
    // pnpm workspace packages for structural resolution (#773).
    "pnpm-workspace.yaml",
    // scip-python project metadata and pyright config (#717).
    "setup.cfg",
    "pyrightconfig.json",
    // scip-java's SCIP build tool (#717).
    "lsif-java.json",
];

/// Root toolchain selectors: each is a `toolchain:` input.
pub const ROOT_TOOLCHAIN_INPUTS: &[&str] = &["rust-toolchain", "rust-toolchain.toml"];

/// Every root input: the config inputs, then the toolchain selectors.
pub fn root_inputs() -> impl Iterator<Item = &'static str> {
    ROOT_CONFIG_INPUTS
        .iter()
        .chain(ROOT_TOOLCHAIN_INPUTS)
        .copied()
}

/// Whether `relative` is a root input.
pub fn is_root_input(relative: &str) -> bool {
    root_inputs().any(|name| name == relative)
}

/// Whether `relative` is a root toolchain selector.
pub fn is_root_toolchain_input(relative: &str) -> bool {
    ROOT_TOOLCHAIN_INPUTS.contains(&relative)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CaptureStat {
    pub version: u8,
    pub kind: String,
    pub size: u64,
    pub device: Option<u64>,
    pub inode: Option<u64>,
    pub mtime_seconds: Option<i64>,
    pub mtime_nanoseconds: Option<i64>,
    pub ctime_seconds: Option<i64>,
    pub ctime_nanoseconds: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "state", rename_all = "camelCase")]
pub enum CaptureInputObservation {
    Absent,
    Present {
        stat: CaptureStat,
        hash: String,
    },
    Directory {
        stat: CaptureStat,
    },
    Root {
        stat: CaptureStat,
    },
    /// The executing native extractor's build-time producer hash
    /// (`EXTRACTOR_INPUT_KEY`), not a filesystem observation.
    Extractor {
        hash: String,
    },
}
/// The persisted input key that records the native extractor producer hash.
pub(crate) const EXTRACTOR_INPUT_KEY: &str = "extractor:trellis.native.syntax";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourceOperations {
    pub opens: usize,
    pub complete_reads: usize,
    pub hashes: usize,
}
impl SourceOperations {
    /// Read whole at most once and hashed once. A source over the size cap is never opened
    /// (#576); every other one is opened and read exactly once.
    pub fn read_once(&self) -> bool {
        self.opens <= 1 && self.complete_reads == self.opens && self.hashes == 1
    }
}

/// Every source extension Trellis indexes, with its language. `.d.ts` ends in `ts`, so a
/// declaration file is TypeScript. `.jsx` is not indexed.
pub(crate) const SOURCE_EXTENSIONS: &[(&str, &str)] = &[
    ("rs", "rust"),
    ("java", "java"),
    ("py", "python"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("ts", "typescript"),
    ("tsx", "typescript"),
    ("mts", "typescript"),
    ("cts", "typescript"),
];

/// The extensions of JavaScript and TypeScript sources: the sources scip-typescript names
/// relative to the nearest `package.json`.
pub(crate) const SCRIPT_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "ts", "tsx", "mts", "cts"];

/// The language of a source path by its (case-sensitive) extension, or `None` when Trellis
/// does not index it.
pub(crate) fn language_of_path(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?;
    SOURCE_EXTENSIONS
        .iter()
        .find_map(|(known, language)| (*known == extension).then_some(*language))
}

/// Nested config inputs: each file name is admitted in every directory strictly below the
/// root that holds a source with one of its extensions, directly or nested.
pub(crate) const NESTED_CONFIG_INPUTS: &[(&str, &[&str])] = &[
    // scip-typescript module markers (owner ruling 2026-10-29, #443).
    ("package.json", SCRIPT_EXTENSIONS),
    // Cargo target symbol namespaces (owner ruling 2026-10-30, #394).
    ("Cargo.toml", &["rs"]),
    // Python app detection (owner ruling 2026-11-08, #807).
    ("pyproject.toml", &["py"]),
    // TypeScript project references a producer plan follows (#717).
    ("tsconfig.json", SCRIPT_EXTENSIONS),
    // Maven and Gradle app detection (owner ruling 2026-11-10, #854).
    ("pom.xml", &["java"]),
    ("build.gradle", &["java"]),
    ("build.gradle.kts", &["java"]),
];

/// The nested config file names admitted above a source with `extension`.
pub(crate) fn nested_config_names(extension: &str) -> impl Iterator<Item = &'static str> + '_ {
    NESTED_CONFIG_INPUTS
        .iter()
        .filter(move |(_, extensions)| extensions.contains(&extension))
        .map(|(name, _)| *name)
}
