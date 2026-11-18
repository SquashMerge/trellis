//! Admission helpers for pinned semantic producers: the witness language table and the
//! selected-role path check. The checked-in producer pin registry is in `crate::producer_pin`,
//! the root URI in `crate::scip_vocab` and the run language table in
//! `crate::producer_profiles`, all re-exported here.
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
pub use crate::producer_profiles::{STUB_PRODUCER, run_languages, stub_languages};
pub use crate::scip_vocab::{REFERENCE_TRUST_ORIGIN, SYNTHETIC_TRUST_ORIGIN};
pub(crate) use crate::scip_vocab::{admission, file_uri, order, path};

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
