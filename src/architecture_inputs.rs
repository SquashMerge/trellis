//! The architecture snapshot inputs: the records storage reads for one revision and hands to
//! the references read (S15). Plain data over shared records; re-exported at their old
//! paths in `references`, `references::external` and `references::tsconfig`.
use crate::model::{IndexPin, SourceFile};
use crate::native_records::{Call, Declaration, DocumentKey};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Architecture-only identities read beside the snapshot, never required of native fixtures.
#[derive(Clone, Debug, Default, serde::Serialize, PartialEq, Eq)]
pub struct ArchitectureFingerprints {
    pub documents: BTreeMap<DocumentKey, DocumentFingerprint>,
    /// Admission metadata used to assemble current global state without reading clean payloads.
    /// The finished dependency index keeps only the fingerprints, not this map.
    pub semantic_states: BTreeMap<DocumentKey, SemanticInputState>,
    pub producers: [u8; 32],
    pub selections: [u8; 32],
    /// The reading revision's native revision ID. Rejoined provenance IDs read at this pin
    /// are derived from it (Decision 0021 D2); each core mints its reused sources' IDs from
    /// it again (#1310).
    pub native_revision: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, PartialEq, Eq)]
pub struct SemanticInputState {
    pub default_producer: Option<String>,
    pub projection: Option<crate::model::SemanticProjection>,
    pub tuple: Option<SemanticTupleState>,
    pub freshness: Option<SemanticFreshnessState>,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct SemanticTupleState {
    pub selected: bool,
    pub state: String,
    pub payload_id: Option<i64>,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct SemanticFreshnessState {
    /// The pin whose freshness map was read. For a current tuple this is a lookup pin,
    /// not semantic identity; only a historical projection makes it a basis identity.
    pub basis_pin: IndexPin,
    pub values: BTreeMap<i64, String>,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct DocumentFingerprint {
    pub native: NativeFingerprint,
    pub semantic: [u8; 32],
    pub sids: [u8; 32],
    /// A historical projection (#620) under the default producer. Its evidence is read at
    /// its semantic basis, and its endpoints' eligibility at the current pin.
    pub projected: bool,
    /// The document's projection state under every producer that projects at this pin, by
    /// producer: a projected source of that producer reads its endpoints' eligibility from it,
    /// so the source is Dirty when it changes for a retained endpoint (#1310). Empty at a pin
    /// without projections.
    pub producer_projections: BTreeMap<String, [u8; 32]>,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct NativeFingerprint {
    pub version_id: String,
    pub coverage_requested: bool,
    pub coverage_selected: bool,
    pub coverage_state: String,
    pub coverage_diagnostic: Option<String>,
}

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
    /// The documents among those whose Rust parse indexing found past measurement's
    /// parse-work budget (#1282). Their syntax was never read, so no read parses them (#1300).
    pub unparsed: BTreeSet<DocumentKey>,
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
    /// Dependency-only checkout endpoints, before representative selection, eligibility
    /// suppression or reference sampling. Indices address the complete snapshot.
    pub endpoint_documents: BTreeSet<usize>,
    /// What the revision-scoped metadata of this document's selected payload is read at:
    /// Read B's for a document it read, else Read A's for the same pin (#1310). `None`
    /// without a selected complete or partial payload.
    pub reading: Option<SemanticReading>,
}

/// The revision-scoped inputs of a payload's metadata (#1310). A reused source's output was
/// materialized for one reading; a later core rebases it to its own.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SemanticReading {
    /// The native revision rejoined provenance IDs are minted at (Decision 0021 D2): the
    /// reading pin's, or a projection's semantic basis's.
    pub native: String,
    /// Each stored basis's freshness at the pin the payload is read at.
    pub freshness: BTreeMap<i64, String>,
}

impl SemanticReading {
    /// The reading Read B uses for a document in `state` at a pin whose native revision is
    /// `native`: only a selected complete or partial payload, not pending, is read.
    pub(crate) fn of(state: &SemanticInputState, native: &str) -> Option<Self> {
        let projection = state.projection.as_ref();
        if projection.is_some_and(|projection| projection.semantic_status.is_pending()) {
            return None;
        }
        let tuple = state.tuple.as_ref()?;
        if !(tuple.selected
            && matches!(tuple.state.as_str(), "complete" | "partial")
            && tuple.payload_id.is_some())
        {
            return None;
        }
        Some(Self {
            native: projection
                .map_or(native, |projection| &projection.basis_native_revision_id)
                .to_owned(),
            freshness: state.freshness.as_ref()?.values.clone(),
        })
    }
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
    /// The provenance ID at the reading native revision (Decision 0021 D2).
    pub provenance_id: String,
    /// The stable identity that orders tied anchors and representative proofs (#1310), and
    /// from which a later core mints `provenance_id` again at its own native revision.
    pub proof: crate::semantic_records::ProofIdentity,
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

/// Sets the shared fields that the basis's freshness at the reading pin and the document's
/// projection determine, as Read B builds them (#936, #1247). A later core rebuilds them the
/// same way from its own Read A state (#1310).
pub(crate) fn set_freshness_fields(
    fields: &mut BTreeMap<String, Value>,
    historical: &str,
    projection: Option<&crate::model::SemanticProjection>,
) -> anyhow::Result<()> {
    fields.insert(
        "freshness".into(),
        Value::String(if projection.is_some() && historical != "stale" {
            "possiblyStale".into()
        } else {
            historical.into()
        }),
    );
    fields.insert(
        "historicalFreshness".into(),
        Value::String(historical.into()),
    );
    match projection {
        Some(projection) => {
            fields.insert("projection".into(), serde_json::to_value(projection)?);
        }
        None => {
            fields.remove("projection");
        }
    }
    Ok(())
}

/// The fields of one row's own proof: `derivedFrom` and `evidenceKind`.
#[derive(Debug)]
pub struct RowProvenance {
    /// The stored evidence kind, one allocation per distinct kind in a read.
    pub evidence_kind: Arc<str>,
    /// The raw proof the row derives from; JSON `null` without one.
    pub derived_from: Option<crate::semantic_records::DerivedFrom>,
    /// The stored basis whose freshness the shared fields carry, so a later core can rebuild
    /// them from its own freshness and projection (#1310). Never serialized.
    pub basis: i64,
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

    /// A row's typed proof fields over the fields its basis shares, which must not name them.
    pub fn with_row(row: Arc<RowProvenance>, shared: Arc<BTreeMap<String, Value>>) -> Self {
        debug_assert!(ROW_KEYS.iter().all(|key| !shared.contains_key(*key)));
        Self {
            row: Some(row),
            own: BTreeMap::new(),
            shared: Some(shared),
        }
    }

    /// The stored basis of a row built by [`Self::with_row`], and the fields it shares.
    pub(crate) fn basis_fields(&self) -> Option<(i64, &Arc<BTreeMap<String, Value>>)> {
        Some((self.row.as_ref()?.basis, self.shared.as_ref()?))
    }

    /// The stable identity of a rejoined row's proof, from which a core mints its provenance
    /// ID (#1310); `None` for a row with a literal provenance ID.
    pub(crate) fn derived_proof(
        &self,
    ) -> anyhow::Result<Option<crate::semantic_records::ProofIdentity>> {
        let Some(row) = &self.row else {
            return Ok(None);
        };
        let Some(raw) = &row.derived_from else {
            return Ok(None);
        };
        Ok(Some(crate::semantic_records::ProofIdentity::Derived {
            raw: raw.provenance_id.clone(),
            kind: serde_json::from_value(Value::String(row.evidence_kind.as_ref().to_owned()))?,
        }))
    }

    /// Replaces the shared fields, keeping this row's own and proof fields. The new fields
    /// must name what the old ones did, so no own field starts or stops overriding one.
    pub(crate) fn replace_shared(&mut self, shared: Arc<BTreeMap<String, Value>>) {
        debug_assert!(ROW_KEYS.iter().all(|key| !shared.contains_key(*key)));
        debug_assert!(self.own.keys().all(|key| !shared.contains_key(key)));
        self.shared = Some(shared);
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
    pub proof: crate::semantic_records::ProofIdentity,
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
                basis: 1,
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

/// The stored identity of one document's native rows at a revision: its content-addressed
/// version, graph projection and class projection, and the producer that wrote it. Two reads
/// of the same identity decode the same rows but for their revision-scoped IDs (#1315).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeIdentity {
    pub(crate) version_id: String,
    pub(crate) graph_hash: String,
    pub(crate) class_hash: Option<String>,
    pub(crate) producer_id: String,
}

/// What Read A learned of one document beside its rows: its identity, its fingerprint, the
/// sizes its byte budgets charged, and whether it was read or taken from kept documents.
#[derive(Clone, Debug)]
pub(crate) struct NativeTable {
    pub(crate) identity: NativeIdentity,
    pub(crate) fingerprint: NativeFingerprint,
    pub(crate) bytes: i64,
    pub(crate) stored_bytes: u64,
    pub(crate) read: bool,
}

/// One Read A's native documents, in snapshot order, with their tables.
pub(crate) struct NativeRead {
    pub(crate) snapshot: Snapshot,
    pub(crate) regions: Vec<Vec<crate::native_records::ControlRegion>>,
    pub(crate) tables: Vec<NativeTable>,
}

/// One document an earlier Read A read, kept for the next one (#1315).
///
/// Its rows are kept without their per-row copies of the document's key, native revision ID
/// and native proof ID (#1313): a read decodes the same three values into every row, so
/// [`Self::restore`] refills them for the revision that takes the document.
pub(crate) struct KeptNativeDocument {
    pub(crate) table: NativeTable,
    input: DocumentInput,
    regions: Vec<crate::native_records::ControlRegion>,
}

/// The native documents of the owner's last Read A, by key, within one index generation.
/// They are owner-level auxiliary state: no result depends on which build kept them.
pub(crate) struct KeptNative {
    pub(crate) generation: uuid::Uuid,
    pub(crate) documents: BTreeMap<DocumentKey, KeptNativeDocument>,
}

impl KeptNative {
    /// The documents of a finished Read A, without the semantic rows Read B added.
    pub(crate) fn of(
        generation: uuid::Uuid,
        documents: Vec<DocumentInput>,
        regions: Vec<Vec<crate::native_records::ControlRegion>>,
        tables: Vec<NativeTable>,
    ) -> Option<Self> {
        if documents.len() != regions.len() || documents.len() != tables.len() {
            return None;
        }
        Some(Self {
            generation,
            documents: documents
                .into_iter()
                .zip(regions)
                .zip(tables)
                .map(|((mut input, mut regions), table)| {
                    input.semantic = DocumentSemantic::default();
                    strip_rows(&mut input, &mut regions);
                    (
                        input.key.clone(),
                        KeptNativeDocument {
                            table,
                            input,
                            regions,
                        },
                    )
                })
                .collect(),
        })
    }
}

/// Drops the per-row copies of the document's key, native revision ID and proof ID.
fn strip_rows(input: &mut DocumentInput, regions: &mut [crate::native_records::ControlRegion]) {
    let empty = || DocumentKey {
        source_set_id: String::new(),
        language: String::new(),
        path: String::new(),
    };
    for declaration in &mut input.declarations {
        declaration.document = empty();
        declaration.revision_id = String::new();
        declaration.provenance_id = String::new();
    }
    for call in &mut input.calls {
        call.document = empty();
        call.revision_id = String::new();
        call.provenance_id = String::new();
    }
    for region in regions {
        region.document = empty();
        region.revision_id = String::new();
        region.provenance_id = String::new();
    }
}

impl KeptNativeDocument {
    /// Whether any kept row still holds a copy of its document's key, native revision ID or
    /// proof ID.
    #[cfg(test)]
    pub(crate) fn holds_row_copies(&self) -> bool {
        let empty = |document: &DocumentKey, revision: &str, proof: &str| {
            document.source_set_id.is_empty()
                && document.language.is_empty()
                && document.path.is_empty()
                && revision.is_empty()
                && proof.is_empty()
        };
        !(self
            .input
            .declarations
            .iter()
            .all(|row| empty(&row.document, &row.revision_id, &row.provenance_id))
            && self
                .input
                .calls
                .iter()
                .all(|row| empty(&row.document, &row.revision_id, &row.provenance_id))
            && self
                .regions
                .iter()
                .all(|row| empty(&row.document, &row.revision_id, &row.provenance_id)))
    }

    /// The kept document as a read at native revision `native_revision` decodes it: every
    /// row gets the document's key, that native revision ID and the document's native proof
    /// ID under `producer_id`, as the decoder writes them.
    pub(crate) fn restore(
        self,
        native_revision: &str,
        producer_id: &str,
    ) -> (DocumentInput, Vec<crate::native_records::ControlRegion>) {
        let Self {
            table: _,
            mut input,
            mut regions,
        } = self;
        let proof = crate::store::native_proof_id(producer_id, &input.key, native_revision);
        let DocumentInput {
            key,
            file: _,
            declarations,
            calls,
            semantic: _,
        } = &mut input;
        for declaration in declarations {
            declaration.document.clone_from(key);
            declaration.revision_id = native_revision.to_owned();
            declaration.provenance_id.clone_from(&proof);
        }
        for call in calls {
            call.document.clone_from(key);
            call.revision_id = native_revision.to_owned();
            call.provenance_id.clone_from(&proof);
        }
        for region in &mut regions {
            region.document.clone_from(key);
            region.revision_id = native_revision.to_owned();
            region.provenance_id.clone_from(&proof);
        }
        (input, regions)
    }
}
