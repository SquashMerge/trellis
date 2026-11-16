//! The declaration identity, candidate buckets and step charge the reference catalogs share.
use super::work::Work;
use super::{SiteResolution, Target, owner_step};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct DeclarationId {
    pub(super) document: usize,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) sid: String,
}
#[derive(Clone)]
pub(super) struct Candidate {
    pub(super) declaration: DeclarationId,
    pub(super) proof: BTreeSet<usize>,
}
#[derive(Clone, Default)]
pub(super) struct Bucket {
    pub(super) candidates: BTreeMap<DeclarationId, Candidate>,
    pub(super) unknown: bool,
}
pub(super) fn unknown_bucket() -> Bucket {
    Bucket {
        unknown: true,
        ..Bucket::default()
    }
}
pub(super) fn charge(
    count: usize,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    for _ in 0..count {
        owner_step(work, check)?;
    }
    Ok(())
}
pub(super) fn add_bucket(
    result: &mut SiteResolution,
    bucket: &Bucket,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    for candidate in bucket.candidates.values() {
        charge(candidate.proof.len() + 1, work, check)?;
        result
            .targets
            .insert(Target::Native(candidate.declaration.document));
        result.proof.extend(candidate.proof.iter().copied());
    }
    Ok(())
}
