//! SCIP admission vocabulary that stored rows and the batch grammar use: the trust origin
//! names, the lowercase SHA-256 text check, the admission error constructor, the relative
//! path shape, the language order key and a root's `file://` URI. Admission itself is in
//! `scip_admission`.
use crate::scip_raw::ScipCaptureError;
use std::path::Path;

/// The trust origin of a pinned reference producer profile.
pub const REFERENCE_TRUST_ORIGIN: &str = "referenceProfile";
/// The trust origin of the synthetic test profile.
pub const SYNTHETIC_TRUST_ORIGIN: &str = "syntheticProfile";

/// Whether `s` is 64 lowercase hexadecimal digits.
pub(crate) fn hexhash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn admission(s: &str) -> ScipCaptureError {
    ScipCaptureError::InvalidAdmission(s.into())
}
pub(crate) fn path(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.contains(['\\', '\0'])
        && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}
pub(crate) fn order(language: &str) -> usize {
    crate::semantic_records::language_order(language).unwrap_or(usize::MAX)
}
pub(crate) fn file_uri(root: &Path) -> Result<String, ScipCaptureError> {
    let s = root
        .to_str()
        .filter(|s| s.starts_with('/'))
        .ok_or_else(|| admission("non-UTF8 physical root"))?;
    let mut out = String::from("file://");
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Ok(out)
}
