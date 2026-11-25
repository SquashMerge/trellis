//! The canonical form of a [`ReferenceBase`] (#1301): every field, so two results compare
//! equal only when every read of them answers the same.
use super::{
    EdgeSites, MAX_SAMPLE_LIMIT, ReferenceBase, Site, SiteCoverage, SourceReferences, Target,
};
use anyhow::Result;
use serde_json::{Value, json};

impl ReferenceBase {
    /// Every field, plus the selected JSON of every source at the largest sample limit.
    pub(crate) fn canonical(&self) -> Result<Value> {
        // Exhaustive, so a new field fails to compile until it is compared.
        let Self {
            pin,
            partial_files,
            identities,
            modules,
            sources,
            external_packages,
            rust_catalog_digest: _,
            python_catalog_digest: _,
            javascript_catalog_digest: _,
            java_catalog_digest: _,
            source_facts: _,
            producers,
            document_status,
            sample_limit,
            work_used,
        } = self;
        let selected = self.select_where(
            &[],
            None,
            None,
            &[],
            MAX_SAMPLE_LIMIT.min(*sample_limit),
            &mut || Ok(()),
        )?;
        let coverage: Vec<Value> = selected
            .coverage
            .iter()
            .map(|((path, test), row)| json!([path, test, coverage_row(row)]))
            .collect();
        Ok(json!({
            "pin": pin,
            "partialFiles": partial_files,
            "identities": identities,
            "modules": modules,
            "sources": sources
                .iter()
                .enumerate()
                .map(|(index, source)| source.as_deref().map(|source| source.canonical(&external_packages.by_source[index])))
                .collect::<Vec<_>>(),
            "externalPackages": external_packages.canonical(),
            "rustCatalogDigest": hex::encode(self.rust_catalog_digest()),
            "pythonCatalogDigest": hex::encode(self.python_catalog_digest()),
            "javascriptCatalogDigest": hex::encode(self.javascript_catalog_digest()),
            "javaCatalogDigest": hex::encode(self.java_catalog_digest()),
            "sourceFacts": self.source_facts().iter().map(super::SourceFacts::canonical).collect::<Vec<_>>(),
            "producers": producers,
            "documentStatus": document_status,
            "sampleLimit": sample_limit,
            "workUsed": work_used,
            "selected": {
                "data": selected.data,
                "modules": selected.modules,
                "coverage": coverage,
            },
        }))
    }
}

impl SourceReferences {
    fn canonical(&self, descriptors: &super::SourcePackageDescriptors) -> Value {
        let Self {
            projection,
            reading,
            edges,
            unresolved,
            proof,
            packages,
            coverage,
            mapped,
            semantic,
        } = self;
        let edges: Vec<Value> = edges
            .iter()
            .map(|((target, test, gates), sites)| {
                let EdgeSites {
                    count,
                    semantic,
                    kinds,
                    calls,
                    others,
                } = sites;
                json!({
                    "target": match target {
                        Target::Native(index) => json!({"native": index}),
                        Target::External(identity) => json!({"external": identity}),
                    },
                    "test": test,
                    "gates": gates,
                    "count": count,
                    "semantic": semantic,
                    "kinds": kinds,
                    "calls": calls.iter().map(Site::canonical).collect::<Vec<_>>(),
                    "others": others.iter().map(Site::canonical).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({
            "projection": projection,
            "reading": reading,
            "edges": edges,
            "unresolved": unresolved
                .iter()
                .map(|(site, reason)| json!([site.canonical(), reason]))
                .collect::<Vec<_>>(),
            "proof": proof,
            "membership": packages,
            "packages": descriptors.iter().collect::<Vec<_>>(),
            "coverage": coverage
                .iter()
                .map(|((test, gates), row)| json!([test, gates, coverage_row(row)]))
                .collect::<Vec<_>>(),
            "mapped": mapped,
            "semantic": semantic,
        })
    }
}

fn coverage_row(row: &SiteCoverage) -> Value {
    // Exhaustive, so a new field fails to compile until it is compared.
    let SiteCoverage {
        sites,
        mapped,
        unresolved,
        semantic,
    } = row;
    json!([sites, mapped, unresolved, semantic])
}

impl Site {
    /// The serialized site and the fields its JSON skips.
    fn canonical(&self) -> Value {
        // Exhaustive, so a new field fails to compile until it is compared: the JSON carries
        // every field but `gates`, `ordinal` and the metadata's stored basis, which are added
        // here.
        let Self {
            owner_syntax_id: _,
            role: _,
            range: _,
            lines: _,
            relation_kind: _,
            spelling: _,
            call_id: _,
            caller: _,
            caller_reason: _,
            owner: _,
            target: _,
            target_reason: _,
            mapping: _,
            provenance_id: _,
            semantic_metadata,
            gates,
            ordinal,
        } = self;
        let mut value = json!(self);
        value["gates"] = json!(gates);
        value["ordinal"] = json!(ordinal);
        value["basis"] = json!(semantic_metadata.basis_fields().map(|(basis, _)| basis));
        value
    }
}
