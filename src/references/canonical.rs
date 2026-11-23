//! The canonical form of a [`ReferenceBase`] (#1301): every field, so two results compare
//! equal only when every read of them answers the same. Test support only.
use super::{EdgeSites, MAX_SAMPLE_LIMIT, ReferenceBase, Site, SourceReferences, Target};
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
            .map(|((path, test), row)| {
                json!([
                    path,
                    test,
                    row.sites,
                    row.mapped,
                    row.unresolved,
                    row.semantic
                ])
            })
            .collect();
        Ok(json!({
            "pin": pin,
            "partialFiles": partial_files,
            "identities": identities,
            "modules": modules,
            "sources": sources
                .iter()
                .map(|source| source.as_deref().map(SourceReferences::canonical))
                .collect::<Vec<_>>(),
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
    fn canonical(&self) -> Value {
        let Self {
            projection,
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
            "edges": edges,
            "unresolved": unresolved
                .iter()
                .map(|(site, reason)| json!([site.canonical(), reason]))
                .collect::<Vec<_>>(),
            "proof": proof,
            "packages": packages.iter().collect::<Vec<_>>(),
            "coverage": coverage
                .iter()
                .map(|((test, gates), row)| {
                    json!([test, gates, row.sites, row.mapped, row.unresolved, row.semantic])
                })
                .collect::<Vec<_>>(),
            "mapped": mapped,
            "semantic": semantic,
        })
    }
}

impl Site {
    /// The serialized site and the fields its JSON skips.
    fn canonical(&self) -> Value {
        let mut value = json!(self);
        value["gates"] = json!(self.gates);
        value["ordinal"] = json!(self.ordinal);
        value
    }
}
