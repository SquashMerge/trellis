//! Batch descriptor admission that needs indexing policy: the producer context profile, the
//! generation-origin rule that applies it, and the path check against the batch directory.
//! The grammar, the producer pin check and the checks over bytes, limits and the captured
//! revision are in `crate::batch_descriptor`, re-exported here.
pub use crate::batch_descriptor::{
    ArtifactEntry, CoverageRow, Descriptor, DescriptorComponents, DescriptorMetadata,
    DescriptorProducer, DescriptorSourceSet, Disposition, ExclusionReason, FORMAT_VERSION,
    Generation, GenerationComponent, GenerationOrigin, GenerationRuntime, GenerationTrust,
    IdentityKind, ImportOrigin, Page, PageEntry, PageKind, RowOrder, SourceManifestHasher,
    SourceRow, Totals, basis_artifact_ordinal, check_page_bytes, parse_page,
};
use crate::{
    scip_admission::{admission, validate_selected_role},
    scip_raw::ScipCaptureError,
};
use std::path::Path;

type Result<T> = std::result::Result<T, ScipCaptureError>;

impl Descriptor {
    /// The admitted run's controlled context policy, or full captured context.
    /// Supplied batches, pickups and the synthetic stand-in cannot narrow input dependencies.
    pub fn context_profile(&self) -> Result<Option<crate::producer::profiles::Row>> {
        use crate::producer::{ProducerName, TrustProfile, profiles};
        let Some(generation) = self.generation.as_ref() else {
            return Ok(None);
        };
        if generation.origin == GenerationOrigin::OwnerPickup {
            if generation.trust_profile.is_some() {
                return Err(admission("generation trust profile"));
            }
            return Ok(None);
        }
        let trust = match generation.trust_profile {
            Some(GenerationTrust::Restricted) => TrustProfile::Restricted,
            Some(GenerationTrust::Trusted) => TrustProfile::Trusted,
            None => return Err(admission("generation trust profile")),
        };
        let pinned = self.check_producer()?;
        if pinned.name == profiles::STUB_NAME {
            return Ok(None);
        }
        let producer =
            ProducerName::parse(&pinned.name).ok_or_else(|| admission("producer profile pin"))?;
        profiles::row(profiles::Target::Real(producer), trust)
            .map(Some)
            .map_err(|_| admission("generation trust profile"))
    }

    /// Generation-origin rule: an external import carries a null `generation`; a generated one
    /// carries exactly the origin and ID of its job.
    pub fn check_origin(&self, origin: ImportOrigin, generation_id: Option<&str>) -> Result<()> {
        let matches = match (origin, &self.generation) {
            (ImportOrigin::External, None) => generation_id.is_none(),
            (ImportOrigin::TrellisRun, Some(generation)) => {
                generation.origin == GenerationOrigin::TrellisRun
                    && generation_id == Some(generation.generation_id.as_str())
            }
            (ImportOrigin::OwnerPickup, Some(generation)) => {
                generation.origin == GenerationOrigin::OwnerPickup
                    && generation_id == Some(generation.generation_id.as_str())
            }
            _ => false,
        };
        if !matches {
            return Err(admission("generation origin"));
        }
        self.context_profile()?;
        Ok(())
    }

    /// Check 6: every named file is under the root with no symlink component.
    pub fn check_paths(&self, root: &Path) -> Result<()> {
        let paths = self
            .pages
            .iter()
            .map(|page| page.relative_path.as_str())
            .chain(
                self.artifacts
                    .iter()
                    .map(|artifact| artifact.relative_path.as_str()),
            );
        for relative in paths {
            validate_selected_role(root, &root.join(relative))?;
        }
        Ok(())
    }
}
