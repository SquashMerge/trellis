//! Bounded, presentation-only class diagrams. Stored relation certainty is never changed.
use crate::model::{ClassDefinition, ClassRelation, IndexPin};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_NODES: usize = 24;
pub const MAX_EDGES: usize = 64;
pub const MAX_EXPANDED: usize = 12;
/// Presentation limits; the persisted catalog retains all recorded evidence.
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const CLASS_BYTES: usize = 64 * 1024;
pub(crate) const PAGE_BYTES: usize = 3 * 1024 * 1024;
pub(crate) const BYTE_NOTICE: &str = "Presentation byte limits omitted some class members or relationships; stored source evidence is unchanged.";
pub const INDEX_NOTICE: &str = "Index workspace to build class diagrams for this snapshot.";

#[derive(Debug)]
pub struct InvalidRequest(pub &'static str);
impl std::fmt::Display for InvalidRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for InvalidRequest {}
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 8192 && !id.contains('\0')
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClassDiagramRequest {
    pub seed: String,
    pub expected_revision: IndexPin,
    #[serde(default)]
    pub expanded: Vec<String>,
    #[serde(default)]
    pub include_unmatched: bool,
    #[serde(default)]
    pub include_hierarchy: bool,
}
impl ClassDiagramRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            valid_id(&self.seed),
            InvalidRequest("Choose a valid class or method ID.")
        );
        ensure!(
            self.expanded.len() <= MAX_EXPANDED && self.expanded.iter().all(|id| valid_id(id)),
            InvalidRequest("Choose at most 12 valid related classes to expand.")
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassDiagramNode {
    pub id: String,
    pub class: Option<ClassDefinition>,
    pub label: String,
    pub kind: String,
    pub expandable: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassDiagram {
    pub revision: IndexPin,
    pub seed: String,
    pub nodes: Vec<ClassDiagramNode>,
    pub edges: Vec<ClassRelation>,
    pub warnings: Vec<String>,
    pub truncated: bool,
    pub require_index: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassPage {
    pub revision: IndexPin,
    pub items: Vec<ClassDefinition>,
    pub next_offset: Option<usize>,
    pub truncated: bool,
    pub warnings: Vec<String>,
    pub require_index: bool,
}
impl ClassDiagram {
    pub fn unindexed(revision: IndexPin, seed: String) -> Self {
        Self {
            revision,
            seed,
            nodes: vec![],
            edges: vec![],
            warnings: vec![INDEX_NOTICE.into()],
            truncated: false,
            require_index: true,
        }
    }
}

/// `seeds` starts with the focus class, then expansions in user order. `bridges`
/// connects each expansion to an earlier seed, so caps cannot strand an expansion.
/// Relations are already bounded by the store, with actual links before hints.
pub(crate) fn project(
    revision: IndexPin,
    seeds: &[String],
    classes: &BTreeMap<String, ClassDefinition>,
    bridges: Vec<ClassRelation>,
    relations: Vec<ClassRelation>,
    mut warnings: Vec<String>,
    mut truncated: bool,
) -> Result<ClassDiagram> {
    // Syntax-only type names cannot connect classes. Keep only explicitly
    // selected measured declarations; relation hints never become arrows.
    let _ = (bridges, relations);
    let mut nodes = Vec::new();
    for seed in seeds {
        if let Some(class) = classes.get(seed) {
            nodes.push(ClassDiagramNode {
                id: class.symbol.id.clone(),
                label: class.qualified_name.clone(),
                class: Some(class.clone()),
                kind: "class".into(),
                expandable: true,
            });
        }
    }
    if nodes.len() > MAX_NODES {
        truncated = true;
        nodes.truncate(MAX_NODES);
    }
    if truncated {
        warnings.push("Class diagram is partial: catalog or node limit was reached.".into());
    }
    let edges = Vec::new();
    Ok(ClassDiagram {
        revision,
        seed: seeds[0].clone(),
        nodes,
        edges,
        warnings,
        truncated,
        require_index: false,
    })
}
