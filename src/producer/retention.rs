//! Admission over the retained runs (#632, #811). Their persistence, the published-run
//! catalog the index writer attaches, and the #664 language policy over them are
//! `crate::store::producer_retention`.
use crate::store::requests::GeneratedImportOptions;
use anyhow::Result;

/// Whether a present retained descriptor still passes check 1 (size, digest, closed grammar,
/// format and decoder version). Its bytes never change, so a failure is permanent: a
/// descriptor written before a grammar change (#800 renamed `language`) can never be
/// admitted again and counts as a missing input (#811). I/O errors stay errors.
pub(crate) fn descriptor_admissible(
    options: &GeneratedImportOptions,
    limits: &crate::semantic_batch::BatchLimits,
) -> Result<bool> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(&options.descriptor)?
        .take(limits.descriptor_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok(
        crate::semantic_batch::descriptor::Descriptor::parse_admitted(
            &bytes,
            &options.descriptor_sha256,
            limits,
        )
        .is_ok(),
    )
}
