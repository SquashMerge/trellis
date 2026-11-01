//! Config inputs a capture admits without reading their bytes (#581).
//!
//! Root config inputs (`package.json`, `package-lock.json`, `Cargo.lock` and the rest of
//! [`crate::capture::ROOT_INPUTS`]) and nested package and Cargo manifests are read for
//! dependency and module-root facts, not as source. So they have their own cap,
//! [`CONFIG_INPUT_MAX_BYTES`], separate from `--max-file-bytes` (owner ruling 2026-11-01,
//! option (a)). One over that cap doesn't fail the index: the capture keeps its stat and
//! stores [`PLACEHOLDER`] as its bytes, the rest of the checkout indexes, and status shows a
//! [`DIAGNOSTIC_CODE`] diagnostic for it.
//!
//! The placeholder is the stored meaning of a skipped input, as for a skipped source (#576):
//! its hash changes when an input starts or stops being skipped, so change detection and
//! the native revision need no special handling. It starts with U+0001, which JSON, TOML,
//! YAML and XML all reject. A manifest that doesn't parse supplies no dependency or
//! package-root facts. A skipped Cargo manifest also leaves every workspace document without
//! a Cargo target: its unread explicit target paths could cross package directories.
//! That makes the facts unknown,
//! never a false external and never a dependency proved absent (#562, #575, #580). A file
//! whose bytes equal the placeholder is treated as skipped.

/// The cap on one config input. Larger inputs are skipped, never read.
///
/// A config input's bytes are kept in the native revision (hex, in both its config and
/// dependency components) for SCIP producer worktrees, so each publication, including one
/// for a single edited source, costs 30 to 65 times the input's size in daemon peak memory
/// and about 8 times on disk (#581 measurements). At 16 MiB that is about 0.9 GiB peak and 2 s per edit;
/// the largest real lockfiles found are about 2.4 MB. 16 MiB is also the old ceiling,
/// `--max-file-bytes` at its maximum.
pub const CONFIG_INPUT_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// The bytes a capture stores for a config input over [`CONFIG_INPUT_MAX_BYTES`].
pub(crate) const PLACEHOLDER: &[u8] =
    b"\x01 Trellis did not read this file: it is larger than the config input cap.\n";

/// The status diagnostic code of a skipped config input.
pub const DIAGNOSTIC_CODE: &str = "config-input-oversized";

/// The status diagnostic message of a skipped config input.
pub(crate) fn diagnostic_message() -> String {
    format!(
        "config input is larger than the {} MiB config input cap and was not read; the dependency and package-root facts it would supply are unknown",
        CONFIG_INPUT_MAX_BYTES / (1024 * 1024)
    )
}

/// The content hash recorded for a skipped config input.
pub(crate) fn placeholder_hash() -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(PLACEHOLDER))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_parses_as_no_config_format() {
        assert!(serde_json::from_slice::<serde_json::Value>(PLACEHOLDER).is_err());
        let text = std::str::from_utf8(PLACEHOLDER).unwrap();
        assert!(toml::from_str::<toml::Value>(text).is_err());
        // YAML and XML both exclude C0 controls other than tab, LF and CR.
        assert_eq!(PLACEHOLDER[0], 0x01);
    }
}
