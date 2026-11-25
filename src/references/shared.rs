//! The declaration identity, candidate buckets and step charge the reference catalogs share.
use super::work::Work;
use super::{SiteResolution, Target, owner_step};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, serde::Serialize)]
pub(crate) enum StructuralCharge {
    None,
    OwnerAmbiguous,
    Resolve(super::RawSite),
}

#[derive(Clone, serde::Serialize)]
pub(crate) enum SemanticChargeOutcome {
    Boundary,
    External,
    Ambiguous,
    Unresolved,
    Internal { document: u32, sid: String },
}

#[derive(Clone, serde::Serialize)]
pub(crate) struct SemanticCharge {
    pub(super) outcome: SemanticChargeOutcome,
    pub(super) historical: bool,
}

#[derive(Clone, serde::Serialize)]
pub(crate) struct ChargeSite {
    pub(super) structural: StructuralCharge,
    pub(super) semantic: Option<SemanticCharge>,
    /// What the site's Rust resolution read in its source's syntax tree, so a later build
    /// can resolve it again without the tree (#1315). `None` when it read nothing there.
    pub(super) glob: Option<super::rust::GlobUse>,
}

impl SemanticCharge {
    pub(super) fn new(anchor: &super::SemanticAnchor) -> Result<Self> {
        use super::SemanticOutcome;
        let outcome = match &anchor.outcome {
            SemanticOutcome::Boundary => SemanticChargeOutcome::Boundary,
            SemanticOutcome::External => SemanticChargeOutcome::External,
            SemanticOutcome::Ambiguous => SemanticChargeOutcome::Ambiguous,
            SemanticOutcome::Unresolved => SemanticChargeOutcome::Unresolved,
            SemanticOutcome::Internal(document, sid) => SemanticChargeOutcome::Internal {
                document: u32::try_from(*document)?,
                sid: sid.clone(),
            },
        };
        Ok(Self {
            outcome,
            historical: anchor.metadata.contains_key("projection"),
        })
    }
}

pub(super) fn structural_charge(
    raw: Option<super::RawSite>,
    historical_anchor: bool,
    owner_ambiguous: bool,
) -> StructuralCharge {
    match raw {
        Some(_) if historical_anchor => StructuralCharge::None,
        None => StructuralCharge::None,
        Some(_) if owner_ambiguous => StructuralCharge::OwnerAmbiguous,
        Some(raw) => StructuralCharge::Resolve(raw),
    }
}

pub(super) struct ChargedSite {
    pub(super) structural: Option<SiteResolution>,
    pub(super) semantic: Option<SiteResolution>,
    pub(super) glob: Option<super::rust::GlobUse>,
}

impl ChargedSite {
    /// Both full output and later charge replay use the same semantic-only skips.
    pub(super) fn effective(self, source: usize) -> Option<(SiteResolution, bool)> {
        match (self.semantic, self.structural) {
            (Some(resolution), None) if resolution.targets.contains(&Target::Native(source)) => {
                None
            }
            (Some(resolution), _) => Some((resolution, true)),
            (None, Some(resolution)) => Some((resolution, false)),
            (None, None) => None,
        }
    }
}

pub(super) fn resolve_charge_site(
    read: super::ReadContext<'_, '_>,
    catalogs: &mut super::Catalogs,
    source: usize,
    site: &ChargeSite,
    packages: &mut super::SourcePackageDescriptors,
    locate: &impl Fn(usize, &str) -> Option<usize>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<ChargedSite> {
    let super::Catalogs {
        rust,
        rust_execution,
        packages: rust_packages,
        java,
        python,
        javascript,
        parsed,
    } = catalogs;
    // A clean source a kept catalog spares from parsing reads what it read before (#1315).
    let syntax = if parsed.as_ref().is_some_and(|parsed| !parsed[source]) {
        super::rust::SiteSyntax::Recorded(site.glob.as_ref())
    } else {
        super::rust::SiteSyntax::Tree(&read.trees[source])
    };
    let mut glob = None;
    let structural = match &site.structural {
        StructuralCharge::None => None,
        StructuralCharge::OwnerAmbiguous => {
            Some(SiteResolution::unknown("reference_owner_ambiguous"))
        }
        StructuralCharge::Resolve(raw) => Some(match read.docs[source].key.language.as_str() {
            "rust" => {
                let resolution = rust.resolver(rust_execution).resolve(
                    source,
                    raw,
                    rust_packages,
                    packages,
                    read.docs,
                    syntax,
                    check,
                )?;
                glob = rust_execution.take_glob();
                resolution
            }
            "java" => java.resolve(source, raw, packages, &mut rust_execution.work, check)?,
            "python" => python.resolve(source, raw, packages, &mut rust_execution.work, check)?,
            language if super::script_family(language) => javascript
                .as_ref()
                .expect("selected JavaScript catalog")
                .resolve(source, raw, packages, &mut rust_execution.work, check)?,
            _ => SiteResolution::unknown("module_mapping_unavailable"),
        }),
    };
    let semantic = site
        .semantic
        .as_ref()
        .and_then(|charge| semantic_charge_resolution(charge, structural.as_ref(), locate));
    Ok(ChargedSite {
        structural,
        semantic,
        glob,
    })
}

pub(super) fn semantic_charge_resolution(
    charge: &SemanticCharge,
    structural: Option<&SiteResolution>,
    locate: &impl Fn(usize, &str) -> Option<usize>,
) -> Option<SiteResolution> {
    use SemanticChargeOutcome::*;
    let historical = charge.historical;
    match &charge.outcome {
        Boundary => Some(SiteResolution::unknown("semantic_endpoint_catching_up")),
        External if historical => Some(SiteResolution::unknown("historical_external_claim")),
        Ambiguous | Unresolved if historical => {
            Some(SiteResolution::unknown("historical_resolution_unproven"))
        }
        Internal { document, sid } => locate(*document as usize, sid)
            .map(|target| SiteResolution {
                targets: BTreeSet::from([Target::Native(target)]),
                ..SiteResolution::default()
            })
            .or_else(|| {
                historical.then(|| SiteResolution::unknown("semantic_endpoint_catching_up"))
            }),
        External => structural
            .is_none_or(|resolution| resolution.targets.is_empty())
            .then(SiteResolution::default),
        Ambiguous | Unresolved => None,
    }
}

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
