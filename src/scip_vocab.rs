//! SCIP admission vocabulary that stored rows record: the trust origin names and the
//! lowercase SHA-256 text check. Admission itself is in `scip_admission`.

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
