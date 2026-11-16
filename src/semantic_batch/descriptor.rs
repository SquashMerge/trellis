//! Batch descriptor admission that needs indexing policy: the generation-origin rule that
//! applies the producer context profile, and the path check against the batch directory.
//! The grammar, the producer pin check, the context profile and the checks over bytes, limits
//! and the captured revision are in `crate::batch_descriptor`, re-exported here.
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
