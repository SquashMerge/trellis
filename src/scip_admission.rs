//! Admission helpers for pinned semantic producers: the run and witness language tables, the
//! selected-role path check and the root URI. The checked-in producer pin registry is in
//! `crate::producer_pin`, re-exported here.
pub use crate::scip_raw::{
    CapturedClaim, CapturedScipFactV1, CapturedScipInventory, Completion, Exclusion,
    ScipCaptureError,
};
use std::path::Path;

pub(crate) use crate::producer_pin::host_target;
pub use crate::producer_pin::{
    PinnedProducer, pinned_producer, pinned_producer_by_id, producer_languages,
    reference_install_method,
};
pub use crate::scip_vocab::{REFERENCE_TRUST_ORIGIN, SYNTHETIC_TRUST_ORIGIN};
pub(crate) use crate::scip_vocab::{admission, order, path};

/// The languages a `producer run` of `producer_id` covers: a real reference producer's, or
/// the test stub's (#664). `None` for an unknown producer.
pub fn run_languages(producer_id: &str) -> Option<&'static [&'static str]> {
    producer_languages(producer_id).or_else(|| (producer_id == STUB_PRODUCER).then(stub_languages))
}

/// The id of the test-support stand-in producer.
pub const STUB_PRODUCER: &str = "trellis-synthetic-scip";

/// The languages the test-support stub producer's runs cover: one language.
pub fn stub_languages() -> &'static [&'static str] {
    #[cfg(feature = "test-support")]
    if std::env::var("TRELLIS_TEST_PRODUCER_STUB_LANGUAGE").as_deref() == Ok("rust") {
        return &["rust"];
    }
    &["javascript"]
}

/// The languages of each producer the witness fast path accepts. `scip-java` is omitted on
/// purpose: its rejoin has always taken the FULL path, and that behavior is preserved.
pub fn witness_languages(producer_id: &str) -> Option<&'static [&'static str]> {
    match producer_id {
        "scip-java" => None,
        other => producer_languages(other),
    }
}

pub(crate) fn validate_selected_role(root: &Path, p: &Path) -> Result<String, ScipCaptureError> {
    if !p.is_absolute() {
        return Err(admission("selected role is not absolute"));
    }
    relative(root, p)
}
fn relative(root: &Path, p: &Path) -> Result<String, ScipCaptureError> {
    let r = p
        .strip_prefix(root)
        .map_err(|_| admission("role outside root"))?
        .to_str()
        .ok_or_else(|| admission("non-UTF8 role"))?;
    if !path(r) {
        return Err(admission("invalid role path"));
    }
    let mut current = root.to_owned();
    for component in r.split('/') {
        current.push(component);
        let metadata =
            std::fs::symlink_metadata(&current).map_err(|_| admission("selected role absent"))?;
        if metadata.file_type().is_symlink() {
            return Err(admission("symlinked selected role"));
        }
    }
    Ok(r.into())
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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn witness_languages_keep_the_fast_path_closure() {
        assert_eq!(
            witness_languages("scip-typescript"),
            Some(&["javascript", "typescript"][..])
        );
        assert_eq!(witness_languages("scip-python"), Some(&["python"][..]));
        assert_eq!(witness_languages("rust-analyzer"), Some(&["rust"][..]));
        assert_eq!(witness_languages("scip-java"), None);
        assert_eq!(witness_languages("trellis-synthetic-scip"), None);
    }
}
