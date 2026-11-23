//! Full-mode facts retained for later bounded charge replay; no clean replay runs here.
use super::*;
use shared::{ChargeSite, SemanticChargeOutcome};

#[derive(Clone, Serialize)]
pub(crate) struct SourceChargePlan {
    pub(crate) prepared_work: usize,
    pub(crate) sites: Vec<ChargeSite>,
}

#[derive(Default)]
pub(crate) struct SourceFacts {
    pub(crate) plan: Option<Arc<SourceChargePlan>>,
    pub(crate) work: usize,
    pub(crate) sites: usize,
    pub(crate) endpoints: Vec<u32>,
}

impl SourceFacts {
    pub(super) fn canonical(&self) -> Value {
        let Self {
            plan,
            work,
            sites,
            endpoints,
        } = self;
        json!({ "plan": plan.as_deref(), "work": work, "sites": sites, "endpoints": endpoints })
    }
}

impl ReferenceBase {
    pub(crate) fn rust_catalog_digest(&self) -> [u8; 32] {
        self.rust_catalog_digest
    }

    /// Every admitted document has a slot, including absent and scanned zero-site sources.
    pub(crate) fn source_facts(&self) -> &[SourceFacts] {
        &self.source_facts
    }
}

pub(super) fn normalize_document(read: ReadContext<'_, '_>, document: usize) -> Result<u32> {
    // An out-of-snapshot outcome remains unusable, rather than aliasing a real document.
    let sorted = read
        .snapshot
        .documents
        .get(document)
        .and_then(|doc| read.document_map.get(&doc.key))
        .copied()
        .unwrap_or(read.docs.len());
    Ok(u32::try_from(sorted)?)
}

impl SourceChargePlan {
    pub(super) fn push(&mut self, read: ReadContext<'_, '_>, mut site: ChargeSite) -> Result<()> {
        if let Some(shared::SemanticCharge {
            outcome: SemanticChargeOutcome::Internal { document, sid: _ },
            historical: _,
        }) = &mut site.semantic
        {
            *document = normalize_document(read, *document as usize)?;
        }
        self.sites.push(site);
        Ok(())
    }
}

pub(super) fn resolution_endpoints(endpoints: &mut BTreeSet<usize>, charged: &shared::ChargedSite) {
    for resolution in [&charged.structural, &charged.semantic]
        .into_iter()
        .flatten()
    {
        endpoints.extend(resolution.proof.iter().copied());
        endpoints.extend(resolution.targets.iter().filter_map(|target| match target {
            Target::Native(document) => Some(*document),
            Target::External(_) => None,
        }));
    }
}

pub(super) fn semantic_endpoints(
    read: ReadContext<'_, '_>,
    source: usize,
) -> Result<BTreeSet<usize>> {
    let mut endpoints = BTreeSet::new();
    let semantic = &read.docs[source].semantic;
    for document in &semantic.endpoint_documents {
        let document = normalize_document(read, *document)? as usize;
        if document < read.docs.len() {
            endpoints.insert(document);
        }
    }
    for anchor in &semantic.anchors {
        if let SemanticOutcome::Internal(document, _) = &anchor.outcome {
            let document = normalize_document(read, *document)? as usize;
            if document < read.docs.len() {
                endpoints.insert(document);
            }
        }
        metadata_endpoints(
            read,
            &read.docs[source].key.source_set_id,
            &anchor.metadata,
            &mut endpoints,
        );
    }
    for relationship in &semantic.relationships {
        endpoints.insert(source);
        if let Some((document, _)) = &relationship.target {
            let document = normalize_document(read, *document)? as usize;
            if document < read.docs.len() {
                endpoints.insert(document);
            }
        }
        metadata_endpoints(
            read,
            &read.docs[source].key.source_set_id,
            &relationship.metadata,
            &mut endpoints,
        );
    }
    Ok(endpoints)
}

fn metadata_endpoints(
    read: ReadContext<'_, '_>,
    source_set_id: &str,
    metadata: &SemanticMetadata,
    endpoints: &mut BTreeSet<usize>,
) {
    // Historical targets retain endpoints even when eligibility turns the outcome into
    // Boundary. External metadata, including coincident checkout paths, is not an endpoint.
    if let Some(targets) = metadata.get("historicalTargets")
        && let Some(targets) = targets.as_array()
    {
        for target in targets {
            if target["kind"] != "internal" {
                continue;
            }
            if let (Some(language), Some(path)) =
                (target["language"].as_str(), target["path"].as_str())
            {
                let key = DocumentKey {
                    source_set_id: source_set_id.into(),
                    language: language.into(),
                    path: path.into(),
                };
                if let Some(document) = read.document_map.get(&key) {
                    endpoints.insert(*document);
                }
            }
        }
    }
}
