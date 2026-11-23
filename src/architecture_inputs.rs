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
/// basis and projection determine (`basis`, `freshness`, `historicalContentHash`,
/// `historicalFreshness`, `projection`) are built once per tuple and basis and shared by every
/// row of that basis in one read (#1247). The two fields of the row's own proof
/// (`derivedFrom`, `evidenceKind`) are kept typed in one shared allocation and written as JSON
/// only when read, so a copy of the metadata, such as a site's, allocates nothing (#1247).
/// Any other field is the row's own JSON. The key sets never overlap, so the object
/// reads, iterates and serializes exactly as one map.
#[derive(Debug, Clone, Default)]
pub struct SemanticMetadata {
    row: Option<Arc<RowProvenance>>,
    own: BTreeMap<String, Value>,
    shared: Option<Arc<BTreeMap<String, Value>>>,
}

/// The fields of one row's own proof: `derivedFrom` and `evidenceKind`.
#[derive(Debug)]
pub struct RowProvenance {
    /// The stored evidence kind, one allocation per distinct kind in a read.
    pub evidence_kind: Arc<str>,
    /// The raw proof the row derives from; JSON `null` without one.
    pub derived_from: Option<crate::semantic_records::DerivedFrom>,
}

/// The keys of a [`RowProvenance`], in key order.
const ROW_KEYS: [&str; 2] = ["derivedFrom", "evidenceKind"];

impl RowProvenance {
    fn value(&self, key: &str) -> Option<Value> {
        match key {
            "derivedFrom" => Some(
                serde_json::to_value(&self.derived_from).expect("a derivation serializes as JSON"),
            ),
            "evidenceKind" => Some(Value::String(self.evidence_kind.as_ref().to_owned())),
            _ => None,
        }
    }
}

impl SemanticMetadata {
    /// A row's own fields over the fields it shares; an own field overrides a shared one.
    pub fn new(own: BTreeMap<String, Value>, shared: Arc<BTreeMap<String, Value>>) -> Self {
        let mut metadata = Self {
            row: None,
            own: BTreeMap::new(),
            shared: Some(shared),
        };
        for (key, value) in own {
            metadata.insert(key, value);
        }
        metadata
    }

    /// A row's typed proof fields over the fields it shares, which must not name them.
    pub fn with_row(row: Arc<RowProvenance>, shared: Arc<BTreeMap<String, Value>>) -> Self {
        debug_assert!(ROW_KEYS.iter().all(|key| !shared.contains_key(*key)));
        Self {
            row: Some(row),
            own: BTreeMap::new(),
            shared: Some(shared),
        }
    }

    pub fn get(&self, key: &str) -> Option<std::borrow::Cow<'_, Value>> {
        use std::borrow::Cow;
        self.own
            .get(key)
            .map(Cow::Borrowed)
            .or_else(|| self.row.as_ref()?.value(key).map(Cow::Owned))
            .or_else(|| {
                self.shared
                    .as_ref()
                    .and_then(|shared| shared.get(key))
                    .map(Cow::Borrowed)
            })
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.own.contains_key(key)
            || (self.row.is_some() && ROW_KEYS.contains(&key))
            || self
                .shared
                .as_ref()
                .is_some_and(|shared| shared.contains_key(key))
    }

    /// Sets a field of this row alone. A shared field of the same name is overridden; the
    /// other rows of the basis keep it. Overriding a typed proof field writes the other one
    /// as this row's own JSON.
    pub fn insert(&mut self, key: String, value: Value) {
        if ROW_KEYS.contains(&key.as_str())
            && let Some(row) = self.row.take()
        {
            for other in ROW_KEYS {
                if other != key {
                    self.own
                        .insert(other.to_owned(), row.value(other).expect("a row key"));
                }
            }
        }
        if let Some(shared) = &mut self.shared
            && shared.contains_key(&key)
        {
            Arc::make_mut(shared).remove(&key);
        }
        self.own.insert(key, value);
    }

    /// Every field in key order, as one `BTreeMap` iterates them.
    pub fn iter(&self) -> impl Iterator<Item = (&str, std::borrow::Cow<'_, Value>)> {
        use std::borrow::Cow;
        let mut own = self
            .own
            .iter()
            .map(|(key, value)| (key.as_str(), Cow::Borrowed(value)))
            .peekable();
        let mut row = self
            .row
            .iter()
            .flat_map(|row| {
                ROW_KEYS
                    .into_iter()
                    .map(move |key| (key, Cow::Owned(row.value(key).expect("a row key"))))
            })
            .peekable();
        let mut shared = self
            .shared
            .iter()
            .flat_map(|shared| shared.iter())
            .map(|(key, value)| (key.as_str(), Cow::Borrowed(value)))
            .peekable();
        std::iter::from_fn(move || {
            let keys = [
                own.peek().map(|(key, _)| *key),
                row.peek().map(|(key, _)| *key),
                shared.peek().map(|(key, _)| *key),
            ];
            let least = *keys.iter().flatten().min()?;
            if keys[0] == Some(least) {
                own.next()
            } else if keys[1] == Some(least) {
                row.next()
            } else {
                shared.next()
            }
        })
    }

    /// Whether both rows hold the same shared fields, not merely equal copies.
    pub fn shares_basis_with(&self, other: &Self) -> bool {
        matches!((&self.shared, &other.shared), (Some(a), Some(b)) if Arc::ptr_eq(a, b))
    }
}

impl From<BTreeMap<String, Value>> for SemanticMetadata {
    fn from(own: BTreeMap<String, Value>) -> Self {
        Self {
            row: None,
            own,
            shared: None,
        }
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

    /// The fields of a flat map that `row`'s typed proof fields stand for.
    fn row_fields(row: &RowProvenance) -> BTreeMap<String, Value> {
        BTreeMap::from([
            (
                "derivedFrom".to_owned(),
                serde_json::to_value(&row.derived_from).unwrap(),
            ),
            ("evidenceKind".to_owned(), json!(row.evidence_kind.as_ref())),
        ])
    }

    /// Reads, iterates and serializes `row` exactly as the flat map `flat`.
    fn same(row: &SemanticMetadata, flat: &BTreeMap<String, Value>) {
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
        assert_eq!(
            serde_json::to_value(row).unwrap(),
            serde_json::to_value(flat).unwrap()
        );
        assert!(
            row.iter()
                .map(|(key, value)| (key.to_owned(), value.into_owned()))
                .eq(flat.iter().map(|(key, value)| (key.clone(), value.clone())))
        );
        for (key, value) in flat {
            assert_eq!(row.get(key).as_deref(), Some(value), "{key}");
            assert!(row.contains_key(key), "{key}");
        }
        assert!(row.get("absent").is_none() && !row.contains_key("absent"));
    }

    fn shared() -> Arc<BTreeMap<String, Value>> {
        Arc::new(BTreeMap::from([
            (
                "basis".to_owned(),
                json!({"producerId":"p","lookupDependencies":["hello"]}),
            ),
            ("freshness".to_owned(), json!("possiblyStale")),
            ("historicalContentHash".to_owned(), json!("h")),
            ("historicalFreshness".to_owned(), json!("fresh")),
            (
                "projection".to_owned(),
                json!({"semanticStatus":"olderBasis"}),
            ),
        ]))
    }

    /// #1247: shared-plus-own metadata reads, iterates and serializes exactly as the flat map
    /// every row used to own, a row-level override of a shared field included, and the
    /// override leaves the other rows of the basis as they were.
    #[test]
    fn shared_metadata_serializes_exactly_as_the_flat_map() {
        let shared = shared();
        let own = BTreeMap::from([
            ("derivedFrom".to_owned(), Value::Null),
            ("evidenceKind".to_owned(), json!("semanticReference")),
        ]);
        let mut flat = (*shared).clone();
        flat.extend(own.clone());
        let mut row = SemanticMetadata::new(own, shared.clone());
        let sibling = SemanticMetadata::new(BTreeMap::new(), shared.clone());
        assert!(row.shares_basis_with(&sibling));
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
        assert_eq!(
            sibling.get("freshness").as_deref(),
            Some(&json!("possiblyStale"))
        );

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

    /// #1247: a row's typed proof fields, with or without a raw proof, read, iterate
    /// and serialize as the JSON the flat map held, between the shared fields and the row's
    /// own; a copy shares the row's one allocation; an override of a typed field leaves the
    /// other as the row's own JSON.
    #[test]
    fn typed_row_fields_serialize_exactly_as_the_flat_map() {
        let shared = shared();
        for derived_from in [
            None,
            Some(crate::semantic_records::DerivedFrom {
                provenance_id: "a".repeat(64),
                record_kind: crate::semantic_records::RecordKind::Occurrence,
                fact_digest: "b".repeat(64),
            }),
        ] {
            let provenance = Arc::new(RowProvenance {
                evidence_kind: Arc::from("semanticReference"),
                derived_from,
            });
            let mut flat = (*shared).clone();
            flat.extend(row_fields(&provenance));
            let mut row = SemanticMetadata::with_row(provenance.clone(), shared.clone());
            same(&row, &flat);
            let copy = row.clone();
            assert_eq!(Arc::strong_count(&provenance), 3);
            assert!(copy.shares_basis_with(&row));

            // Fields of the row's own around the typed ones: before, between and after.
            for (key, value) in [
                ("basisSite", json!(true)),
                ("derivedFromNote", json!("x")),
                ("historicalJoin", json!({"status":"joined"})),
            ] {
                row.insert(key.into(), value.clone());
                flat.insert(key.into(), value);
            }
            same(&row, &flat);
            same(&copy, &{
                let mut flat = (*shared).clone();
                flat.extend(row_fields(&provenance));
                flat
            });

            // Overriding one typed field keeps the other.
            row.insert("evidenceKind".into(), json!("semanticCall"));
            flat.insert("evidenceKind".into(), json!("semanticCall"));
            same(&row, &flat);
        }
    }
}
