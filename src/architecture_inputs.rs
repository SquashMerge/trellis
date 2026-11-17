//! The architecture snapshot inputs: the records storage reads for one revision and hands to
//! the references read (S15). Plain data over shared records; re-exported at their old
//! paths in `references`, `references::external` and `references::tsconfig`.
use crate::model::{IndexPin, SourceFile};
use crate::native_records::{Call, Declaration, DocumentKey};
use serde_json::Value;
use std::collections::BTreeMap;

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
    pub metadata: BTreeMap<String, Value>,
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
    pub metadata: BTreeMap<String, Value>,
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
