//! Authenticated, document-only SCIP 0.10.0 capture. No destination coordinates are emitted.
use crate::{
    model::CancelFlag,
    native_ids::{canonical, digest},
    semantic_batch::{BatchCounters, BatchLimits},
};
use protobuf::Message;
use scip::types::{self, occurrence};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, Read};
use std::sync::atomic::Ordering;

pub mod cargo_targets;
pub mod module_roots;
pub(crate) use cargo_targets::CargoTargetsContinuation;
pub use cargo_targets::{CargoTarget, CargoTargets, CargoTargetsBuilder};
pub use module_roots::ModuleRoots;

pub const DECODER_VERSION: &str = "scip-0.10.0/CapturedScipFactV1/2";
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Completion {
    Complete,
    Partial,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Exclusion {
    pub language: String,
    pub relative_path: String,
    pub reason: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScipCaptureError {
    InvalidAdmission(String),
    InvalidArtifact(String),
    LimitExceeded(String),
    Cancelled,
    MissingProof,
    NonUniqueProof,
    DigestCollision,
}
impl std::fmt::Display for ScipCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl ScipCaptureError {
    /// The bare reason text, without the `Debug` variant wrapper that `Display` prints.
    pub fn detail(&self) -> &str {
        match self {
            Self::InvalidAdmission(detail)
            | Self::InvalidArtifact(detail)
            | Self::LimitExceeded(detail) => detail,
            Self::Cancelled => "cancelled",
            Self::MissingProof => "missing proof",
            Self::NonUniqueProof => "non-unique proof",
            Self::DigestCollision => "digest collision",
        }
    }
}
impl std::error::Error for ScipCaptureError {}
fn valid_path(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.contains(['\\', '\0'])
        && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}
fn invalid(s: &str) -> ScipCaptureError {
    ScipCaptureError::InvalidArtifact(s.into())
}
/// Bind SCIP's optional document language to the admitted source under pinned coverage.
pub fn bind_document_language<'a>(
    raw: &str,
    detected: &'a str,
    profile_languages: &[String],
) -> Result<&'a str, ScipCaptureError> {
    if detected.is_empty()
        || !profile_languages
            .iter()
            .any(|language| language == detected)
        || !raw.is_empty() && raw != detected
    {
        return Err(invalid("document source binding"));
    }
    Ok(detected)
}

fn limit(s: &str) -> ScipCaptureError {
    ScipCaptureError::LimitExceeded(s.into())
}
fn cancelled(flag: &CancelFlag) -> Result<(), ScipCaptureError> {
    if flag.load(Ordering::Relaxed) {
        Err(ScipCaptureError::Cancelled)
    } else {
        Ok(())
    }
}
pub(crate) fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
pub(crate) struct AuthenticatedScipDocument<'a> {
    pub language: String,
    pub relative_path: String,
    pub source_bytes: &'a [u8],
    pub content_hash: String,
}
#[cfg(test)]
pub(crate) struct AuthenticatedScipInput<'a> {
    pub artifact_bytes: &'a [u8],
    pub artifact_hash: String,
    pub producer_id: String,
    pub profile_id: String,
    pub profile_languages: Vec<String>,
    pub source_set_id: String,
    pub source_revision: String,
    pub source_manifest_hash: String,
    pub decoder_version: &'static str,
    pub completion: Completion,
    pub exclusions: Vec<Exclusion>,
    pub project_root_uri: String,
    pub producer_version: String,
    pub position_encoding: u8,
    pub protocol_version: i32,
    pub text_document_encoding: i32,
    pub documents: Vec<AuthenticatedScipDocument<'a>>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedScipFactV1 {
    pub format_version: u8,
    pub record_kind: String,
    pub relative_path: String,
    pub position_encoding: u8,
    pub raw: Value,
    /// The Cargo target of a Rust document whose target is determined (owner ruling
    /// 2026-10-30, #394). It is part of the envelope, so the fact digest and proof identity
    /// of the same raw fact differ between targets. Absent for every other document.
    pub cargo_target: Option<CargoTarget>,
}

impl CapturedScipFactV1 {
    /// The envelope's `cargoTarget`, as `make_claim` writes it.
    pub fn cargo_target_json(target: &CargoTarget) -> Value {
        json!({"kind": target.kind, "package": target.package, "root": target.root, "version": target.version})
    }

    /// Reads an envelope's `cargoTarget` back: absent, or exactly a known kind, a package
    /// name, a root and a version string or null.
    pub fn cargo_target_from(envelope: &Value) -> Result<Option<CargoTarget>, ScipCaptureError> {
        let Some(value) = envelope.get("cargoTarget") else {
            return Ok(None);
        };
        let object = value.as_object().filter(|object| object.len() == 4);
        let text = |key: &str| object.and_then(|o| o.get(key)).and_then(Value::as_str);
        let version = match object.and_then(|o| o.get("version")) {
            Some(Value::Null) => Some(None),
            Some(Value::String(version)) => Some(Some(version.clone())),
            _ => None,
        };
        match (text("kind"), text("package"), text("root"), version) {
            (Some(kind), Some(package), Some(root), Some(version))
                if cargo_targets::TARGET_KINDS.contains(&kind) && valid_path(root) =>
            {
                Ok(Some(CargoTarget {
                    kind: kind.to_owned(),
                    root: root.to_owned(),
                    package: package.to_owned(),
                    version,
                }))
            }
            _ => Err(invalid("cargo target")),
        }
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedClaim {
    pub envelope: CapturedScipFactV1,
    pub canonical_bytes: Vec<u8>,
    pub fact_digest: String,
    pub captured_proof_id: String,
    pub language: String,
    pub artifact_hash: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawProofIdentity {
    pub(crate) proof_id: String,
    pub(crate) record_kind: String,
    pub(crate) fact_digest: String,
    pub(crate) language: String,
    pub(crate) path: String,
    pub(crate) artifact_hash: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ProofInventory {
    pub(crate) artifact_hashes: Vec<String>,
    pub(crate) source_manifest_hash: String,
    pub(crate) capped_documents: CappedDocuments,
    pub(crate) claims: Vec<RawProofIdentity>,
}

fn unique_proof<'a, T>(
    rows: &'a [T],
    id: &str,
    kind: &str,
    digest: &str,
    fields: impl Fn(&T) -> (&str, &str, &str),
) -> Result<&'a T, ScipCaptureError> {
    let mut found = None;
    for row in rows {
        let (proof_id, record_kind, fact_digest) = fields(row);
        if proof_id == id {
            if found.is_some() {
                return Err(ScipCaptureError::NonUniqueProof);
            }
            found = Some((row, record_kind, fact_digest));
        }
    }
    let (row, record_kind, fact_digest) = found.ok_or(ScipCaptureError::MissingProof)?;
    if record_kind != kind || fact_digest != digest {
        return Err(ScipCaptureError::MissingProof);
    }
    Ok(row)
}

impl ProofInventory {
    /// A proof-ID index for looking up many proofs without a scan of the inventory each (#494).
    pub(crate) fn proof_lookup(&self) -> ProofLookup<'_> {
        let mut index = BTreeMap::new();
        for claim in &self.claims {
            index
                .entry(claim.proof_id.as_str())
                .and_modify(|found| *found = None)
                .or_insert(Some(claim));
        }
        ProofLookup(index)
    }
}

/// Finds one raw proof by ID, kind and digest, as `unique_proof` does over a slice: a proof ID
/// held by several claims (`None`) is non-unique, and a kind or digest mismatch is missing.
pub(crate) struct ProofLookup<'a>(BTreeMap<&'a str, Option<&'a RawProofIdentity>>);
impl<'a> ProofLookup<'a> {
    pub(crate) fn lookup(
        &self,
        id: &str,
        kind: &str,
        digest: &str,
    ) -> Result<&'a RawProofIdentity, ScipCaptureError> {
        let claim = self
            .0
            .get(id)
            .ok_or(ScipCaptureError::MissingProof)?
            .ok_or(ScipCaptureError::NonUniqueProof)?;
        if claim.record_kind != kind || claim.fact_digest != digest {
            return Err(ScipCaptureError::MissingProof);
        }
        Ok(claim)
    }
}

#[derive(Debug, Clone)]
pub struct CapturedScipInventory {
    pub artifact_hashes: Vec<String>,
    pub decoder_version: String,
    pub source_manifest_hash: String,
    pub completion: Completion,
    pub exclusions: Vec<Exclusion>,
    /// Documents the decoder admitted under a per-document cap ([`DocumentCap`]).
    pub capped_documents: CappedDocuments,
    pub claims: Vec<CapturedClaim>,
}
impl CapturedScipInventory {
    pub(crate) fn proof_identities(&self) -> ProofInventory {
        ProofInventory {
            artifact_hashes: self.artifact_hashes.clone(),
            source_manifest_hash: self.source_manifest_hash.clone(),
            capped_documents: self.capped_documents.clone(),
            claims: self
                .claims
                .iter()
                .map(|claim| RawProofIdentity {
                    proof_id: claim.captured_proof_id.clone(),
                    record_kind: claim.envelope.record_kind.clone(),
                    fact_digest: claim.fact_digest.clone(),
                    language: claim.language.clone(),
                    path: claim.envelope.relative_path.clone(),
                    artifact_hash: claim.artifact_hash.clone(),
                })
                .collect(),
        }
    }
    pub fn lookup(
        &self,
        proof_id: &str,
        record_kind: &str,
        fact_digest: &str,
    ) -> Result<&CapturedClaim, ScipCaptureError> {
        unique_proof(&self.claims, proof_id, record_kind, fact_digest, |claim| {
            (
                &claim.captured_proof_id,
                &claim.envelope.record_kind,
                &claim.fact_digest,
            )
        })
    }
}

// Wire grammar is the locked generated scip 0.10.0 message graph, not a generic protobuf guess.
// Each scalar and each packed element consumes the common budget before generated allocation.
#[derive(Clone, Copy)]
enum Wire {
    Scalar,
    Bytes,
    Packed,
    Message(Node),
}
#[derive(Clone, Copy)]
enum Node {
    #[cfg(test)]
    Index,
    Metadata,
    Tool,
    Document,
    Symbol,
    Relationship,
    Signature,
    Occurrence,
    Diagnostic,
    Single,
    Multi,
}
fn field(node: Node, n: u64) -> Option<(u8, Wire, usize)> {
    use Node::*;
    use Wire::*;
    Some(match (node, n) {
        #[cfg(test)]
        (Index, 1) => (2, Message(Metadata), 1),
        #[cfg(test)]
        (Index, 2) => (2, Message(Document), 1000),
        #[cfg(test)]
        (Index, 3) => (2, Message(Symbol), 10000),
        (Metadata, 1) => (0, Scalar, 1),
        (Metadata, 2) => (2, Message(Tool), 1),
        (Metadata, 3) => (2, Bytes, 1),
        (Metadata, 4) => (0, Scalar, 1),
        (Tool, 1 | 2) => (2, Bytes, 1),
        (Tool, 3) => (2, Bytes, 1000000),
        (Document, 1 | 4 | 5) => (2, Bytes, 1),
        (Document, 2) => (2, Message(Occurrence), 10000),
        (Document, 3) => (2, Message(Symbol), 10000),
        (Document, 6) => (0, Scalar, 1),
        (Symbol, 1 | 6 | 8) => (2, Bytes, 1),
        (Symbol, 3) => (2, Bytes, 1000000),
        (Symbol, 4) => (2, Message(Relationship), 20000),
        (Symbol, 5) => (0, Scalar, 1),
        (Symbol, 7) => (2, Message(Signature), 1),
        (Relationship, 1) => (2, Bytes, 1),
        (Relationship, 2..=5) => (0, Scalar, 1),
        // Signature `relative_path` (scip-java writes it) and `position_encoding`
        // (rust-analyzer writes it) are presentation-only, like `language` and `text` (owner
        // rulings 2026-10-29, #439, and 2026-10-28, #394); see `signature_unknown`.
        (Signature, 1 | 4 | 5) => (2, Bytes, 1),
        (Signature, 6) => (0, Scalar, 1),
        (Signature, 2) => (2, Message(Occurrence), 10000),
        (Occurrence, 1 | 7) => (0, Packed, 1000000),
        (Occurrence, 2) => (2, Bytes, 1),
        (Occurrence, 3 | 5) => (0, Scalar, 1),
        (Occurrence, 4) => (2, Bytes, 1000000),
        (Occurrence, 6) => (2, Message(Diagnostic), 1000),
        (Occurrence, 8 | 10) => (2, Message(Single), 1),
        (Occurrence, 9 | 11) => (2, Message(Multi), 1),
        (Diagnostic, 1) => (0, Scalar, 1),
        (Diagnostic, 2..=4) => (2, Bytes, 1),
        (Diagnostic, 5) => (0, Packed, 1000000),
        (Single, 1..=3) => (0, Scalar, 1),
        (Multi, 1..=4) => (0, Scalar, 1),
        _ => return None,
    })
}
fn varint(b: &[u8], i: &mut usize) -> Result<u64, ScipCaptureError> {
    let mut v = 0u64;
    for shift in (0..=63).step_by(7) {
        let x = *b.get(*i).ok_or_else(|| invalid("truncated varint"))?;
        *i += 1;
        if shift == 63 && x > 1 {
            return Err(invalid("varint overflow"));
        }
        v |= u64::from(x & 127) << shift;
        if x < 128 {
            return Ok(v);
        }
    }
    Err(invalid("varint overflow"))
}
struct WireBudget<'a> {
    fields: u64,
    relationships: u64,
    candidates: u64,
    /// Occurrences of the `Document` units walked since the caller last reset it. The walk
    /// does not cap them: a document over `document_occurrences` is admitted partially.
    document_occurrences: u64,
    /// The walk is inside a `Document` unit, where a per-document count cap excludes the
    /// document instead of rejecting the artifact (#634).
    in_document: bool,
    /// The `Document` unit walked since the caller last reset it exceeds a per-document count
    /// cap other than its occurrences ([`DOCUMENT_CAP_EXCEEDED`]).
    document_cap_exceeded: bool,
    limits: &'a BatchLimits,
}
impl<'a> WireBudget<'a> {
    fn new(limits: &'a BatchLimits) -> Self {
        WireBudget {
            fields: 0,
            relationships: 0,
            candidates: 0,
            document_occurrences: 0,
            in_document: false,
            document_cap_exceeded: false,
            limits,
        }
    }
    /// A repeated field over its count: a per-document count cap inside a `Document` unit
    /// marks the document excluded and the walk goes on; anything else rejects the artifact.
    fn over_count(&mut self, per_document: bool, name: &str) -> Result<(), ScipCaptureError> {
        if per_document && self.in_document {
            self.document_cap_exceeded = true;
            Ok(())
        } else {
            Err(limit(name))
        }
    }
}
impl Default for WireBudget<'static> {
    fn default() -> Self {
        WireBudget::new(&BatchLimits::PRODUCTION)
    }
}
fn wire(
    b: &[u8],
    node: Node,
    depth: usize,
    budget: &mut WireBudget<'_>,
    flag: &CancelFlag,
) -> Result<(), ScipCaptureError> {
    if depth as u64 > budget.limits.message_depth {
        return Err(limit("message depth"));
    }
    let mut i = 0;
    let mut counts = BTreeMap::<u64, usize>::new();
    let mut ranges = 0u8;
    while i < b.len() {
        budget.fields += 1;
        if budget.fields > budget.limits.wire_fields {
            return Err(limit("wire fields"));
        }
        if budget.fields.is_multiple_of(1024) {
            cancelled(flag)?;
        }
        let tag = varint(b, &mut i)?;
        let n = tag >> 3;
        let wt = (tag & 7) as u8;
        let (expected, kind, max) = field(node, n).ok_or_else(|| invalid("unknown field"))?;
        // A count over 1 is a per-document cap: inside a `Document` unit, exceeding it excludes
        // that document (owner ruling 2026-11-02, #634). A singular field repeated, and every
        // cap outside a document (metadata, external symbols), still reject the artifact.
        let (max, max_name, per_document) = match (node, n) {
            // Bounded by the wire-field and unit budgets; over `document_occurrences` the
            // document is admitted partially (owner ruling 2026-10-28, #394).
            (Node::Document, 2) => (u64::MAX, "document entries", false),
            (Node::Document, 3) => (budget.limits.document_symbols, "document entries", true),
            (Node::Signature, 2) => (
                budget.limits.signature_occurrences,
                "signature occurrences",
                true,
            ),
            _ => (max as u64, "repeated field", max > 1),
        };
        let max = usize::try_from(max).unwrap_or(usize::MAX);
        if wt != expected && !(matches!(kind, Wire::Packed) && wt == 2) {
            return Err(invalid("field wire type"));
        }
        if matches!(node, Node::Occurrence) && matches!(n, 1 | 7 | 8 | 9 | 10 | 11) {
            let mask = if matches!(n, 1 | 8 | 9) { 1 } else { 2 };
            if n >= 8 && ranges & mask != 0 {
                return Err(invalid("competing range tags"));
            }
            if n >= 8 {
                ranges |= mask;
            } else {
                ranges |= if n == 1 { 4 } else { 8 };
            }
            if ranges & 1 != 0 && ranges & 4 != 0 || ranges & 2 != 0 && ranges & 8 != 0 {
                return Err(invalid("mixed legacy/typed range"));
            }
        }
        if wt == 2 && matches!(kind, Wire::Message(_)) {
            if matches!(node, Node::Symbol) && n == 4 {
                budget.relationships += 1;
            }
            if matches!(node, Node::Document) && matches!(n, 2 | 3)
                || matches!(node, Node::Symbol) && n == 4 && depth >= 2
            {
                budget.candidates += 1;
            }
            if matches!(node, Node::Document) && n == 2 {
                budget.document_occurrences += 1;
            }
            if budget.relationships > budget.limits.relationships_per_artifact {
                return Err(limit("relationships"));
            }
            if budget.candidates > budget.limits.candidates {
                return Err(limit("candidate facts"));
            }
        }
        let count = counts.entry(n).or_default();
        if wt == 0 || !matches!(kind, Wire::Packed) {
            *count += 1;
        }
        if *count > max {
            budget.over_count(per_document, max_name)?;
        }
        if wt == 0 {
            varint(b, &mut i)?;
            continue;
        }
        let len = usize::try_from(varint(b, &mut i)?).map_err(|_| limit("field length"))?;
        let end = i
            .checked_add(len)
            .filter(|e| *e <= b.len())
            .ok_or_else(|| invalid("truncated field"))?;
        if matches!(kind, Wire::Packed) {
            let mut k = i;
            while k < end {
                varint(&b[..end], &mut k)?;
                budget.fields += 1;
                *count += 1;
                if budget.fields > budget.limits.wire_fields {
                    return Err(limit("packed values"));
                }
                if *count > max {
                    budget.over_count(per_document, "packed values")?;
                }
                if budget.fields.is_multiple_of(1024) {
                    cancelled(flag)?;
                }
            }
        } else if let Wire::Message(child) = kind {
            wire(&b[i..end], child, depth + 1, budget, flag)?;
        }
        i = end;
    }
    Ok(())
}
fn known<T: protobuf::Enum>(e: protobuf::EnumOrUnknown<T>) -> Result<i32, ScipCaptureError> {
    e.enum_value()
        .map(|_| e.value())
        .map_err(|_| invalid("unknown enum"))
}
fn unknown(f: &protobuf::SpecialFields) -> Result<(), ScipCaptureError> {
    if f.unknown_fields().iter().next().is_some() {
        Err(invalid("unknown protobuf field"))
    } else {
        Ok(())
    }
}
/// The pinned `scip` crate's `Signature` message omits the SCIP `Document.relative_path`
/// (field 1) that scip-java writes and the `Document.position_encoding` (field 6) that
/// rust-analyzer writes on `signature_documentation`, so both parse as unknown fields. Both are
/// presentation-only (owner rulings 2026-10-29, #439, and 2026-10-28, #394): the path must be
/// well-formed UTF-8 and the encoding a known `PositionEncoding` value, and neither is ever read
/// for identity, binding, path, positions or digests. Every other unknown field still rejects.
fn signature_unknown(f: &protobuf::SpecialFields) -> Result<(), ScipCaptureError> {
    use protobuf::Enum;
    for (number, value) in f.unknown_fields().iter() {
        match (number, value) {
            (1, protobuf::UnknownValueRef::LengthDelimited(path)) => {
                std::str::from_utf8(path).map_err(|_| invalid("signature relative path"))?;
            }
            (6, protobuf::UnknownValueRef::Varint(encoding)) => {
                i32::try_from(encoding)
                    .ok()
                    .and_then(types::PositionEncoding::from_i32)
                    .ok_or_else(|| invalid("signature position encoding"))?;
            }
            _ => return Err(invalid("unknown protobuf field")),
        }
    }
    Ok(())
}
fn range(
    o: &types::Occurrence,
    enclosing: bool,
    positions: &mut Vec<(i32, i32)>,
    lenient: bool,
) -> Result<Value, ScipCaptureError> {
    let legacy = if enclosing {
        &o.enclosing_range
    } else {
        &o.range
    };
    let typed = if enclosing {
        o.typed_enclosing_range
            .as_ref()
            .map(|r| {
                Ok(match r {
                    occurrence::Typed_enclosing_range::SingleLineEnclosingRange(v) => {
                        vec![v.line, v.start_character, v.end_character]
                    }
                    occurrence::Typed_enclosing_range::MultiLineEnclosingRange(v) => {
                        vec![v.start_line, v.start_character, v.end_line, v.end_character]
                    }
                    _ => return Err(invalid("unknown enclosing range variant")),
                })
            })
            .transpose()?
    } else {
        o.typed_range
            .as_ref()
            .map(|r| {
                Ok(match r {
                    occurrence::Typed_range::SingleLineRange(v) => {
                        vec![v.line, v.start_character, v.end_character]
                    }
                    occurrence::Typed_range::MultiLineRange(v) => {
                        vec![v.start_line, v.start_character, v.end_line, v.end_character]
                    }
                    _ => return Err(invalid("unknown primary range variant")),
                })
            })
            .transpose()?
    };
    if !legacy.is_empty() && typed.is_some() {
        return Err(invalid("mixed ranges"));
    }
    let (values, shape) = if !legacy.is_empty() {
        (legacy.as_slice(), 0)
    } else if let Some(ref t) = typed {
        (t.as_slice(), if t.len() == 3 { 1 } else { 2 })
    } else if enclosing {
        return Ok(Value::Null);
    } else {
        return Err(invalid("missing range"));
    };
    if !matches!(values.len(), 3 | 4) || values.iter().any(|v| *v < 0) {
        return Err(invalid("range components"));
    }
    let (sl, sc, el, ec) = if values.len() == 3 {
        (values[0], values[1], values[0], values[2])
    } else {
        (values[0], values[1], values[2], values[3])
    };
    // A reversed range excludes its document (#652); `range_problem` found it first.
    if (sl, sc) > (el, ec) && !lenient {
        return Err(invalid("reversed range"));
    }
    positions.push((sl, sc));
    positions.push((el, ec));
    Ok(match shape {
        0 => json!({"encoding":"legacy","values":values}),
        1 => json!({"encoding":"singleLine","line":sl,"startCharacter":sc,"endCharacter":ec}),
        _ => {
            json!({"encoding":"multiLine","startLine":sl,"startCharacter":sc,"endLine":el,"endCharacter":ec})
        }
    })
}
#[cfg(test)]
type PositionWalkHook = Box<dyn Fn(&CancelFlag)>;
#[cfg(test)]
thread_local! {static POSITION_WALK_HOOK: std::cell::RefCell<Option<PositionWalkHook>> = std::cell::RefCell::new(None);}
#[cfg(test)]
fn position_walk_hook(flag: &CancelFlag) {
    POSITION_WALK_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow().as_ref() {
            hook(flag);
        }
    });
}
#[cfg(test)]
thread_local! { static POSITION_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(crate) fn take_position_work() -> usize {
    POSITION_WORK.with(|work| work.replace(0))
}
fn next_newline(bytes: &[u8], flag: &CancelFlag) -> Result<Option<usize>, ScipCaptureError> {
    for (chunk_index, chunk) in bytes.chunks(65536).enumerate() {
        cancelled(flag)?;
        let newline = chunk.iter().position(|b| *b == b'\n');
        #[cfg(test)]
        POSITION_WORK.with(|work| work.set(work.get() + newline.map_or(chunk.len(), |at| at + 1)));
        if let Some(at) = newline {
            return Ok(Some(chunk_index * 65536 + at));
        }
    }
    Ok(None)
}
pub(crate) fn source_byte_offsets(
    text: &str,
    enc: u8,
    positions: &mut Vec<(i32, i32)>,
    flag: &CancelFlag,
) -> Result<BTreeMap<(i32, i32), usize>, ScipCaptureError> {
    cancelled(flag)?;
    if !matches!(enc, 1..=3) {
        return Err(invalid("position encoding"));
    }
    positions.sort_unstable();
    positions.dedup();
    let bytes = text.as_bytes();
    let mut current_line = 0usize;
    let mut line_start = 0usize;
    let mut loaded_line = None;
    let mut chars = "".chars();
    let mut measured = 0usize;
    let mut byte_offset = 0usize;
    let mut steps = 0usize;
    let mut result = BTreeMap::new();
    for &(line, column) in positions.iter() {
        if line < 0 || column < 0 {
            return Err(invalid("negative position"));
        }
        let target = line as usize;
        if loaded_line != Some(target) {
            while current_line < target {
                let newline = next_newline(&bytes[line_start..], flag)?
                    .ok_or_else(|| invalid("range line outside source"))?;
                line_start += newline + 1;
                current_line += 1;
            }
            let end = match next_newline(&bytes[line_start..], flag)? {
                Some(offset) => {
                    let end = line_start + offset;
                    if end > line_start && bytes[end - 1] == b'\r' {
                        end - 1
                    } else {
                        end
                    }
                }
                None => bytes.len(),
            };
            chars = text[line_start..end].chars();
            measured = 0;
            byte_offset = line_start;
            loaded_line = Some(target);
        }
        while measured < column as usize {
            let c = chars
                .next()
                .ok_or_else(|| invalid("range column outside source"))?;
            steps += 1;
            #[cfg(test)]
            POSITION_WORK.with(|work| work.set(work.get() + c.len_utf8()));
            if steps.is_multiple_of(1024) {
                #[cfg(test)]
                position_walk_hook(flag);
                cancelled(flag)?;
            }
            measured += match enc {
                1 => c.len_utf8(),
                2 => c.len_utf16(),
                3 => 1,
                _ => unreachable!(),
            };
            byte_offset += c.len_utf8();
        }
        if measured != column as usize {
            return Err(invalid("split codepoint"));
        }
        result.insert((line, column), byte_offset);
    }
    Ok(result)
}
fn validate_positions(
    text: &str,
    enc: u8,
    positions: &mut Vec<(i32, i32)>,
    flag: &CancelFlag,
) -> Result<(), ScipCaptureError> {
    source_byte_offsets(text, enc, positions, flag).map(|_| ())
}
#[cfg(test)]
fn check_position(text: &str, line: i32, column: i32, enc: u8) -> Result<(), ScipCaptureError> {
    validate_positions(
        text,
        enc,
        &mut vec![(line, column)],
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
}

/// Validates one occurrence and pushes its range endpoints. `lenient` admits a reversed range,
/// for a document already excluded under [`DocumentCap::InvalidRange`].
fn occurrence(
    o: &types::Occurrence,
    positions: &mut Vec<(i32, i32)>,
    lenient: bool,
) -> Result<Value, ScipCaptureError> {
    unknown(&o.special_fields)?;
    let syntax = known(o.syntax_kind)?;
    if o.symbol_roles < 0 || o.symbol_roles & !0x7f != 0 {
        return Err(invalid("symbol roles"));
    }
    for d in &o.diagnostics {
        unknown(&d.special_fields)?;
        known(d.severity)?;
        for tag in &d.tags {
            known(*tag)?;
        }
    }
    // `wire` bounds the diagnostics count before any parse (#634).
    if let Some(r) = &o.typed_range {
        match r {
            occurrence::Typed_range::SingleLineRange(v) => unknown(&v.special_fields)?,
            occurrence::Typed_range::MultiLineRange(v) => unknown(&v.special_fields)?,
            _ => return Err(invalid("unknown primary range variant")),
        }
    }
    if let Some(r) = &o.typed_enclosing_range {
        match r {
            occurrence::Typed_enclosing_range::SingleLineEnclosingRange(v) => {
                unknown(&v.special_fields)?
            }
            occurrence::Typed_enclosing_range::MultiLineEnclosingRange(v) => {
                unknown(&v.special_fields)?
            }
            _ => return Err(invalid("unknown enclosing range variant")),
        }
    }
    Ok(
        json!({"range":range(o,false,positions,lenient)?,"enclosingRange":range(o,true,positions,lenient)?,"symbol":o.symbol,"symbolRoles":o.symbol_roles,"syntaxKind":syntax}),
    )
}
// Conservative envelope expansion bound for one document unit, before any relationship JSON or
// canonical bytes exist. Strings may be copied into JSON and canonical output; fixed overhead
// covers keys/escaping.
fn reserve_claim(total: &mut usize, max: usize, strings: &[&str]) -> Result<(), ScipCaptureError> {
    let bytes = strings
        .iter()
        .try_fold(512usize, |size, text| {
            size.checked_add(text.len().checked_mul(6)?)
        })
        .ok_or_else(|| limit("unit expansion"))?;
    *total = total
        .checked_add(bytes)
        .filter(|v| *v <= max)
        .ok_or_else(|| limit("unit expansion"))?;
    Ok(())
}
fn reserve_document(
    d: &types::Document,
    max: usize,
    flag: &CancelFlag,
) -> Result<usize, ScipCaptureError> {
    let mut total = 0usize;
    let mut traversed = 0usize;
    for o in &d.occurrences {
        reserve_claim(&mut total, max, &[&d.relative_path, &d.language, &o.symbol])?;
        traversed += 1;
        if traversed.is_multiple_of(1024) {
            cancelled(flag)?;
        }
    }
    for s in &d.symbols {
        reserve_claim(
            &mut total,
            max,
            &[
                &d.relative_path,
                &d.language,
                &s.symbol,
                &s.display_name,
                &s.enclosing_symbol,
            ],
        )?;
        for r in &s.relationships {
            reserve_claim(
                &mut total,
                max,
                &[&d.relative_path, &d.language, &s.symbol, &r.symbol],
            )?;
            traversed += 1;
            if traversed.is_multiple_of(1024) {
                cancelled(flag)?;
            }
        }
    }
    Ok(total)
}
fn symbol(
    s: &types::SymbolInformation,
    enc: u8,
    flag: &CancelFlag,
    traversed: &mut usize,
    emit_relationships: bool,
) -> Result<(Value, Vec<Value>), ScipCaptureError> {
    unknown(&s.special_fields)?;
    let kind = known(s.kind)?;
    if let Some(sig) = s.signature_documentation.as_ref() {
        // `wire` bounds the occurrence count before any parse (#634).
        signature_unknown(&sig.special_fields)?;
        let mut positions = Vec::with_capacity(sig.occurrences.len() * 4);
        for o in &sig.occurrences {
            *traversed += 1;
            if (*traversed).is_multiple_of(1024) {
                cancelled(flag)?;
            }
            occurrence(o, &mut positions, false)?;
        }
        validate_positions(&sig.text, enc, &mut positions, flag)?;
    }
    let mut relationships = Vec::new();
    for r in &s.relationships {
        *traversed += 1;
        if (*traversed).is_multiple_of(1024) {
            cancelled(flag)?;
        }
        unknown(&r.special_fields)?;
        if emit_relationships {
            relationships.push(json!({"sourceSymbol":s.symbol,"targetSymbol":r.symbol,"isReference":r.is_reference,"isImplementation":r.is_implementation,"isTypeDefinition":r.is_type_definition,"isDefinition":r.is_definition}));
        }
    }
    Ok((
        json!({"symbol":s.symbol,"kind":kind,"displayName":s.display_name,"enclosingSymbol":s.enclosing_symbol}),
        relationships,
    ))
}
fn make_claim(
    artifact: &AuthenticatedArtifact<'_>,
    d: &types::Document,
    language: &str,
    kind: &str,
    raw: Value,
) -> CapturedClaim {
    let envelope = CapturedScipFactV1 {
        format_version: 1,
        record_kind: kind.into(),
        relative_path: d.relative_path.clone(),
        position_encoding: d.position_encoding.value() as u8,
        raw,
        cargo_target: (language == "rust")
            .then(|| artifact.cargo_targets.target_of(&d.relative_path))
            .flatten()
            .cloned(),
    };
    let mut value = json!({"formatVersion":1,"recordKind":kind,"relativePath":d.relative_path,"positionEncoding":envelope.position_encoding,"raw":envelope.raw});
    if let Some(target) = &envelope.cargo_target {
        value["cargoTarget"] = CapturedScipFactV1::cargo_target_json(target);
    }
    let bytes = canonical(&value);
    let fact = digest(b"trellis.semantic-fact.v1\0", &bytes);
    let proof = digest(
        b"trellis.capture-proof.v1\0",
        &canonical(
            &json!({"producerId":artifact.producer_id,"artifactHash":artifact.artifact_hash,"sourceSetId":artifact.source_set_id,"sourceRevision":artifact.source_revision,"language":language,"relativePath":d.relative_path,"factDigest":fact}),
        ),
    );
    CapturedClaim {
        envelope,
        canonical_bytes: bytes,
        fact_digest: fact,
        captured_proof_id: proof,
        language: language.to_owned(),
        artifact_hash: artifact.artifact_hash.to_owned(),
    }
}

/// Authenticated context of one artifact, supplied by the caller before any byte is decoded.
pub struct AuthenticatedArtifact<'a> {
    pub artifact_hash: &'a str,
    pub producer_id: &'a str,
    pub producer_version: &'a str,
    pub source_set_id: &'a str,
    pub source_revision: &'a str,
    pub position_encoding: u8,
    pub protocol_version: i32,
    pub text_document_encoding: i32,
    pub project_root_uri: &'a str,
    /// Package roots that bind scip-typescript module markers (#443).
    pub module_roots: &'a ModuleRoots,
    /// The Cargo target of each Rust document, its symbol namespace (#394).
    pub cargo_targets: &'a CargoTargets,
    /// Source documents assigned to this artifact; every one must appear exactly once.
    pub assigned_documents: u64,
}
/// Identity of the document unit being decoded, handed to the sink before its claims.
pub struct UnitDocument<'a> {
    pub path: &'a str,
    pub language: &'a str,
    pub wire_digest: &'a str,
    /// The per-document cap the document was admitted under, if any.
    pub cap: Option<DocumentCap>,
}

/// Coverage diagnostic of a document admitted partially because it has more occurrences than
/// the per-document cap (owner ruling 2026-10-28, #394). Such a document keeps its symbol
/// information, relationships and definition occurrences, and drops every other occurrence;
/// if its definitions alone exceed the cap it keeps no occurrence. Every occurrence is still
/// fully validated, so a malformed one rejects the artifact as before.
pub const OCCURRENCE_CAP_EXCEEDED: &str = "occurrence_cap_exceeded";

/// Coverage diagnostic of a document admitted with no claims because it exceeds another
/// per-document wire cap: more symbols than `document_symbols`, a symbol with more signature
/// occurrences than `signature_occurrences`, or any other repeated field of the document over
/// its decoder table count (owner ruling 2026-11-02, #634). The document is still walked and
/// every occurrence and symbol fully validated, one at a time, so a malformed one rejects the
/// artifact as below the cap; the rest of the artifact imports. Aggregate caps still reject it.
pub const DOCUMENT_CAP_EXCEEDED: &str = "document_cap_exceeded";

/// Coverage diagnostic of a document admitted with no claims because one of its occurrences has
/// a reversed range, or an empty primary or enclosing range, and is not a module marker (owner
/// ruling 2026-11-03, #652). Every occurrence and symbol is still fully validated, so any other
/// malformation rejects the artifact; the rest of the artifact imports.
pub const INVALID_RANGE: &str = "invalid_range";

/// The per-document cap a document was admitted under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DocumentCap {
    /// [`OCCURRENCE_CAP_EXCEEDED`]: only definition occurrences are dropped.
    Occurrences,
    /// [`DOCUMENT_CAP_EXCEEDED`]: the document keeps no claim.
    Document,
    /// [`INVALID_RANGE`]: the document keeps no claim.
    InvalidRange,
}
impl DocumentCap {
    /// The coverage diagnostic of a document admitted under this cap.
    pub fn diagnostic(self) -> &'static str {
        match self {
            Self::Occurrences => OCCURRENCE_CAP_EXCEEDED,
            Self::Document => DOCUMENT_CAP_EXCEEDED,
            Self::InvalidRange => INVALID_RANGE,
        }
    }
    /// Whether the document is admitted with no claims, so it proves nothing absent.
    pub fn excludes(self) -> bool {
        self != Self::Occurrences
    }
    /// The stored `capped` value: 0 for none, 1 to 3 for these caps.
    pub fn code(cap: Option<Self>) -> i64 {
        match cap {
            None => 0,
            Some(Self::Occurrences) => 1,
            Some(Self::Document) => 2,
            Some(Self::InvalidRange) => 3,
        }
    }
    /// The cap of a stored `capped` value.
    pub fn from_code(code: i64) -> Option<Self> {
        match code {
            1 => Some(Self::Occurrences),
            2 => Some(Self::Document),
            3 => Some(Self::InvalidRange),
            _ => None,
        }
    }
}

/// `(language, path)` of the documents admitted under a per-document cap, with that cap.
pub type CappedDocuments = BTreeMap<(String, String), DocumentCap>;
/// One assigned document's authenticated source, returned by the caller's resolver.
pub struct DocumentSource<'s> {
    pub language: String,
    pub bytes: Cow<'s, [u8]>,
    pub content_hash: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactSummary {
    pub documents: u64,
    pub external_symbols: u64,
    pub relationships: u64,
}
/// Whether Trellis admitted this source without reading it (#576). The producer read the real
/// file, so its positions can't be checked against the captured placeholder.
fn unread(source: &DocumentSource<'_>) -> bool {
    std::str::from_utf8(&source.bytes)
        .is_ok_and(|text| crate::skipped_source::skipped(&source.language, text).is_some())
}
/// Receiver of one artifact's claims, one document unit at a time.
pub trait ClaimSink {
    fn begin_document(&mut self, doc: &UnitDocument<'_>) -> Result<(), ScipCaptureError>;
    fn claim(&mut self, claim: CapturedClaim) -> Result<(), ScipCaptureError>;
    fn end_document(&mut self) -> Result<(), ScipCaptureError>;
}
/// Collects claims in memory; `finish` applies the language/path/bytes sort and dedup and the
/// digest-collision check that the whole-Index decoder applied.
#[derive(Default)]
pub struct VecSink {
    claims: Vec<CapturedClaim>,
    paths: BTreeSet<String>,
    capped: CappedDocuments,
}
impl VecSink {
    /// The documents admitted under a per-document cap so far.
    pub fn capped_documents(&self) -> &CappedDocuments {
        &self.capped
    }
    pub fn finish(mut self) -> Result<Vec<CapturedClaim>, ScipCaptureError> {
        self.claims.sort_by(|a, b| {
            a.language
                .as_bytes()
                .cmp(b.language.as_bytes())
                .then(
                    a.envelope
                        .relative_path
                        .as_bytes()
                        .cmp(b.envelope.relative_path.as_bytes()),
                )
                .then(a.canonical_bytes.cmp(&b.canonical_bytes))
        });
        self.claims
            .dedup_by(|a, b| a.language == b.language && a.canonical_bytes == b.canonical_bytes);
        let mut digests = BTreeMap::new();
        for claim in &self.claims {
            if let Some(bytes) = digests.insert(&claim.fact_digest, &claim.canonical_bytes)
                && bytes != &claim.canonical_bytes
            {
                return Err(ScipCaptureError::DigestCollision);
            }
        }
        Ok(self.claims)
    }
}
impl ClaimSink for VecSink {
    fn begin_document(&mut self, doc: &UnitDocument<'_>) -> Result<(), ScipCaptureError> {
        if !self.paths.insert(doc.path.to_string()) {
            return Err(invalid("duplicate document path"));
        }
        if let Some(cap) = doc.cap {
            self.capped
                .insert((doc.language.to_owned(), doc.path.to_owned()), cap);
        }
        Ok(())
    }
    fn claim(&mut self, claim: CapturedClaim) -> Result<(), ScipCaptureError> {
        self.claims.push(claim);
        Ok(())
    }
    fn end_document(&mut self) -> Result<(), ScipCaptureError> {
        Ok(())
    }
}

struct ArtifactReader<R> {
    reader: BufReader<R>,
    remaining: u64,
    fault: Option<std::io::Error>,
}
impl<R: Read> ArtifactReader<R> {
    fn read_exact(&mut self, bytes: &mut [u8]) -> Result<(), ScipCaptureError> {
        self.reader.read_exact(bytes).map_err(|error| {
            if error.kind() != std::io::ErrorKind::UnexpectedEof {
                self.fault = Some(error);
            }
            invalid("artifact read")
        })
    }

    fn byte(&mut self, truncated: &str) -> Result<u8, ScipCaptureError> {
        if self.remaining == 0 {
            return Err(invalid(truncated));
        }
        let mut byte = [0u8];
        self.read_exact(&mut byte)?;
        self.remaining -= 1;
        Ok(byte[0])
    }
    fn varint(&mut self) -> Result<u64, ScipCaptureError> {
        let mut v = 0u64;
        for shift in (0..=63).step_by(7) {
            let x = self.byte("truncated varint")?;
            if shift == 63 && x > 1 {
                return Err(invalid("varint overflow"));
            }
            v |= u64::from(x & 127) << shift;
            if x < 128 {
                return Ok(v);
            }
        }
        Err(invalid("varint overflow"))
    }
}
/// The one live decode unit. Creation and drop keep the live count and high-water marks.
struct UnitBuffer<'c> {
    bytes: Vec<u8>,
    counters: &'c mut BatchCounters,
}
impl<'c> UnitBuffer<'c> {
    fn new(counters: &'c mut BatchCounters, len: usize) -> Self {
        counters.live_unit_buffers += 1;
        counters.live_unit_buffers_high_water = counters
            .live_unit_buffers_high_water
            .max(counters.live_unit_buffers);
        counters.unit_buffer_high_water_bytes =
            counters.unit_buffer_high_water_bytes.max(len as u64);
        UnitBuffer {
            bytes: vec![0; len],
            counters,
        }
    }
}
impl Drop for UnitBuffer<'_> {
    fn drop(&mut self) {
        self.counters.live_unit_buffers -= 1;
    }
}

/// A scip-typescript module-definition marker that does not name this document under its
/// authenticated package or project root. Only an empty one is invalid; the caller checks.
fn unresolved_module_marker(
    artifact: &AuthenticatedArtifact<'_>,
    language: &str,
    path: &str,
    o: &types::Occurrence,
) -> bool {
    if !matches!(language, "javascript" | "typescript") || o.symbol_roles & 1 == 0 {
        return false;
    }
    let Ok(symbol) = scip::symbol::parse_symbol(&o.symbol) else {
        return false;
    };
    module_roots::typescript_module_path(&symbol).is_some()
        && !artifact.module_roots.names_document(&symbol, path)
}

/// Whether a definition occurrence of `symbol` has the shape of a pinned producer's raw
/// file/module marker for the document at `path`: the only occurrence whose range may be empty.
/// A scip-typescript marker must also name its document under its authenticated root
/// ([`unresolved_module_marker`]); that binding is checked separately.
pub(crate) fn module_marker_shape(language: &str, path: &str, symbol: &str) -> bool {
    use types::descriptor::Suffix;
    let Ok(symbol) = scip::symbol::parse_symbol(symbol) else {
        return false;
    };
    let package =
        |descriptor: &types::Descriptor| descriptor.suffix.enum_value() == Ok(Suffix::Package);
    match (language, symbol.scheme.as_str()) {
        ("javascript" | "typescript", "scip-typescript") => {
            module_roots::typescript_module_path(&symbol).is_some()
        }
        ("python", "scip-python") => {
            symbol
                .descriptors
                .split_last()
                .is_some_and(|(last, prefix)| {
                    last.suffix.enum_value() == Ok(Suffix::Meta)
                        && last.name == "__init__"
                        && !prefix.is_empty()
                        && prefix.iter().all(package)
                        && path
                            .strip_suffix("/__init__.py")
                            .or_else(|| path.strip_suffix(".py"))
                            .is_some_and(|path| {
                                prefix
                                    .iter()
                                    .map(|descriptor| descriptor.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(".")
                                    == path.replace('/', ".")
                            })
                })
        }
        _ => false,
    }
}

/// The raw values of an occurrence's primary or enclosing range, when they have a valid shape.
/// A malformed shape is left to [`occurrence`], which rejects it.
fn raw_range_values(o: &types::Occurrence, enclosing: bool) -> Option<[i32; 4]> {
    let (legacy, typed) = if enclosing {
        let typed = match &o.typed_enclosing_range {
            Some(occurrence::Typed_enclosing_range::SingleLineEnclosingRange(v)) => {
                Some(vec![v.line, v.start_character, v.end_character])
            }
            Some(occurrence::Typed_enclosing_range::MultiLineEnclosingRange(v)) => Some(vec![
                v.start_line,
                v.start_character,
                v.end_line,
                v.end_character,
            ]),
            _ => None,
        };
        (&o.enclosing_range, typed)
    } else {
        let typed = match &o.typed_range {
            Some(occurrence::Typed_range::SingleLineRange(v)) => {
                Some(vec![v.line, v.start_character, v.end_character])
            }
            Some(occurrence::Typed_range::MultiLineRange(v)) => Some(vec![
                v.start_line,
                v.start_character,
                v.end_line,
                v.end_character,
            ]),
            _ => None,
        };
        (&o.range, typed)
    };
    let values = if legacy.is_empty() {
        typed?
    } else {
        legacy.clone()
    };
    match values[..] {
        [line, start, end] => Some([line, start, line, end]),
        [start_line, start, end_line, end] => Some([start_line, start, end_line, end]),
        _ => None,
    }
}

/// The first range that makes an occurrence's document [`DocumentCap::InvalidRange`] (owner
/// ruling 2026-11-03, #652): a reversed primary or enclosing range, or an empty primary or
/// enclosing range on an occurrence that is not a module marker by shape. An empty marker that
/// does not name its document stays an artifact error (#443, `check_module_markers`).
fn range_problem(o: &types::Occurrence, language: &str, path: &str) -> Option<[i32; 4]> {
    let marker = o.symbol_roles & 1 != 0 && module_marker_shape(language, path, &o.symbol);
    [false, true]
        .into_iter()
        .filter_map(|enclosing| raw_range_values(o, enclosing))
        .find(|values| {
            let (start, end) = ((values[0], values[1]), (values[2], values[3]));
            start > end || start == end && !marker
        })
}

/// Opt-in (`TRELLIS_INDEX_DIAGNOSTICS=1`), best-effort: names the document an invalid range
/// excluded and its first such occurrence. The request row never carries it (#445).
fn report_invalid_range(language: &str, path: &str, ordinal: usize, values: [i32; 4]) {
    if std::env::var("TRELLIS_INDEX_DIAGNOSTICS").as_deref() == Ok("1") {
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr(),
            "index-phase semantic_invalid_range language={language} path={path:?} occurrence={ordinal} range={values:?}"
        );
    }
}

type FieldSpan = (u64, std::ops::Range<usize>, std::ops::Range<usize>);

/// The first top-level field of a message already accepted by `wire`: its number, the
/// field's bytes and its payload (empty for a varint).
fn next_field(bytes: &[u8], start: usize) -> Result<FieldSpan, ScipCaptureError> {
    let mut i = start;
    let tag = varint(bytes, &mut i)?;
    let payload = if tag & 7 == 0 {
        varint(bytes, &mut i)?;
        i..i
    } else {
        let len =
            usize::try_from(varint(bytes, &mut i)?).map_err(|_| invalid("truncated field"))?;
        let end = i
            .checked_add(len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| invalid("truncated field"))?;
        let payload = i..end;
        i = end;
        payload
    };
    Ok((tag >> 3, start..i, payload))
}

/// `relative_path` and `language` of a `Document` unit accepted by `wire`, read without
/// parsing its occurrences.
fn document_identity(bytes: &[u8]) -> Result<(String, String), ScipCaptureError> {
    let (mut path, mut language) = (String::new(), String::new());
    let mut at = 0;
    while at < bytes.len() {
        let (number, span, payload) = next_field(bytes, at)?;
        at = span.end;
        let target = match number {
            1 => &mut path,
            4 => &mut language,
            _ => continue,
        };
        *target = std::str::from_utf8(&bytes[payload])
            .map_err(|_| invalid("protobuf parse"))?
            .to_owned();
    }
    Ok((path, language))
}

/// The `symbol_roles` of an `Occurrence` accepted by `wire`; the last value wins, as in a parse.
fn occurrence_roles(bytes: &[u8]) -> Result<u64, ScipCaptureError> {
    let mut roles = 0;
    let mut at = 0;
    while at < bytes.len() {
        let (number, span, _) = next_field(bytes, at)?;
        if number == 3 {
            let mut i = span.start;
            varint(bytes, &mut i)?;
            roles = varint(bytes, &mut i)?;
        }
        at = span.end;
    }
    Ok(roles)
}

/// Positions validated per batch while a capped document's occurrences are checked, so the
/// pending position list stays bounded however many occurrences the document has.
const CAPPED_POSITION_BATCH: usize = 1 << 16;

/// Elements of capacity charged per wire field of an entry: a generated `Vec` grows to at least
/// four slots, then by doubling, so its capacity is at most four times its length.
const ENTRY_CAPACITY_FACTOR: u64 = 4;

/// Fails `limit_exceeded: unit expansion` unless one entry of a capped document fits the batch
/// live budget once parsed. Inside an excluded document an entry's own repeated fields may be
/// over their counts (#634), so the generated parse is bounded here, before it allocates. Every
/// field the walk counts becomes at most one element of a generated `Vec` (a message, string or
/// scalar) or one boxed message, sized at most `per_field`. A growing `Vec` holds at most four
/// times its length in capacity (at least four slots, then doubling), so each field is charged
/// [`ENTRY_CAPACITY_FACTOR`] elements. String and byte contents are charged twice the entry's
/// bytes. The bound is of allocated capacity, not of element counts (#635 review).
fn entry_within_budget(
    bytes: &[u8],
    node: Node,
    limits: &BatchLimits,
    cancel: &CancelFlag,
) -> Result<(), ScipCaptureError> {
    use std::mem::size_of;
    let mut budget = WireBudget::new(limits);
    budget.in_document = true;
    wire(bytes, node, 2, &mut budget, cancel)?;
    let per_field = [
        size_of::<types::Occurrence>(),
        size_of::<types::SymbolInformation>(),
        size_of::<types::Relationship>(),
        size_of::<types::Diagnostic>(),
        size_of::<types::Signature>(),
        size_of::<String>(),
    ]
    .into_iter()
    .max()
    .unwrap_or(0) as u64;
    let bound = budget
        .fields
        .saturating_mul(per_field)
        .saturating_mul(ENTRY_CAPACITY_FACTOR)
        .saturating_add((bytes.len() as u64).saturating_mul(2));
    if bound > limits.unit_expansion_bytes {
        return Err(limit("unit expansion"));
    }
    Ok(())
}

/// Admits an over-cap `Document` unit partially, in place: validates every occurrence exactly
/// as an admitted one is validated, then keeps every other field and only the definition
/// occurrences (none if they alone exceed the cap), in wire order. Under
/// [`DocumentCap::Document`] it also validates every symbol, one at a time, and keeps no
/// occurrence and no symbol. A document with an invalid range ([`range_problem`]) is admitted
/// that way under [`DocumentCap::InvalidRange`] whatever its cap, which it returns. No
/// occurrence becomes a claim here; the caller decodes the kept unit as usual.
#[allow(clippy::too_many_arguments)]
fn cap_document(
    bytes: &mut Vec<u8>,
    artifact: &AuthenticatedArtifact<'_>,
    language: &str,
    path: &str,
    text: Option<&str>,
    cap: DocumentCap,
    limits: &BatchLimits,
    traversed: &mut usize,
    cancel: &CancelFlag,
) -> Result<DocumentCap, ScipCaptureError> {
    let mut cap = cap;
    let mut at = 0;
    let mut ordinal = 0;
    while cap != DocumentCap::InvalidRange && at < bytes.len() {
        let (number, span, payload) = next_field(bytes, at)?;
        at = span.end;
        if number != 2 {
            continue;
        }
        entry_within_budget(&bytes[payload.clone()], Node::Occurrence, limits, cancel)?;
        let o = types::Occurrence::parse_from_bytes(&bytes[payload])
            .map_err(|_| invalid("protobuf parse"))?;
        if let Some(values) = range_problem(&o, language, path) {
            report_invalid_range(language, path, ordinal, values);
            cap = DocumentCap::InvalidRange;
        }
        ordinal += 1;
    }
    let lenient = cap == DocumentCap::InvalidRange;
    let excluded = cap.excludes();
    let occurrence_cap = limits.document_occurrences;
    let mut definitions = 0u64;
    let mut at = 0;
    while !excluded && at < bytes.len() {
        let (number, span, payload) = next_field(bytes, at)?;
        at = span.end;
        if number == 2 && occurrence_roles(&bytes[payload])? & 1 != 0 {
            definitions += 1;
        }
    }
    let keep_definitions = !excluded && definitions <= occurrence_cap;
    let mut positions = Vec::new();
    let mut unresolved_markers = Vec::new();
    let mut write = 0;
    let mut read = 0;
    while read < bytes.len() {
        let (number, span, payload) = next_field(bytes, read)?;
        let keep = if number == 2 {
            *traversed += 1;
            if traversed.is_multiple_of(1024) {
                cancelled(cancel)?;
            }
            entry_within_budget(&bytes[payload.clone()], Node::Occurrence, limits, cancel)?;
            let o = types::Occurrence::parse_from_bytes(&bytes[payload])
                .map_err(|_| invalid("protobuf parse"))?;
            let first = positions.len();
            occurrence(&o, &mut positions, lenient)?;
            if unresolved_module_marker(artifact, language, path, &o) {
                unresolved_markers.push((positions[first], positions[first + 1]));
            }
            if positions.len() >= CAPPED_POSITION_BATCH {
                if let Some(text) = text {
                    validate_positions(text, artifact.position_encoding, &mut positions, cancel)?;
                }
                positions.clear();
            }
            keep_definitions && o.symbol_roles & 1 != 0
        } else if number == 3 && excluded {
            *traversed += 1;
            if traversed.is_multiple_of(1024) {
                cancelled(cancel)?;
            }
            entry_within_budget(&bytes[payload.clone()], Node::Symbol, limits, cancel)?;
            let s = types::SymbolInformation::parse_from_bytes(&bytes[payload])
                .map_err(|_| invalid("protobuf parse"))?;
            symbol(&s, artifact.position_encoding, cancel, traversed, false)?;
            false
        } else {
            true
        };
        read = span.end;
        if keep {
            let len = span.len();
            bytes.copy_within(span, write);
            write += len;
        }
    }
    if let Some(text) = text {
        validate_positions(text, artifact.position_encoding, &mut positions, cancel)?;
        check_module_markers(
            text,
            artifact.position_encoding,
            &unresolved_markers,
            cancel,
        )?;
    }
    bytes.truncate(write);
    Ok(cap)
}

/// The start and end positions of an occurrence range.
type RangeEnds = ((i32, i32), (i32, i32));

/// An unresolved module marker must span source text.
fn check_module_markers(
    text: &str,
    encoding: u8,
    markers: &[RangeEnds],
    cancel: &CancelFlag,
) -> Result<(), ScipCaptureError> {
    if markers.is_empty() {
        return Ok(());
    }
    let mut ends: Vec<_> = markers
        .iter()
        .flat_map(|(start, end)| [*start, *end])
        .collect();
    let offsets = source_byte_offsets(text, encoding, &mut ends, cancel)?;
    if markers
        .iter()
        .any(|(start, end)| offsets.get(start) == offsets.get(end))
    {
        return Err(invalid("empty or reversed SCIP range"));
    }
    Ok(())
}

/// Decode one authenticated artifact as a stream of top-level units. Each unit is length-checked
/// against the unit limit before it is allocated, walked, then parsed and emitted on its own, so
/// one document unit is live at a time. Metadata is merged as `parse_from_bytes` merges repeated
/// fields and is validated at the end of the artifact.
#[allow(clippy::too_many_arguments)]
pub fn decode_streaming<'s, R: Read>(
    reader: R,
    byte_length: u64,
    artifact: &AuthenticatedArtifact<'_>,
    resolve: &mut dyn FnMut(&str, &str) -> Result<DocumentSource<'s>, ScipCaptureError>,
    counters: &mut BatchCounters,
    limits: &BatchLimits,
    sink: &mut dyn ClaimSink,
    cancel: &CancelFlag,
) -> Result<ArtifactSummary, ScipCaptureError> {
    cancelled(cancel)?;
    let mut continuation = ScipDecodeContinuation::new(reader, byte_length, limits)?;
    loop {
        if let Some(summary) =
            continuation.advance(artifact, resolve, counters, limits, sink, cancel)?
        {
            return Ok(summary);
        }
    }
}

/// Owns the original wire cursor and budgets; each advance consumes exactly one top-level
/// metadata, document or external-symbol unit. No unit bytes survive an advance.
pub(crate) struct ScipDecodeContinuation<R> {
    input: ArtifactReader<R>,
    metadata: Option<types::Metadata>,
    documents: u64,
    external_symbols: u64,
    relationships: u64,
    traversed: usize,
}

impl<R: Read> ScipDecodeContinuation<R> {
    pub(crate) fn take_read_fault(&mut self) -> Option<std::io::Error> {
        self.input.fault.take()
    }

    pub(crate) fn new(
        reader: R,
        byte_length: u64,
        limits: &BatchLimits,
    ) -> Result<Self, ScipCaptureError> {
        if byte_length > limits.artifact_bytes {
            return Err(limit("artifact bytes"));
        }
        Ok(Self {
            input: ArtifactReader {
                reader: BufReader::with_capacity(byte_length.clamp(1, 1 << 20) as usize, reader),
                remaining: byte_length,
                fault: None,
            },
            metadata: None,
            documents: 0,
            external_symbols: 0,
            relationships: 0,
            traversed: 0,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn advance<'s>(
        &mut self,
        artifact: &AuthenticatedArtifact<'_>,
        resolve: &mut dyn FnMut(&str, &str) -> Result<DocumentSource<'s>, ScipCaptureError>,
        counters: &mut BatchCounters,
        limits: &BatchLimits,
        sink: &mut dyn ClaimSink,
        cancel: &CancelFlag,
    ) -> Result<Option<ArtifactSummary>, ScipCaptureError> {
        cancelled(cancel)?;
        if self.input.remaining == 0 {
            if let Some(meta) = self.metadata.as_ref() {
                unknown(&meta.special_fields)?;
                if known(meta.version)? != artifact.protocol_version
                    || known(meta.text_document_encoding)? != artifact.text_document_encoding
                    || !meta.project_root.is_empty()
                        && meta.project_root != artifact.project_root_uri
                {
                    return Err(invalid("metadata binding"));
                }
                if let Some(tool) = meta.tool_info.as_ref() {
                    unknown(&tool.special_fields)?;
                    if !tool.name.is_empty() && tool.name != artifact.producer_id
                        || !tool.version.is_empty() && tool.version != artifact.producer_version
                    {
                        return Err(invalid("tool binding"));
                    }
                }
            }
            if self.documents != artifact.assigned_documents {
                return Err(invalid("missing artifact document"));
            }
            return Ok(Some(ArtifactSummary {
                documents: self.documents,
                external_symbols: self.external_symbols,
                relationships: self.relationships,
            }));
        }
        let mut budget = WireBudget::new(limits);
        budget.fields = counters.wire_fields;
        budget.candidates = counters.candidates;
        budget.relationships = self.relationships;
        let max_unit = usize::try_from(limits.unit_expansion_bytes).unwrap_or(usize::MAX);
        cancelled(cancel)?;
        budget.fields += 1;
        if budget.fields > limits.wire_fields {
            return Err(limit("wire fields"));
        }
        let tag = self.input.varint()?;
        let number = tag >> 3;
        if !matches!(number, 1..=3) {
            return Err(invalid("unknown field"));
        }
        if tag & 7 != 2 {
            return Err(invalid("field wire type"));
        }
        let len = self.input.varint()?;
        if len > self.input.remaining {
            return Err(invalid("protobuf parse"));
        }
        if len > limits.unit_expansion_bytes {
            return Err(limit("unit expansion"));
        }
        if number == 3 {
            self.external_symbols += 1;
            if self.external_symbols > limits.external_symbols {
                return Err(limit("index entries"));
            }
        }
        let len = usize::try_from(len).map_err(|_| limit("unit expansion"))?;
        let mut unit = UnitBuffer::new(counters, len);
        self.input.read_exact(&mut unit.bytes)?;
        self.input.remaining -= len as u64;
        let node = match number {
            1 => Node::Metadata,
            2 => Node::Document,
            _ => Node::Symbol,
        };
        budget.document_occurrences = 0;
        budget.in_document = number == 2;
        budget.document_cap_exceeded = false;
        let walked = wire(&unit.bytes, node, 1, &mut budget, cancel);
        unit.counters.wire_fields = budget.fields;
        unit.counters.candidates = budget.candidates;
        walked?;
        match number {
            1 => self
                .metadata
                .get_or_insert_with(types::Metadata::new)
                .merge_from_bytes(&unit.bytes)
                .map_err(|_| invalid("protobuf parse"))?,
            3 => {
                let s = types::SymbolInformation::parse_from_bytes(&unit.bytes)
                    .map_err(|_| invalid("protobuf parse"))?;
                self.traversed += 1;
                if self.traversed.is_multiple_of(1024) {
                    cancelled(cancel)?;
                }
                let _ = symbol(
                    &s,
                    artifact.position_encoding,
                    cancel,
                    &mut self.traversed,
                    false,
                )?;
            }
            _ => {
                let wire_digest = sha(&unit.bytes);
                let mut cap = if budget.document_cap_exceeded {
                    Some(DocumentCap::Document)
                } else if budget.document_occurrences > limits.document_occurrences {
                    Some(DocumentCap::Occurrences)
                } else {
                    None
                };
                let (d, source) = if let Some(over) = cap {
                    let (path, language) = document_identity(&unit.bytes)?;
                    if !valid_path(&path) {
                        return Err(invalid("unlisted document"));
                    }
                    let source = resolve(&path, &language)?;
                    let text = if unread(&source) {
                        None
                    } else {
                        Some(
                            std::str::from_utf8(&source.bytes)
                                .map_err(|_| invalid("non-UTF8 source"))?,
                        )
                    };
                    cap = Some(cap_document(
                        &mut unit.bytes,
                        artifact,
                        &source.language,
                        &path,
                        text,
                        over,
                        limits,
                        &mut self.traversed,
                        cancel,
                    )?);
                    let d = types::Document::parse_from_bytes(&unit.bytes)
                        .map_err(|_| invalid("protobuf parse"))?;
                    unknown(&d.special_fields)?;
                    (d, source)
                } else {
                    let mut d = types::Document::parse_from_bytes(&unit.bytes)
                        .map_err(|_| invalid("protobuf parse"))?;
                    unknown(&d.special_fields)?;
                    if !valid_path(&d.relative_path) {
                        return Err(invalid("unlisted document"));
                    }
                    let source = resolve(&d.relative_path, &d.language)?;
                    let problem = d.occurrences.iter().enumerate().find_map(|(ordinal, o)| {
                        range_problem(o, &source.language, &d.relative_path)
                            .map(|values| (ordinal, values))
                    });
                    if let Some((ordinal, values)) = problem {
                        // Owner ruling 2026-11-03 (#652): the document keeps no claim; the rest
                        // of the artifact imports.
                        report_invalid_range(&source.language, &d.relative_path, ordinal, values);
                        let text = if unread(&source) {
                            None
                        } else {
                            Some(
                                std::str::from_utf8(&source.bytes)
                                    .map_err(|_| invalid("non-UTF8 source"))?,
                            )
                        };
                        let path = d.relative_path.clone();
                        cap = Some(cap_document(
                            &mut unit.bytes,
                            artifact,
                            &source.language,
                            &path,
                            text,
                            DocumentCap::InvalidRange,
                            limits,
                            &mut self.traversed,
                            cancel,
                        )?);
                        d = types::Document::parse_from_bytes(&unit.bytes)
                            .map_err(|_| invalid("protobuf parse"))?;
                    }
                    (d, source)
                };
                // An unread source (#576) is admitted with no claims: the producer read the real
                // file, so its text and positions can't be bound to the stored placeholder, and
                // nothing it said there can be anchored. Every source-independent check still runs.
                let read = !unread(&source);
                sink.begin_document(&UnitDocument {
                    path: &d.relative_path,
                    language: &source.language,
                    wire_digest: &wire_digest,
                    cap: cap.filter(|_| read),
                })?;
                if read && !d.text.is_empty() && d.text.as_bytes() != source.bytes.as_ref() {
                    return Err(invalid("document source binding"));
                }
                let raw = known(d.position_encoding)?;
                if !(0..=3).contains(&raw)
                    || raw != 0 && raw != i32::from(artifact.position_encoding)
                {
                    return Err(invalid("position encoding"));
                }
                let reserved = reserve_document(&d, max_unit, cancel)?;
                unit.counters.unit_reservation_high_water_bytes = unit
                    .counters
                    .unit_reservation_high_water_bytes
                    .max(reserved as u64);
                let text = if read {
                    Some(
                        std::str::from_utf8(&source.bytes)
                            .map_err(|_| invalid("non-UTF8 source"))?,
                    )
                } else {
                    None
                };
                let mut positions = Vec::with_capacity(d.occurrences.len() * 4);
                let mut unresolved_markers = Vec::new();
                for o in &d.occurrences {
                    self.traversed += 1;
                    if self.traversed.is_multiple_of(1024) {
                        cancelled(cancel)?;
                    }
                    let first = positions.len();
                    let raw = occurrence(o, &mut positions, false)?;
                    if unresolved_module_marker(artifact, &source.language, &d.relative_path, o) {
                        unresolved_markers.push((positions[first], positions[first + 1]));
                    }
                    if read {
                        sink.claim(make_claim(
                            artifact,
                            &d,
                            &source.language,
                            "occurrence",
                            raw,
                        ))?;
                    }
                }
                if let Some(text) = text {
                    validate_positions(text, artifact.position_encoding, &mut positions, cancel)?;
                    check_module_markers(
                        text,
                        artifact.position_encoding,
                        &unresolved_markers,
                        cancel,
                    )?;
                }
                for s in &d.symbols {
                    self.traversed += 1;
                    if self.traversed.is_multiple_of(1024) {
                        cancelled(cancel)?;
                    }
                    let (info, rels) = symbol(
                        s,
                        artifact.position_encoding,
                        cancel,
                        &mut self.traversed,
                        true,
                    )?;
                    if !read {
                        continue;
                    }
                    sink.claim(make_claim(
                        artifact,
                        &d,
                        &source.language,
                        "symbolInformation",
                        info,
                    ))?;
                    for r in rels {
                        self.traversed += 1;
                        if self.traversed.is_multiple_of(1024) {
                            cancelled(cancel)?;
                        }
                        sink.claim(make_claim(
                            artifact,
                            &d,
                            &source.language,
                            "relationship",
                            r,
                        ))?;
                    }
                }
                sink.end_document()?;
                self.documents += 1;
            }
        }
        self.relationships = budget.relationships;
        Ok(None)
    }
}

/// Whole-artifact decoder over an in-memory artifact: the streaming decoder into a `VecSink`.
#[cfg(test)]
pub(crate) fn decode_authenticated(
    input: &AuthenticatedScipInput<'_>,
    flag: &CancelFlag,
) -> Result<CapturedScipInventory, ScipCaptureError> {
    decode_authenticated_with(input, &BatchLimits::PRODUCTION, flag)
}

/// `decode_authenticated` under explicit limits.
#[cfg(test)]
pub(crate) fn decode_authenticated_with(
    input: &AuthenticatedScipInput<'_>,
    limits: &BatchLimits,
    flag: &CancelFlag,
) -> Result<CapturedScipInventory, ScipCaptureError> {
    cancelled(flag)?;
    if input.artifact_bytes.len() as u64 > limits.artifact_bytes {
        return Err(limit("artifact bytes"));
    }
    if sha(input.artifact_bytes) != input.artifact_hash {
        return Err(ScipCaptureError::InvalidAdmission("artifact digest".into()));
    }
    if input.decoder_version != DECODER_VERSION
        || !matches!(input.position_encoding, 1..=3)
        || input.protocol_version != 0
        || input.text_document_encoding != 1
        || input.profile_id.is_empty()
    {
        return Err(ScipCaptureError::InvalidAdmission("decoder context".into()));
    }
    let mut admitted = BTreeMap::new();
    for d in &input.documents {
        if d.content_hash != sha(d.source_bytes)
            || !valid_path(&d.relative_path)
            || admitted.insert(d.relative_path.as_str(), d).is_some()
        {
            return Err(ScipCaptureError::InvalidAdmission(
                "source document hash or duplicate".into(),
            ));
        }
    }
    let artifact = AuthenticatedArtifact {
        artifact_hash: &input.artifact_hash,
        producer_id: &input.producer_id,
        producer_version: &input.producer_version,
        source_set_id: &input.source_set_id,
        source_revision: &input.source_revision,
        position_encoding: input.position_encoding,
        protocol_version: input.protocol_version,
        text_document_encoding: input.text_document_encoding,
        project_root_uri: &input.project_root_uri,
        module_roots: &ModuleRoots::default(),
        cargo_targets: &CargoTargets::default(),
        assigned_documents: admitted.len() as u64,
    };
    let mut resolve = |path: &str, language: &str| {
        let source = admitted
            .get(path)
            .ok_or_else(|| invalid("unlisted document"))?;
        let language =
            bind_document_language(language, &source.language, &input.profile_languages)?;
        Ok(DocumentSource {
            language: language.to_owned(),
            bytes: Cow::Borrowed(source.source_bytes),
            content_hash: source.content_hash.clone(),
        })
    };
    let mut sink = VecSink::default();
    decode_streaming(
        input.artifact_bytes,
        input.artifact_bytes.len() as u64,
        &artifact,
        &mut resolve,
        &mut BatchCounters::default(),
        limits,
        &mut sink,
        flag,
    )?;
    Ok(CapturedScipInventory {
        artifact_hashes: vec![input.artifact_hash.clone()],
        decoder_version: input.decoder_version.into(),
        source_manifest_hash: input.source_manifest_hash.clone(),
        completion: input.completion,
        exclusions: input.exclusions.clone(),
        capped_documents: sink.capped_documents().clone(),
        claims: sink.finish()?,
    })
}
#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod continuation_tests;
