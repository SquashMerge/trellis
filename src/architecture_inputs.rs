//! The architecture snapshot inputs: the records storage reads for one revision and hands to
//! the references read (S15). Plain data over shared records; re-exported at their old
//! paths in `references`, `references::external` and `references::tsconfig`.
use crate::model::{IndexPin, SourceFile};
use crate::native_records::{Call, Declaration, DocumentKey};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct DocumentInput {
    pub key: DocumentKey,
    pub file: SourceFile,
    pub declarations: Vec<Declaration>,
    pub calls: Vec<Call>,
    /// Selected current or explicitly labelled historical producer evidence.
    pub semantic: DocumentSemantic,
}
pub struct Snapshot {
    pub pin: IndexPin,
    pub documents: Vec<DocumentInput>,
    pub manifests: Vec<ManifestInput>,
    /// The root `tsconfig.json`, else `jsconfig.json`, when the revision captured one.
    pub root_tsconfig: Option<RootTsconfigInput>,
    /// Documents published with no evidence but their file module, and their `partialFiles`
    /// reason: over the evidence budget (#564), or not read (#576).
    pub evidence_gaps: BTreeMap<DocumentKey, &'static str>,
}

/// Selected producer evidence: current bindings that are not stale (fresh or possibly stale,
/// #936) or an authorized historical overlay.
/// Historical outcomes retain P authority and cannot be re-resolved against native N.
#[derive(Debug, Default, Clone)]
pub struct DocumentSemantic {
    /// The selected producer, when the document has a usable coverage tuple.
    pub producer_id: Option<String>,
    /// The selected coverage tuple is complete, not partial.
    pub complete: bool,
    /// Call bindings of measured calls and resolved semantic references, by source range.
    pub anchors: Vec<SemanticAnchor>,
    /// `extends`/`implements` relationships, not stale, whose source is declared here.
    pub relationships: Vec<SemanticRelationship>,
    /// Native-N selection of a validated historical semantic basis, or explicit pending.
    pub projection: Option<crate::model::SemanticProjection>,
    /// The original coverage status, including unavailable/not-requested evidence.
    pub coverage_state: Option<String>,
    pub boundary: bool,
}

/// One bound name or call in a document.
#[derive(Debug, Clone)]
pub struct SemanticAnchor {
    /// The producer's joined source range: a callee or invocation, or a referenced name.
    pub range: crate::native_records::Range,
    /// The measured call of a call binding; `None` for a semantic reference.
    pub call_id: Option<String>,
    /// A semantic reference with the `type` role.
    pub type_role: bool,
    /// A call binding whose dispatch is direct or a constructor.
    pub direct: bool,
    pub outcome: SemanticOutcome,
    /// The binding resolves to an internal declaration its own basis confirmed current: the
    /// binding a site of its own needs at that basis. A projected binding keeps it when its
    /// endpoint is now catching up (#934).
    pub basis_site: bool,
    pub provenance_id: String,
    pub metadata: SemanticMetadata,
}

/// The provenance metadata of one anchor or relationship, as one JSON object. The fields its
/// basis and projection determine (`basis`, `freshness`, `historicalFreshness`, `projection`)
/// are built once per tuple and basis and shared by every row of that basis in one read
/// (#1247); the rest are the row's own. The two key sets never overlap, so the object reads,
/// iterates and serializes exactly as one map.
#[derive(Debug, Clone, Default)]
pub struct SemanticMetadata {
    own: BTreeMap<String, Value>,
    shared: Option<Arc<BTreeMap<String, Value>>>,
}

impl SemanticMetadata {
    /// A row's own fields over the fields it shares; an own field overrides a shared one.
    pub fn new(own: BTreeMap<String, Value>, shared: Arc<BTreeMap<String, Value>>) -> Self {
        let mut metadata = Self {
            own: BTreeMap::new(),
            shared: Some(shared),
        };
        for (key, value) in own {
            metadata.insert(key, value);
        }
        metadata
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.own
            .get(key)
            .or_else(|| self.shared.as_ref().and_then(|shared| shared.get(key)))
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Sets a field of this row alone. A shared field of the same name is overridden; the
    /// other rows of the basis keep it.
    pub fn insert(&mut self, key: String, value: Value) {
        if let Some(shared) = &mut self.shared
            && shared.contains_key(&key)
        {
            Arc::make_mut(shared).remove(&key);
        }
        self.own.insert(key, value);
    }

    /// Every field in key order, as one `BTreeMap` iterates them.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        let mut own = self.own.iter().peekable();
        let mut shared = self.shared.iter().flat_map(|s| s.iter()).peekable();
        std::iter::from_fn(move || match (own.peek(), shared.peek()) {
            (Some((a, _)), Some((b, _))) if b < a => shared.next(),
            (Some(_), _) => own.next(),
            (None, _) => shared.next(),
        })
    }

    /// Whether both rows hold the same shared fields, not merely equal copies.
    pub fn shares_basis_with(&self, other: &Self) -> bool {
        matches!((&self.shared, &other.shared), (Some(a), Some(b)) if Arc::ptr_eq(a, b))
    }
}

impl From<BTreeMap<String, Value>> for SemanticMetadata {
    fn from(own: BTreeMap<String, Value>) -> Self {
        Self { own, shared: None }
    }
}

impl serde::Serialize for SemanticMetadata {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticOutcome {
    /// A declared internal target: its document's index in `Snapshot::documents` and its
    /// syntax ID.
    Internal(usize, String),
    /// Bound outside the checkout.
    External,
    Ambiguous,
    Unresolved,
    /// An endpoint is not eligible against native N; never resolve it by spelling.
    Boundary,
}

/// A semantic type relationship, not stale, from a declaration of this document.
#[derive(Debug, Clone)]
pub struct SemanticRelationship {
    /// `extends` or `implements`.
    pub kind: String,
    pub source_syntax_id: String,
    /// The internal target, or `None` for an external one.
    pub target: Option<(usize, String)>,
    pub provenance_id: String,
    pub metadata: SemanticMetadata,
}

#[derive(Clone, Debug)]
pub struct ManifestInput {
    pub source_set_id: String,
    pub native_revision_id: String,
    pub dependency_hash: String,
    pub path: String,
    pub content_hash: Option<String>,
    pub bytes: Option<Vec<u8>>,
}

/// The captured bytes of the selected root config of one source set.
#[derive(Clone, Debug)]
pub struct RootTsconfigInput {
    pub source_set_id: String,
    pub path: String,
    pub bytes: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A site's shape: metadata flattened beside the site's own fields.
    #[derive(serde::Serialize)]
    struct Site<M> {
        mapping: &'static str,
        #[serde(flatten)]
        metadata: M,
    }

    /// #1247: shared-plus-own metadata reads, iterates and serializes exactly as the flat map
    /// every row used to own, a row-level override of a shared field included, and the
    /// override leaves the other rows of the basis as they were.
    #[test]
    fn shared_metadata_serializes_exactly_as_the_flat_map() {
        let shared = Arc::new(BTreeMap::from([
            (
                "basis".to_owned(),
                json!({"producerId":"p","lookupDependencies":["hello"]}),
            ),
            ("freshness".to_owned(), json!("possiblyStale")),
            ("historicalFreshness".to_owned(), json!("fresh")),
            (
                "projection".to_owned(),
                json!({"semanticStatus":"olderBasis"}),
            ),
        ]));
        let own = BTreeMap::from([
            ("derivedFrom".to_owned(), Value::Null),
            ("evidenceKind".to_owned(), json!("semanticReference")),
            ("historicalContentHash".to_owned(), json!("h")),
        ]);
        let mut flat = (*shared).clone();
        flat.extend(own.clone());
        let mut row = SemanticMetadata::new(own, shared.clone());
        let sibling = SemanticMetadata::new(BTreeMap::new(), shared.clone());
        assert!(row.shares_basis_with(&sibling));
        let same = |row: &SemanticMetadata, flat: &BTreeMap<String, Value>| {
            assert_eq!(
                serde_json::to_string(row).unwrap(),
                serde_json::to_string(flat).unwrap()
            );
            assert_eq!(
                serde_json::to_string(&Site {
                    mapping: "semantic",
                    metadata: row
                })
                .unwrap(),
                serde_json::to_string(&Site {
                    mapping: "semantic",
                    metadata: flat
                })
                .unwrap()
            );
            assert!(row.iter().eq(flat.iter()));
            for key in flat.keys() {
                assert_eq!(row.get(key), flat.get(key), "{key}");
                assert!(row.contains_key(key), "{key}");
            }
            assert!(row.get("absent").is_none() && !row.contains_key("absent"));
        };
        same(&row, &flat);

        // A row's own field between shared ones, and a row-level override of a shared one.
        for (key, value) in [
            ("historicalResolution", json!("resolved")),
            ("freshness", json!("stale")),
        ] {
            row.insert(key.into(), value.clone());
            flat.insert(key.into(), value);
        }
        same(&row, &flat);
        assert!(!row.shares_basis_with(&sibling));
        same(&sibling, &shared);
        assert_eq!(sibling.get("freshness"), Some(&json!("possiblyStale")));

        // An own field given at construction overrides the shared one the same way.
        let constructed = SemanticMetadata::new(
            BTreeMap::from([("freshness".to_owned(), json!("stale"))]),
            shared.clone(),
        );
        let mut expected = (*shared).clone();
        expected.insert("freshness".into(), json!("stale"));
        same(&constructed, &expected);
        same(&SemanticMetadata::from(flat.clone()), &flat);
        same(&SemanticMetadata::default(), &BTreeMap::new());
    }
}
