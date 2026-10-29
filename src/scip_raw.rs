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
    pub(crate) exclusions: Vec<Exclusion>,
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
    pub(crate) fn lookup(
        &self,
        id: &str,
        kind: &str,
        digest: &str,
    ) -> Result<&RawProofIdentity, ScipCaptureError> {
        unique_proof(&self.claims, id, kind, digest, |claim| {
            (&claim.proof_id, &claim.record_kind, &claim.fact_digest)
        })
    }
}

#[derive(Debug, Clone)]
pub struct CapturedScipInventory {
    pub artifact_hashes: Vec<String>,
    pub decoder_version: String,
    pub source_manifest_hash: String,
    pub completion: Completion,
    pub exclusions: Vec<Exclusion>,
    pub claims: Vec<CapturedClaim>,
}
impl CapturedScipInventory {
    pub(crate) fn proof_identities(&self) -> ProofInventory {
        ProofInventory {
            artifact_hashes: self.artifact_hashes.clone(),
            source_manifest_hash: self.source_manifest_hash.clone(),
            exclusions: self.exclusions.clone(),
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
        // Signature `relative_path` (scip-java writes it) is presentation-only, like `language`
        // and `text` (owner ruling 2026-10-29, #439); see `signature_unknown`.
        (Signature, 1 | 4 | 5) => (2, Bytes, 1),
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
    limits: &'a BatchLimits,
}
impl<'a> WireBudget<'a> {
    fn new(limits: &'a BatchLimits) -> Self {
        WireBudget {
            fields: 0,
            relationships: 0,
            candidates: 0,
            limits,
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
        let (max, max_name) = match (node, n) {
            (Node::Document, 2) => (budget.limits.document_occurrences, "document entries"),
            (Node::Document, 3) => (budget.limits.document_symbols, "document entries"),
            (Node::Signature, 2) => (budget.limits.signature_occurrences, "signature occurrences"),
            _ => (max as u64, "repeated field"),
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
            return Err(limit(max_name));
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
                if budget.fields > budget.limits.wire_fields || *count > max {
                    return Err(limit("packed values"));
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
/// (field 1) that scip-java writes on `signature_documentation`, so it parses as an unknown field.
/// By owner ruling 2026-10-29 (#439) it is presentation-only: it must be well-formed UTF-8 and is
/// never read for identity, binding, path or digests. Every other unknown field still rejects.
fn signature_unknown(f: &protobuf::SpecialFields) -> Result<(), ScipCaptureError> {
    for (number, value) in f.unknown_fields().iter() {
        match (number, value) {
            (1, protobuf::UnknownValueRef::LengthDelimited(path)) => {
                std::str::from_utf8(path).map_err(|_| invalid("signature relative path"))?;
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
    if (sl, sc) > (el, ec) {
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

fn occurrence(
    o: &types::Occurrence,
    positions: &mut Vec<(i32, i32)>,
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
    if o.diagnostics.len() > 1000 {
        return Err(limit("diagnostics"));
    }
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
        json!({"range":range(o,false,positions)?,"enclosingRange":range(o,true,positions)?,"symbol":o.symbol,"symbolRoles":o.symbol_roles,"syntaxKind":syntax}),
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
        signature_unknown(&sig.special_fields)?;
        if sig.occurrences.len() > 10000 {
            return Err(limit("signature occurrences"));
        }
        let mut positions = Vec::with_capacity(sig.occurrences.len() * 4);
        for o in &sig.occurrences {
            *traversed += 1;
            if (*traversed).is_multiple_of(1024) {
                cancelled(flag)?;
            }
            occurrence(o, &mut positions)?;
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
    };
    let bytes = canonical(
        &json!({"formatVersion":1,"recordKind":kind,"relativePath":d.relative_path,"positionEncoding":envelope.position_encoding,"raw":envelope.raw}),
    );
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
    /// Source documents assigned to this artifact; every one must appear exactly once.
    pub assigned_documents: u64,
}
/// Identity of the document unit being decoded, handed to the sink before its claims.
pub struct UnitDocument<'a> {
    pub path: &'a str,
    pub language: &'a str,
    pub wire_digest: &'a str,
}
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
}
impl VecSink {
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
        if self.paths.insert(doc.path.to_string()) {
            Ok(())
        } else {
            Err(invalid("duplicate document path"))
        }
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
}
impl<R: Read> ArtifactReader<R> {
    fn byte(&mut self, truncated: &str) -> Result<u8, ScipCaptureError> {
        if self.remaining == 0 {
            return Err(invalid(truncated));
        }
        let mut byte = [0u8];
        self.reader
            .read_exact(&mut byte)
            .map_err(|_| invalid("artifact read"))?;
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
    if byte_length > limits.artifact_bytes {
        return Err(limit("artifact bytes"));
    }
    let capacity = byte_length.clamp(1, 1 << 20) as usize;
    let mut input = ArtifactReader {
        reader: BufReader::with_capacity(capacity, reader),
        remaining: byte_length,
    };
    let mut budget = WireBudget::new(limits);
    budget.fields = counters.wire_fields;
    budget.candidates = counters.candidates;
    let max_unit = usize::try_from(limits.unit_expansion_bytes).unwrap_or(usize::MAX);
    let mut metadata: Option<types::Metadata> = None;
    let mut documents = 0u64;
    let mut external_symbols = 0u64;
    let mut traversed = 0usize;
    while input.remaining > 0 {
        cancelled(cancel)?;
        budget.fields += 1;
        if budget.fields > limits.wire_fields {
            return Err(limit("wire fields"));
        }
        let tag = input.varint()?;
        let number = tag >> 3;
        if !matches!(number, 1..=3) {
            return Err(invalid("unknown field"));
        }
        if tag & 7 != 2 {
            return Err(invalid("field wire type"));
        }
        let len = input.varint()?;
        if len > input.remaining {
            return Err(invalid("protobuf parse"));
        }
        if len > limits.unit_expansion_bytes {
            return Err(limit("unit expansion"));
        }
        if number == 3 {
            external_symbols += 1;
            if external_symbols > limits.external_symbols {
                return Err(limit("index entries"));
            }
        }
        let len = usize::try_from(len).map_err(|_| limit("unit expansion"))?;
        let mut unit = UnitBuffer::new(counters, len);
        input
            .reader
            .read_exact(&mut unit.bytes)
            .map_err(|_| invalid("artifact read"))?;
        input.remaining -= len as u64;
        let node = match number {
            1 => Node::Metadata,
            2 => Node::Document,
            _ => Node::Symbol,
        };
        let walked = wire(&unit.bytes, node, 1, &mut budget, cancel);
        unit.counters.wire_fields = budget.fields;
        unit.counters.candidates = budget.candidates;
        walked?;
        match number {
            1 => metadata
                .get_or_insert_with(types::Metadata::new)
                .merge_from_bytes(&unit.bytes)
                .map_err(|_| invalid("protobuf parse"))?,
            3 => {
                let s = types::SymbolInformation::parse_from_bytes(&unit.bytes)
                    .map_err(|_| invalid("protobuf parse"))?;
                traversed += 1;
                if traversed.is_multiple_of(1024) {
                    cancelled(cancel)?;
                }
                let _ = symbol(
                    &s,
                    artifact.position_encoding,
                    cancel,
                    &mut traversed,
                    false,
                )?;
            }
            _ => {
                let wire_digest = sha(&unit.bytes);
                let d = types::Document::parse_from_bytes(&unit.bytes)
                    .map_err(|_| invalid("protobuf parse"))?;
                unknown(&d.special_fields)?;
                if !valid_path(&d.relative_path) {
                    return Err(invalid("unlisted document"));
                }
                let source = resolve(&d.relative_path, &d.language)?;
                sink.begin_document(&UnitDocument {
                    path: &d.relative_path,
                    language: &source.language,
                    wire_digest: &wire_digest,
                })?;
                if !d.text.is_empty() && d.text.as_bytes() != source.bytes.as_ref() {
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
                let text =
                    std::str::from_utf8(&source.bytes).map_err(|_| invalid("non-UTF8 source"))?;
                let mut positions = Vec::with_capacity(d.occurrences.len() * 4);
                for o in &d.occurrences {
                    traversed += 1;
                    if traversed.is_multiple_of(1024) {
                        cancelled(cancel)?;
                    }
                    let raw = occurrence(o, &mut positions)?;
                    sink.claim(make_claim(
                        artifact,
                        &d,
                        &source.language,
                        "occurrence",
                        raw,
                    ))?;
                }
                validate_positions(text, artifact.position_encoding, &mut positions, cancel)?;
                for s in &d.symbols {
                    traversed += 1;
                    if traversed.is_multiple_of(1024) {
                        cancelled(cancel)?;
                    }
                    let (info, rels) =
                        symbol(s, artifact.position_encoding, cancel, &mut traversed, true)?;
                    sink.claim(make_claim(
                        artifact,
                        &d,
                        &source.language,
                        "symbolInformation",
                        info,
                    ))?;
                    for r in rels {
                        traversed += 1;
                        if traversed.is_multiple_of(1024) {
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
                documents += 1;
            }
        }
    }
    if let Some(meta) = metadata.as_ref() {
        unknown(&meta.special_fields)?;
        if known(meta.version)? != artifact.protocol_version
            || known(meta.text_document_encoding)? != artifact.text_document_encoding
            || !meta.project_root.is_empty() && meta.project_root != artifact.project_root_uri
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
    if documents != artifact.assigned_documents {
        return Err(invalid("missing artifact document"));
    }
    Ok(ArtifactSummary {
        documents,
        external_symbols,
        relationships: budget.relationships,
    })
}

/// Whole-artifact decoder over an in-memory artifact: the streaming decoder into a `VecSink`.
#[cfg(test)]
pub(crate) fn decode_authenticated(
    input: &AuthenticatedScipInput<'_>,
    flag: &CancelFlag,
) -> Result<CapturedScipInventory, ScipCaptureError> {
    cancelled(flag)?;
    if input.artifact_bytes.len() as u64 > BatchLimits::PRODUCTION.artifact_bytes {
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
        &BatchLimits::PRODUCTION,
        &mut sink,
        flag,
    )?;
    Ok(CapturedScipInventory {
        artifact_hashes: vec![input.artifact_hash.clone()],
        decoder_version: input.decoder_version.into(),
        source_manifest_hash: input.source_manifest_hash.clone(),
        completion: input.completion,
        exclusions: input.exclusions.clone(),
        claims: sink.finish()?,
    })
}
#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicBool};
    pub(super) const FULL: &str = "12580a087372632f412e6a73220a6a617661736372697074120b0a0300090a120241231801120c42060800100d180e12024223120e4a080800100d1800200e120242231a130a02412322080a024223100118012811320166300112180a087372632f422e6a73220a6a6176617363726970743001";
    pub(super) const EXTERNAL: &str = "12580a087372632f412e6a73220a6a617661736372697074120b0a0300090a120241231801120c42060800100d180e12024223120e4a080800100d1800200e120242231a130a02412322080a024223100118012811320166300112180a087372632f422e6a73220a6a61766173637269707430011a0b0a0965787465726e616c23";
    pub(super) const SIGNATURE: &str = "124c0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a270a02412328113201663a1c220a6a6176617363726970742a0366282912090a0300000112024123";
    pub(super) const OMITTED: &str = "122e0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a090a0241232811320166";
    pub(super) const LEGACY: &str =
        "122a0a087372632f412e6a73220a6a61766173637269707412100a03000a0e12076c6f63616c203018013001";
    pub(super) const BRANCHES: &str = "12440a087372632f412e6a73220a6a61766173637269707412120a04000d000e1202422352060800100c1810121642060800100d180e120242235a080800100c180020103001";
    pub(super) fn vector_documents(wire_hex: &str) -> Vec<AuthenticatedScipDocument<'static>> {
        let a: &'static [u8] = if wire_hex == LEGACY {
            b"0123456789call\n"
        } else {
            b"function f(){g();}\n"
        };
        let b: &'static [u8] = b"function g(){}\n";
        if wire_hex == FULL || wire_hex == EXTERNAL {
            vec![
                AuthenticatedScipDocument {
                    language: "javascript".into(),
                    relative_path: "src/A.js".into(),
                    source_bytes: a,
                    content_hash:
                        "8026dced2c17cbbfb8563d8a7f07e250a141be61cd497e6ba88caeb52a6de8f2".into(),
                },
                AuthenticatedScipDocument {
                    language: "javascript".into(),
                    relative_path: "src/B.js".into(),
                    source_bytes: b,
                    content_hash:
                        "ae7703c040471d7d8cf8032c1d4acf32d03fb3ea089cc341f72e91cfcb3f24b5".into(),
                },
            ]
        } else {
            vec![AuthenticatedScipDocument {
                language: "javascript".into(),
                relative_path: "src/A.js".into(),
                source_bytes: a,
                content_hash: sha(a),
            }]
        }
    }
    pub(super) fn vector_input<'a>(
        bytes: &'a [u8],
        documents: Vec<AuthenticatedScipDocument<'a>>,
    ) -> AuthenticatedScipInput<'a> {
        AuthenticatedScipInput {
            artifact_hash: sha(bytes),
            artifact_bytes: bytes,
            producer_id: "scip-test".into(),
            profile_id: "synthetic-test-profile".into(),
            profile_languages: vec!["javascript".into()],
            source_set_id: "app".into(),
            source_revision: "r1".into(),
            source_manifest_hash:
                "5578e3d23b2cba76685271de456413c9b207379613d72f1b55b25d84a487cf95".into(),
            decoder_version: DECODER_VERSION,
            completion: Completion::Complete,
            exclusions: vec![],
            project_root_uri: "file:///synthetic".into(),
            producer_version: "1.0.0".into(),
            position_encoding: 1,
            protocol_version: 0,
            text_document_encoding: 1,
            documents,
        }
    }

    #[test]
    fn document_language_binding_requires_covered_source_and_exact_nonempty_language() {
        let languages = vec!["javascript".to_owned()];
        assert_eq!(
            bind_document_language("", "javascript", &languages),
            Ok("javascript")
        );
        assert_eq!(
            bind_document_language("javascript", "javascript", &languages),
            Ok("javascript")
        );
        for (raw, detected) in [("", "python"), ("python", "javascript"), ("", "")] {
            assert_eq!(
                bind_document_language(raw, detected, &languages),
                Err(invalid("document source binding"))
            );
        }
        let bytes = hex::decode(FULL).unwrap();
        let mut input = vector_input(&bytes, vector_documents(FULL));
        input.profile_languages = vec!["python".into()];
        assert_eq!(
            decode_authenticated(&input, &Arc::new(AtomicBool::new(false))).unwrap_err(),
            invalid("document source binding")
        );
    }
    #[test]
    fn empty_document_language_binds_source_without_rewriting_artifact() {
        let original = hex::decode(FULL).unwrap();
        let mut index = types::Index::parse_from_bytes(&original).unwrap();
        for document in &mut index.documents {
            document.language.clear();
        }
        let bytes = index.write_to_bytes().unwrap();
        let input = vector_input(&bytes, vector_documents(FULL));
        let inventory = decode_authenticated(&input, &Arc::new(AtomicBool::new(false)))
            .expect("empty language binds the admitted JavaScript source");
        let explicit = decoded(FULL).unwrap();
        assert_eq!(inventory.artifact_hashes, [sha(&bytes)]);
        assert!(!inventory.claims.is_empty());
        for (claim, expected) in inventory.claims.iter().zip(&explicit.claims) {
            assert_eq!(claim.language, "javascript");
            assert_eq!(claim.canonical_bytes, expected.canonical_bytes);
            assert_eq!(claim.fact_digest, expected.fact_digest);
            assert_ne!(claim.captured_proof_id, expected.captured_proof_id);
        }
        assert_eq!(input.artifact_bytes, bytes);
        assert!(
            types::Index::parse_from_bytes(&bytes)
                .unwrap()
                .documents
                .iter()
                .all(|document| document.language.is_empty())
        );
    }
    fn decoded(wire_hex: &str) -> Result<CapturedScipInventory, ScipCaptureError> {
        let bytes = hex::decode(wire_hex).unwrap();
        let input = vector_input(&bytes, vector_documents(wire_hex));
        decode_authenticated(&input, &Arc::new(AtomicBool::new(false)))
    }
    pub(super) fn var(mut x: usize) -> Vec<u8> {
        let mut b = Vec::new();
        while x >= 128 {
            b.push((x as u8) | 128);
            x >>= 7;
        }
        b.push(x as u8);
        b
    }
    pub(super) fn message(n: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![(n << 3) | 2];
        out.extend(var(body.len()));
        out.extend(body);
        out
    }
    pub(super) fn base_doc(mut fields: Vec<u8>, encoding: u8) -> Vec<u8> {
        let mut doc = message(1, b"src/A.js");
        doc.extend(message(4, b"javascript"));
        doc.append(&mut fields);
        doc.extend([0x30, encoding]);
        message(2, &doc)
    }
    fn custom(
        bytes: &[u8],
        source: &[u8],
        enc: u8,
    ) -> Result<CapturedScipInventory, ScipCaptureError> {
        custom_document(bytes, source, enc, "src/A.js", "javascript")
    }
    fn custom_document(
        bytes: &[u8],
        source: &[u8],
        enc: u8,
        path: &str,
        language: &str,
    ) -> Result<CapturedScipInventory, ScipCaptureError> {
        let input = AuthenticatedScipInput {
            artifact_hash: sha(bytes),
            artifact_bytes: bytes,
            producer_id: "scip-test".into(),
            profile_id: "synthetic-test-profile".into(),
            profile_languages: vec![language.into()],
            source_set_id: "app".into(),
            source_revision: "r1".into(),
            source_manifest_hash: "pinned-source".into(),
            decoder_version: DECODER_VERSION,
            completion: Completion::Complete,
            exclusions: vec![],
            project_root_uri: "file:///synthetic".into(),
            producer_version: "1.0.0".into(),
            position_encoding: enc,
            protocol_version: 0,
            text_document_encoding: 1,
            documents: vec![AuthenticatedScipDocument {
                language: language.into(),
                relative_path: path.into(),
                source_bytes: source,
                content_hash: sha(source),
            }],
        };
        decode_authenticated(&input, &Arc::new(AtomicBool::new(false)))
    }
    #[test]
    fn document_identity_uses_original_wire_payload_not_reserialization() {
        let original = hex::decode(FULL).unwrap();
        let mut alternate = original.clone();
        assert_eq!(&alternate[88..90], &[0x30, 0x01]);
        alternate[1] += 1;
        alternate.splice(88..90, [0x30, 0x81, 0x00]);
        struct Digests(BTreeMap<(String, String), String>);
        impl ClaimSink for Digests {
            fn begin_document(&mut self, doc: &UnitDocument<'_>) -> Result<(), ScipCaptureError> {
                self.0.insert(
                    (doc.language.to_owned(), doc.path.to_owned()),
                    doc.wire_digest.to_owned(),
                );
                Ok(())
            }
            fn claim(&mut self, _claim: CapturedClaim) -> Result<(), ScipCaptureError> {
                Ok(())
            }
            fn end_document(&mut self) -> Result<(), ScipCaptureError> {
                Ok(())
            }
        }
        let wire_digests = |bytes: &[u8]| {
            let documents = vector_documents(FULL);
            let hash = sha(bytes);
            let artifact = AuthenticatedArtifact {
                artifact_hash: &hash,
                producer_id: "scip-test",
                producer_version: "1.0.0",
                source_set_id: "app",
                source_revision: "r1",
                position_encoding: 1,
                protocol_version: 0,
                text_document_encoding: 1,
                project_root_uri: "file:///synthetic",
                assigned_documents: documents.len() as u64,
            };
            let mut resolve = |path: &str, _language: &str| {
                let document = documents
                    .iter()
                    .find(|document| document.relative_path == path)
                    .ok_or_else(|| invalid("unlisted document"))?;
                Ok(DocumentSource {
                    language: document.language.clone(),
                    bytes: Cow::Borrowed(document.source_bytes),
                    content_hash: document.content_hash.clone(),
                })
            };
            let mut sink = Digests(BTreeMap::new());
            decode_streaming(
                bytes,
                bytes.len() as u64,
                &artifact,
                &mut resolve,
                &mut BatchCounters::default(),
                &BatchLimits::PRODUCTION,
                &mut sink,
                &Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            sink.0
        };
        let first = wire_digests(&original);
        let second = wire_digests(&alternate);
        assert_ne!(
            first[&("javascript".into(), "src/A.js".into())],
            second[&("javascript".into(), "src/A.js".into())]
        );
        assert_eq!(
            first[&("javascript".into(), "src/B.js".into())],
            second[&("javascript".into(), "src/B.js".into())]
        );
        assert_eq!(
            types::Index::parse_from_bytes(&original).unwrap(),
            types::Index::parse_from_bytes(&alternate).unwrap()
        );
    }

    #[test]
    fn scip_exact_pinned_full_inventory() {
        let inventory = decoded(FULL).unwrap();
        assert_eq!(inventory.claims.len(), 5);
        let pinned = [
            (
                "138cf48a43d4bedcee1dc8236317c40d691ef5fda117d1e30c15526c8bda21dc",
                "8d91e854886b2b9f02aaa8dc4a240714a5ca4de8b144b3cf6dd430219100a0e0",
            ),
            (
                "e49f19eab6e6c590aa18b0e4257c9627bbc8ee998cc31c2567d8de168467c54f",
                "74f8160501fb821c4554cc2671bb843eda75fd17294466d94213f547687807fc",
            ),
            (
                "40a8a56aa6ed4e8011c75c28ab01cb9f23a0bdc504fb4ec97025142e00f1f7a6",
                "8e02c380dd47ad9a2f83cfeb0ffa138a7925817ee00207d42a9727849bb07dca",
            ),
            (
                "39d9337353c729180a48d4489882e6740625199bd54fe9a98b59ad3eab98ec9f",
                "c1311eda3c429295f8f161484b0831f57d4eacb2854ad92204fbde79a4eb7c77",
            ),
            (
                "e38fa5f77ba73cee8bbe54eeec279ff98c4b250366f3b59dc8288015bd538fb0",
                "c07c6266d07c757383acd60e0dedc46a05ff560bb3b1ccbfbaa1c5ce4c3b669c",
            ),
        ];
        for (fact, proof) in pinned {
            let claim = inventory
                .claims
                .iter()
                .find(|c| c.fact_digest == fact)
                .expect("pinned fact digest");
            assert_eq!(claim.captured_proof_id, proof);
        }
        assert_eq!(inventory.claims.iter().find(|c|c.fact_digest=="39d9337353c729180a48d4489882e6740625199bd54fe9a98b59ad3eab98ec9f").unwrap().canonical_bytes, br#"{"formatVersion":1,"positionEncoding":1,"raw":{"displayName":"f","enclosingSymbol":"","kind":17,"symbol":"A#"},"recordKind":"symbolInformation","relativePath":"src/A.js"}"#);
        assert_eq!(inventory.claims.iter().find(|c|c.fact_digest=="e38fa5f77ba73cee8bbe54eeec279ff98c4b250366f3b59dc8288015bd538fb0").unwrap().canonical_bytes, br#"{"formatVersion":1,"positionEncoding":1,"raw":{"isDefinition":false,"isImplementation":true,"isReference":true,"isTypeDefinition":false,"sourceSymbol":"A#","targetSymbol":"B#"},"recordKind":"relationship","relativePath":"src/A.js"}"#);
        let call = inventory
            .claims
            .iter()
            .find(|c| {
                c.fact_digest == "138cf48a43d4bedcee1dc8236317c40d691ef5fda117d1e30c15526c8bda21dc"
            })
            .unwrap();
        assert_eq!(call.canonical_bytes,b"{\"formatVersion\":1,\"positionEncoding\":1,\"raw\":{\"enclosingRange\":null,\"range\":{\"encoding\":\"singleLine\",\"endCharacter\":14,\"line\":0,\"startCharacter\":13},\"symbol\":\"B#\",\"symbolRoles\":0,\"syntaxKind\":0},\"recordKind\":\"occurrence\",\"relativePath\":\"src/A.js\"}");
        assert_eq!(
            call.captured_proof_id,
            "8d91e854886b2b9f02aaa8dc4a240714a5ca4de8b144b3cf6dd430219100a0e0"
        );
        assert_eq!(
            inventory
                .lookup(&call.captured_proof_id, "occurrence", &call.fact_digest)
                .unwrap(),
            call
        );
        assert_eq!(
            inventory
                .lookup(&call.captured_proof_id, "occurrence", &call.fact_digest)
                .unwrap(),
            call
        );
        assert!(matches!(
            inventory.lookup(&call.captured_proof_id, "relationship", &call.fact_digest),
            Err(ScipCaptureError::MissingProof)
        ));
    }
    fn assert_repeated_legacy_equivalent(primary: &[u8], enclosing: &[u8]) {
        let packed_primary = [0x0a, 3, 0, 9, 10];
        let packed_enclosing = [0x3a, 3, 0, 0, 17];
        let baseline = custom(
            &base_doc(
                message(2, &[packed_primary.as_slice(), &packed_enclosing].concat()),
                1,
            ),
            b"function f(){g();}\n",
            1,
        )
        .expect("packed range baseline");
        let bytes = base_doc(message(2, &[primary, enclosing].concat()), 1);
        let generated = types::Index::parse_from_bytes(&bytes).expect("legal repeated int32 wire");
        assert_eq!(generated.documents[0].occurrences[0].range, [0, 9, 10]);
        assert_eq!(
            generated.documents[0].occurrences[0].enclosing_range,
            [0, 0, 17]
        );
        let actual = custom(&bytes, b"function f(){g();}\n", 1)
            .expect("legal repeated legacy fields must concatenate");
        assert_eq!(actual.claims.len(), 1);
        assert_eq!(
            actual.claims[0].canonical_bytes,
            baseline.claims[0].canonical_bytes
        );
        assert_eq!(actual.claims[0].fact_digest, baseline.claims[0].fact_digest);
    }
    #[test]
    fn scip_repeated_legacy_unpacked_primary() {
        assert_repeated_legacy_equivalent(&[8, 0, 8, 9, 8, 10], &[0x3a, 3, 0, 0, 17]);
    }
    #[test]
    fn scip_repeated_legacy_chunked_primary() {
        assert_repeated_legacy_equivalent(&[0x0a, 1, 0, 0x0a, 2, 9, 10], &[0x3a, 3, 0, 0, 17]);
    }
    #[test]
    fn scip_repeated_legacy_unpacked_enclosing() {
        assert_repeated_legacy_equivalent(&[0x0a, 3, 0, 9, 10], &[0x38, 0, 0x38, 0, 0x38, 17]);
    }
    #[test]
    fn scip_repeated_legacy_chunked_enclosing() {
        assert_repeated_legacy_equivalent(&[0x0a, 3, 0, 9, 10], &[0x3a, 1, 0, 0x3a, 2, 0, 17]);
    }
    #[test]
    fn scip_repeated_legacy_mixed_packed_and_unpacked() {
        assert_repeated_legacy_equivalent(&[8, 0, 0x0a, 2, 9, 10], &[0x3a, 2, 0, 0, 0x38, 17]);
        let baseline = base_doc(message(2, &[0x0a, 4, 0, 9, 0, 10, 0x3a, 4, 0, 0, 0, 17]), 1);
        let repeated = base_doc(
            message(
                2,
                &[8, 0, 8, 9, 8, 0, 8, 10, 0x38, 0, 0x38, 0, 0x38, 0, 0x38, 17],
            ),
            1,
        );
        let expected = custom(&baseline, b"function f(){g();}\n", 1).unwrap();
        let actual = custom(&repeated, b"function f(){g();}\n", 1).unwrap();
        assert_eq!(
            actual.claims[0].canonical_bytes,
            expected.claims[0].canonical_bytes
        );
        assert_eq!(actual.claims[0].fact_digest, expected.claims[0].fact_digest);
    }
    #[test]
    fn scip_legacy_typed_and_competing_typed_ranges_still_reject() {
        let primary = vec![0x0a, 3, 0, 9, 10];
        let enclosing = vec![0x3a, 3, 0, 0, 17];
        let typed = message(8, &[0x10, 9, 0x18, 10]);
        let multi = message(9, &[0x10, 9, 0x20, 10]);
        for fields in [
            [primary.clone(), typed.clone()].concat(),
            [primary.clone(), enclosing, message(10, &[0x18, 17])].concat(),
            [typed, multi].concat(),
            [primary, message(10, &[0x18, 17]), message(11, &[0x20, 17])].concat(),
        ] {
            assert!(matches!(
                custom(
                    &base_doc(message(2, &fields), 1),
                    b"function f(){g();}\n",
                    1
                ),
                Err(ScipCaptureError::InvalidArtifact(_))
            ));
        }
    }
    #[test]
    fn scip_complete_negative_int32_coordinates_reject() {
        let source = b"function f(){g();}\n";
        for occurrence in [vec![0x0a, 3, 0, 0, 0], message(8, &[])] {
            custom(&base_doc(message(2, &occurrence), 1), source, 1)
                .expect("zero-coordinate baseline");
        }
        for occurrence in [
            hex::decode("0a0c0000ffffffffffffffffff01").unwrap(),
            message(8, &hex::decode("18ffffffffffffffffff01").unwrap()),
        ] {
            let bytes = base_doc(message(2, &occurrence), 1);
            let generated =
                types::Index::parse_from_bytes(&bytes).expect("complete signed int32 wire");
            let occurrence = &generated.documents[0].occurrences[0];
            let mut positions = Vec::new();
            assert!(matches!(
                range(occurrence, false, &mut positions),
                Err(ScipCaptureError::InvalidArtifact(reason)) if reason == "range components"
            ));
            assert!(matches!(
                custom(&bytes, source, 1),
                Err(ScipCaptureError::InvalidArtifact(reason)) if reason == "range components"
            ));
        }
    }
    #[test]
    fn scip_int32_overflow_rejects_in_generated_decoder() {
        for occurrence in [
            hex::decode("0a0700008080808010").unwrap(),
            message(8, &hex::decode("188080808010").unwrap()),
        ] {
            let bytes = base_doc(message(2, &occurrence), 1);
            assert!(types::Index::parse_from_bytes(&bytes).is_err());
            assert!(matches!(
                custom(&bytes, b"function f(){g();}\n", 1),
                Err(ScipCaptureError::InvalidArtifact(_))
            ));
        }
    }
    #[test]
    fn scip_rust_utf8_bom_is_a_source_scalar() {
        for (end, should_accept) in [(4, true), (1, false)] {
            let occurrence = message(2, &[0x0a, 3, 0, 0, end]);
            let bytes = message(
                2,
                &[
                    message(1, b"src/A.rs"),
                    message(4, b"rust"),
                    occurrence,
                    vec![0x30, 1],
                ]
                .concat(),
            );
            let result = custom_document(&bytes, &[0xef, 0xbb, 0xbf, b'x'], 1, "src/A.rs", "rust");
            if should_accept {
                assert_eq!(result.unwrap().claims.len(), 1);
            } else {
                assert!(matches!(
                    result,
                    Err(ScipCaptureError::InvalidArtifact(reason)) if reason == "split codepoint"
                ));
            }
        }
    }
    #[test]
    fn scip_wire_preflight_rejects_ambiguity_and_unknowns() {
        let flag = Arc::new(AtomicBool::new(false));
        let occ = hex::decode("0a0300090a120241231801").unwrap();
        let mut competing = occ.clone();
        competing.extend(hex::decode("4200").unwrap());
        assert!(matches!(
            wire(
                &competing,
                Node::Occurrence,
                0,
                &mut WireBudget::default(),
                &flag
            ),
            Err(ScipCaptureError::InvalidArtifact(_))
        ));
        let mut duplicate = hex::decode("4200").unwrap();
        duplicate.extend(hex::decode("4200").unwrap());
        assert!(
            wire(
                &duplicate,
                Node::Occurrence,
                0,
                &mut WireBudget::default(),
                &flag
            )
            .is_err()
        );
        for node in [
            Node::Index,
            Node::Metadata,
            Node::Tool,
            Node::Document,
            Node::Symbol,
            Node::Relationship,
            Node::Signature,
            Node::Occurrence,
            Node::Diagnostic,
            Node::Single,
            Node::Multi,
        ] {
            assert!(
                wire(
                    &[0xf8, 0x07, 0x01],
                    node,
                    0,
                    &mut WireBudget::default(),
                    &flag
                )
                .is_err()
            );
        }
        assert!(
            wire(
                &[0x12, 0x05, 0x00],
                Node::Index,
                0,
                &mut WireBudget::default(),
                &flag
            )
            .is_err()
        );
        assert!(
            wire(
                &[0x0a, 0x05, 0x80],
                Node::Occurrence,
                0,
                &mut WireBudget::default(),
                &flag
            )
            .is_err()
        );
    }
    #[test]
    fn scip_duplicate_documents_and_mismatched_languages_reject() {
        let source = b"function f(){g();}\n";
        for (first_language, second_language) in [
            ("javascript", "javascript"),
            ("javascript", "python"),
            ("python", "javascript"),
        ] {
            let document = |language: &str| {
                message(
                    2,
                    &[message(1, b"src/A.js"), message(4, language.as_bytes())].concat(),
                )
            };
            let first = document(first_language);
            custom_document(&first, source, 1, "src/A.js", first_language)
                .expect("single admitted document baseline");
            let bytes = [first, document(second_language)].concat();
            let error = custom_document(&bytes, source, 1, "src/A.js", first_language).unwrap_err();
            assert_eq!(
                error,
                invalid(if first_language == second_language {
                    "duplicate document path"
                } else {
                    "document source binding"
                }),
                "{first_language}/{second_language}"
            );
        }
    }
    #[test]
    fn scip_unknown_protocol_enum_rejects() {
        let enum_wire = format!("0a020863{FULL}");
        assert!(matches!(
            decoded(&enum_wire),
            Err(ScipCaptureError::InvalidArtifact(_))
        ));
    }
    #[test]
    fn scip_wire_ranges_and_limits_before_allocation() {
        let flag = Arc::new(AtomicBool::new(false));
        for bytes in [
            "42004200",
            "42004a00",
            "0a0300090a4200",
            "3a01005200",
            "3a01005a00",
            "52005a00",
        ] {
            assert!(
                wire(
                    &hex::decode(bytes).unwrap(),
                    Node::Occurrence,
                    0,
                    &mut WireBudget::default(),
                    &flag
                )
                .is_err(),
                "{bytes}"
            );
        }
        let mut pack = vec![0x0a, 0x80, 0x80, 0x40];
        pack.extend(std::iter::repeat_n(0u8, 1_048_576));
        assert!(matches!(
            wire(
                &pack,
                Node::Occurrence,
                0,
                &mut WireBudget::default(),
                &flag
            ),
            Err(ScipCaptureError::LimitExceeded(_))
        ));
        for (node, tag, limit_count) in [
            (Node::Index, 0x12, 1001),
            (Node::Document, 0x12, 10001),
            (Node::Document, 0x1a, 10001),
            (Node::Symbol, 0x22, 20001),
            (Node::Signature, 0x12, 10001),
            (Node::Occurrence, 0x32, 1001),
        ] {
            let repeated = std::iter::repeat_n([tag, 0], limit_count)
                .flatten()
                .collect::<Vec<_>>();
            let mut budget = WireBudget::default();
            assert!(matches!(
                wire(&repeated, node, 0, &mut budget, &flag),
                Err(ScipCaptureError::LimitExceeded(_))
            ));
        }
    }
    #[test]
    fn scip_global_candidate_wire_budget_is_independent_of_per_document_limit() {
        let flag = Arc::new(AtomicBool::new(false));
        let occurrence = message(2, &[0x0a, 3, 0, 0, 1]);
        let document = |number: usize, count: usize| {
            message(
                2,
                &[
                    message(1, format!("src/{number}.js").as_bytes()),
                    message(4, b"javascript"),
                    occurrence.repeat(count),
                    vec![0x30, 1],
                ]
                .concat(),
            )
        };
        let baseline = (0..10)
            .flat_map(|number| document(number, 10000))
            .collect::<Vec<_>>();
        let limits = BatchLimits {
            candidates: 100_000,
            ..BatchLimits::PRODUCTION
        };
        let mut budget = WireBudget::new(&limits);
        wire(&baseline, Node::Index, 0, &mut budget, &flag)
            .expect("100000 candidates within per-document and batch wire caps");
        assert_eq!(budget.candidates, 100000);
        let bytes = [baseline, document(10, 1)].concat();
        assert!(matches!(
            wire(&bytes, Node::Index, 0, &mut WireBudget::new(&limits), &flag),
            Err(ScipCaptureError::LimitExceeded(reason)) if reason == "candidate facts"
        ));
    }
    #[test]
    fn scip_raw_duplicate_claims_and_lookup_fail_closed() {
        let first = decoded(FULL).unwrap();
        let mut duplicate = first.clone();
        duplicate.claims.push(first.claims[0].clone());
        let claim = &first.claims[0];
        assert!(matches!(
            duplicate.lookup(
                &claim.captured_proof_id,
                &claim.envelope.record_kind,
                &claim.fact_digest
            ),
            Err(ScipCaptureError::NonUniqueProof)
        ));
        assert!(matches!(
            first.lookup("0", &claim.envelope.record_kind, &claim.fact_digest),
            Err(ScipCaptureError::MissingProof)
        ));
        assert!(matches!(
            first.lookup(&claim.captured_proof_id, &claim.envelope.record_kind, "0"),
            Err(ScipCaptureError::MissingProof)
        ));
        let mut bytes = hex::decode(LEGACY).unwrap();
        let original_doc_length = usize::from(bytes[1]);
        let duplicate = hex::decode("12100a03000a0e12076c6f63616c20301801").unwrap();
        bytes[1] = u8::try_from(original_doc_length + duplicate.len()).unwrap();
        bytes.splice(2 + original_doc_length..2 + original_doc_length, duplicate);
        let mut inventory = decoded(&hex::encode(&bytes)).unwrap();
        assert_eq!(inventory.claims.len(), 1);
        assert_eq!(
            inventory.claims[0].fact_digest,
            "6d600757051c743f49830194c6d0cf5d78b27c666fbf60e3ce95100384d45670"
        );
        inventory.claims.push(inventory.claims[0].clone());
        assert!(matches!(
            inventory.lookup(
                &inventory.claims[0].captured_proof_id,
                "occurrence",
                &inventory.claims[0].fact_digest
            ),
            Err(ScipCaptureError::NonUniqueProof)
        ));
    }
    #[test]
    fn scip_nested_unknown_and_enum_closure() {
        let unknown = [0xf8, 0x07, 0x01];
        let occ = hex::decode("0a0300090a120241231801").unwrap();
        let mut cases = vec![
            ("index", unknown.to_vec()),
            ("metadata", message(1, &unknown)),
            ("tool", message(1, &message(2, &unknown))),
            ("document", base_doc(unknown.to_vec(), 1)),
        ];
        let mut o = occ.clone();
        o.extend(unknown);
        cases.push(("occurrence", base_doc(message(2, &o), 1)));
        let mut o = occ.clone();
        o.extend(message(8, &unknown));
        cases.push(("typed range", base_doc(message(2, &o), 1)));
        let typed_single = message(8, &[0x10, 0x00, 0x18, 0x01]);
        let typed_multi = message(9, &[0x10, 0x00, 0x20, 0x01]);
        let unknown_single = message(
            8,
            &[vec![0x10, 0x00, 0x18, 0x01], unknown.to_vec()].concat(),
        );
        let unknown_multi = message(
            9,
            &[vec![0x10, 0x00, 0x20, 0x01], unknown.to_vec()].concat(),
        );
        let unknown_enclosing = message(
            10,
            &[vec![0x10, 0x00, 0x18, 0x01], unknown.to_vec()].concat(),
        );
        cases.push(("typed single", base_doc(message(2, &unknown_single), 1)));
        cases.push(("typed multi", base_doc(message(2, &unknown_multi), 1)));
        cases.push((
            "typed enclosing",
            base_doc(
                message(2, &[typed_single.clone(), unknown_enclosing].concat()),
                1,
            ),
        ));
        cases.push((
            "typed multi enclosing",
            base_doc(
                message(2, &[typed_multi, message(11, &unknown)].concat()),
                1,
            ),
        ));
        cases.push((
            "external nested relationship",
            message(3, &message(4, &unknown)),
        ));
        let mut o = occ.clone();
        o.extend(message(6, &unknown));
        cases.push(("diagnostic", base_doc(message(2, &o), 1)));
        cases.push(("symbol", base_doc(message(3, &unknown), 1)));
        cases.push(("external symbol", message(3, &unknown)));
        cases.push((
            "relationship",
            base_doc(message(3, &message(4, &unknown)), 1),
        ));
        cases.push(("signature", base_doc(message(3, &message(7, &unknown)), 1)));
        let mut sig = message(5, b"f()");
        sig.extend(message(2, &[occ.clone(), unknown.to_vec()].concat()));
        cases.push((
            "signature occurrence",
            base_doc(message(3, &message(7, &sig)), 1),
        ));
        for (name, bytes) in cases {
            assert!(
                matches!(
                    custom(&bytes, b"function f(){g();}\n", 1),
                    Err(ScipCaptureError::InvalidArtifact(_))
                ),
                "{name}"
            );
        }
        let mut bad = occ.clone();
        bad.extend([0x28, 0x7f]);
        for (name, bytes) in [
            ("syntax", base_doc(message(2, &bad), 1)),
            (
                "roles",
                base_doc(
                    message(
                        2,
                        &[occ[..occ.len() - 2].to_vec(), vec![0x18, 0x80, 0x01]].concat(),
                    ),
                    1,
                ),
            ),
            ("position", base_doc(vec![], 99)),
            ("symbol kind", base_doc(message(3, &[0x28, 0x7f]), 1)),
            (
                "severity",
                base_doc(
                    message(2, &[occ.clone(), message(6, &[0x08, 0x7f])].concat()),
                    1,
                ),
            ),
            (
                "tag",
                base_doc(
                    message(2, &[occ.clone(), message(6, &[0x28, 0x7f])].concat()),
                    1,
                ),
            ),
            ("external kind", message(3, &[0x28, 0x7f])),
        ] {
            assert!(
                matches!(
                    custom(&bytes, b"function f(){g();}\n", 1),
                    Err(ScipCaptureError::InvalidArtifact(_))
                ),
                "{name}"
            );
        }
    }
    #[test]
    fn scip_metadata_and_tool_context_reject_mismatches() {
        let doc = base_doc(vec![], 1);
        let metadata = [vec![0x20, 0x02], message(3, b"file:///synthetic")].concat();
        assert!(matches!(
            custom(
                &[message(1, &metadata), doc.clone()].concat(),
                b"function f(){g();}\n",
                1
            ),
            Err(ScipCaptureError::InvalidArtifact(_))
        ));
        for (name, version, should_accept) in [
            ("scip-test", "1.0.0", true),
            ("wrong-producer", "1.0.0", false),
            ("scip-test", "wrong-version", false),
        ] {
            let tool = [message(1, name.as_bytes()), message(2, version.as_bytes())].concat();
            let metadata = [vec![0x20, 1], message(2, &tool)].concat();
            let result = custom(
                &[message(1, &metadata), doc.clone()].concat(),
                b"function f(){g();}\n",
                1,
            );
            if should_accept {
                result.expect("matching tool and text encoding baseline");
            } else {
                assert!(
                    matches!(
                        result,
                        Err(ScipCaptureError::InvalidArtifact(reason)) if reason == "tool binding"
                    ),
                    "{name}/{version}"
                );
            }
        }
    }
    #[test]
    fn scip_document_source_and_encoding_bindings() {
        let source = b"function f(){g();}\n";
        let occ = hex::decode("0a0300090a120241231801").unwrap();
        let doc = base_doc(message(2, &occ), 0);
        let result = custom(&doc, source, 1).unwrap();
        assert_eq!(result.claims[0].envelope.position_encoding, 0);
        assert_eq!(
            result.claims[0].fact_digest,
            "48eecbae6ce056236cc3decb2fb3c25c46bcd2fe54cc9badb5fb9a12ae318dc7"
        );
        assert!(custom(&doc, source, 0).is_err());
        assert!(custom(&base_doc(message(2, &occ), 2), source, 1).is_err());
        let changed_text = [message(5, b"other"), message(2, &occ)].concat();
        assert!(custom(&base_doc(changed_text, 1), source, 1).is_err());
        let wrong_language = [message(4, b"python"), message(2, &occ)].concat();
        assert!(custom(&base_doc(wrong_language, 1), source, 1).is_err());
        let mut unlisted = message(1, b"other.js");
        unlisted.extend(message(4, b"javascript"));
        assert!(custom(&message(2, &unlisted), source, 1).is_err());
        assert!(custom(&[], source, 1).is_err());
        let outside = message(2, &[0x0a, 0x03, 0x00, 0x64, 0x65]);
        assert!(custom(&base_doc(outside, 1), source, 1).is_err());
    }
    #[test]
    fn scip_fanout_expansion_is_bounded_before_materialization() {
        let source = "x".repeat(12_000);
        let mut symbol = types::SymbolInformation::new();
        symbol.symbol = source;
        symbol.relationships = (0..2000).map(|_| types::Relationship::new()).collect();
        let mut doc = types::Document::new();
        doc.relative_path = "src/A.js".into();
        doc.language = "javascript".into();
        doc.symbols.push(symbol);
        assert!(matches!(
            reserve_document(
                &doc,
                BatchLimits::PRODUCTION.unit_expansion_bytes as usize,
                &Arc::new(AtomicBool::new(false))
            ),
            Err(ScipCaptureError::LimitExceeded(reason)) if reason == "unit expansion"
        ));
    }
    #[test]
    fn scip_duplicate_symbols_and_relationships_deduplicate() {
        let symbol = [message(1, b"A#"), vec![0x28, 17], message(6, b"f")].concat();
        let rel = [message(1, b"B#"), vec![0x10, 1]].concat();
        let with_relation = [symbol.clone(), message(4, &rel)].concat();
        let duplicate = base_doc(
            [message(3, &with_relation), message(3, &with_relation)].concat(),
            1,
        );
        let inventory = custom(&duplicate, b"function f(){g();}\n", 1).unwrap();
        assert_eq!(inventory.claims.len(), 2);
        let distinct = base_doc(
            [
                message(3, &with_relation),
                message(
                    3,
                    &[symbol, message(4, &[rel, vec![0x18, 1]].concat())].concat(),
                ),
            ]
            .concat(),
            1,
        );
        assert_eq!(
            custom(&distinct, b"function f(){g();}\n", 1)
                .unwrap()
                .claims
                .len(),
            3
        );
    }
    #[test]
    fn authenticated_coordinate_conversion_and_zero_fallback() {
        let source = "a😀b\r\nz";
        let flag = Arc::new(AtomicBool::new(false));
        for (encoding, end) in [(1, 5), (2, 3), (3, 2)] {
            let mut positions = vec![(1, 1), (0, end), (0, 1), (1, 0), (0, end)];
            let bytes = source_byte_offsets(source, encoding, &mut positions, &flag).unwrap();
            assert_eq!(bytes[&(0, 1)], 1);
            assert_eq!(bytes[&(0, end)], 5);
            assert_eq!(bytes[&(1, 0)], 8);
            assert_eq!(bytes[&(1, 1)], 9);
            assert_eq!(positions.len(), 4);
            assert!(source_byte_offsets(source, encoding, &mut vec![(0, end + 2)], &flag).is_err());
            if encoding != 3 {
                assert!(
                    source_byte_offsets(source, encoding, &mut vec![(0, end - 1)], &flag).is_err()
                );
            }
        }
        let occ = hex::decode("0a03000103120241231801").unwrap();
        let raw_zero = base_doc(message(2, &occ), 0);
        assert!(custom(&raw_zero, source.as_bytes(), 2).is_ok());
        assert!(
            matches!(custom(&raw_zero, source.as_bytes(), 1), Err(ScipCaptureError::InvalidArtifact(reason)) if reason == "split codepoint")
        );
        assert!(custom(&base_doc(message(2, &occ), 1), source.as_bytes(), 2).is_err());
    }
    #[test]
    fn scip_position_scalar_boundaries_and_cancel() {
        assert!(check_position("a😀b\r\n", 0, 1, 1).is_ok());
        assert!(check_position("a😀b\r\n", 0, 2, 1).is_err());
        assert!(check_position("a😀b\r\n", 0, 2, 2).is_err());
        assert!(check_position("a😀b\r\n", 0, 3, 2).is_ok());
        assert!(check_position("a😀b\r\n", 0, 2, 3).is_ok());
        assert!(check_position("a😀b\r\n", 1, 1, 3).is_err());
        for enc in 1..=3 {
            assert!(check_position("a\r", 0, 2, enc).is_ok());
            assert!(check_position("a\r\r\n", 0, 2, enc).is_ok());
            assert!(check_position("a\r\r\n", 0, 3, enc).is_err());
            assert!(check_position("a\r\r\n", 1, 0, enc).is_ok());
        }
        let dense = "\n".repeat(50_000);
        let mut sparse = vec![(0, 0), (49_999, 0), (50_000, 0)];
        validate_positions(&dense, 1, &mut sparse, &Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(
            sparse.len(),
            3,
            "memory follows requested positions, not newline count"
        );
        assert!(check_position(&dense, 50_001, 0, 1).is_err());
        let long = format!("{}z", "x".repeat(120_000));
        let mut many = (0..2000).map(|_| (0, 120_000)).collect::<Vec<_>>();
        validate_positions(&long, 1, &mut many, &Arc::new(AtomicBool::new(false))).unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        POSITION_WALK_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|flag| flag.store(true, Ordering::Relaxed)))
        });
        let mut far = vec![(0, 120_000)];
        let result = validate_positions(&long, 1, &mut far, &flag);
        POSITION_WALK_HOOK.with(|hook| *hook.borrow_mut() = None);
        assert!(matches!(result, Err(ScipCaptureError::Cancelled)));
        assert!(flag.load(Ordering::Relaxed));
        let flag = Arc::new(AtomicBool::new(true));
        assert_eq!(
            wire(&[], Node::Index, 0, &mut WireBudget::default(), &flag),
            Ok(())
        );
        let many = std::iter::repeat_n([0x1a, 0], 1024)
            .flatten()
            .collect::<Vec<_>>();
        assert!(matches!(
            wire(&many, Node::Tool, 0, &mut WireBudget::default(), &flag),
            Err(ScipCaptureError::Cancelled)
        ));
        let bytes = hex::decode(FULL).unwrap();
        let context = AuthenticatedScipInput {
            artifact_hash: sha(&bytes),
            artifact_bytes: &bytes,
            producer_id: "scip-test".into(),
            profile_id: "test".into(),
            profile_languages: vec!["javascript".into()],
            source_set_id: "app".into(),
            source_revision: "r1".into(),
            source_manifest_hash: "test".into(),
            decoder_version: DECODER_VERSION,
            completion: Completion::Complete,
            exclusions: vec![],
            project_root_uri: "file:///synthetic".into(),
            producer_version: "1.0.0".into(),
            position_encoding: 1,
            protocol_version: 0,
            text_document_encoding: 1,
            documents: vec![],
        };
        assert!(matches!(
            decode_authenticated(&context, &flag),
            Err(ScipCaptureError::Cancelled)
        ));
    }
    #[test]
    fn scip_legacy_and_typed_range_vectors() {
        let legacy = decoded(LEGACY).unwrap();
        assert_eq!(legacy.claims.len(), 1);
        assert_eq!(
            legacy.claims[0].fact_digest,
            "6d600757051c743f49830194c6d0cf5d78b27c666fbf60e3ce95100384d45670"
        );
        let branches = decoded(BRANCHES).unwrap();
        assert_eq!(branches.claims.len(), 2);
        assert_eq!(
            branches.claims[0].fact_digest,
            "c7cbbbd7c3e447e4f3d1e5438b361d94b755627dc66f4c9601a0f6449e6b18bd"
        );
        assert_eq!(
            branches.claims[1].fact_digest,
            "93671bccac30c539d7d133849995f8798f3e4ba7a56804e4577ab108870e30c4"
        );
    }
    #[test]
    fn scip_presentation_and_external_context_hash_only() {
        let (base, external, signature, omitted) = (
            decoded(FULL).unwrap(),
            decoded(EXTERNAL).unwrap(),
            decoded(SIGNATURE).unwrap(),
            decoded(OMITTED).unwrap(),
        );
        assert_eq!(
            base.claims
                .iter()
                .map(|c| (&c.canonical_bytes, &c.fact_digest))
                .collect::<Vec<_>>(),
            external
                .claims
                .iter()
                .map(|c| (&c.canonical_bytes, &c.fact_digest))
                .collect::<Vec<_>>()
        );
        for (fact, proof) in [
            (
                "138cf48a43d4bedcee1dc8236317c40d691ef5fda117d1e30c15526c8bda21dc",
                "5f06a5a26e82a671ac92c9f60ecd804aa0a1161e9933ae6b3b96b3eb77f5e9b6",
            ),
            (
                "e49f19eab6e6c590aa18b0e4257c9627bbc8ee998cc31c2567d8de168467c54f",
                "3a5a01c21df6e88eca70514edb3d9433e0b03ae9a84eb0cad409aa4641f314d1",
            ),
            (
                "40a8a56aa6ed4e8011c75c28ab01cb9f23a0bdc504fb4ec97025142e00f1f7a6",
                "82b53fe23ca439ff9517ce43979039dce1b2d4cbd7ee59c316801c3c3cc1700a",
            ),
            (
                "39d9337353c729180a48d4489882e6740625199bd54fe9a98b59ad3eab98ec9f",
                "aadefd7776bc74f80c4b600b3e0421e9bc0c4122cec7b6125a085721855299c1",
            ),
            (
                "e38fa5f77ba73cee8bbe54eeec279ff98c4b250366f3b59dc8288015bd538fb0",
                "f5f084bbb2ac6624e3e5f2d7d8b5550a40e355523dee30d9906edf2525e21e97",
            ),
        ] {
            let claim = external
                .claims
                .iter()
                .find(|c| c.fact_digest == fact)
                .expect("129-byte fact");
            assert_eq!(claim.captured_proof_id, proof);
        }
        assert_eq!(signature.claims.len(), 2);
        assert_eq!(
            signature
                .claims
                .iter()
                .map(|c| &c.fact_digest)
                .collect::<Vec<_>>(),
            omitted
                .claims
                .iter()
                .map(|c| &c.fact_digest)
                .collect::<Vec<_>>()
        );
        for (fact, proof) in [
            (
                "48eecbae6ce056236cc3decb2fb3c25c46bcd2fe54cc9badb5fb9a12ae318dc7",
                "1ad7595f3e9fd3d984cf62c564cd1e72f48e912389a83d5a7b67abefb3fb3328",
            ),
            (
                "68953b47a29e361cf99726fe506d7449dfc9a4c2c1a250aa0a6f52d0a96cc561",
                "3eef2daff2d27a4dcdbc9d7e75ed9575ba60c47538ff362f80471f6100b411ac",
            ),
        ] {
            let claim = omitted
                .claims
                .iter()
                .find(|c| c.fact_digest == fact)
                .expect("48-byte fact");
            assert_eq!(claim.captured_proof_id, proof);
        }
    }
    /// Owner ruling 2026-10-29 (#439): scip-java writes `relative_path` (field 1) on the
    /// `Document` under `signature_documentation`; it is presentation-only.
    fn with_signature_path(wire_hex: &str, path: &str) -> Vec<u8> {
        let mut index = types::Index::parse_from_bytes(&hex::decode(wire_hex).unwrap()).unwrap();
        let signature = index.documents[0].symbols[0]
            .signature_documentation
            .as_mut()
            .expect("signature fixture");
        if !path.is_empty() {
            // The pinned crate's `Signature` lacks field 1; scip-java writes it first.
            signature
                .special_fields
                .mut_unknown_fields()
                .add_length_delimited(1, path.as_bytes().to_vec());
        }
        index.write_to_bytes().unwrap()
    }
    #[test]
    fn signature_relative_path_is_presentation_only() {
        let without = decoded(SIGNATURE).unwrap();
        for path in ["src/A.js", "src/Other.java", "../outside", ""] {
            let bytes = with_signature_path(SIGNATURE, path);
            if !path.is_empty() {
                assert_ne!(bytes, hex::decode(SIGNATURE).unwrap());
            }
            let with = custom(&bytes, b"function f(){g();}\n", 1).expect(path);
            assert_eq!(with.artifact_hashes, [sha(&bytes)]);
            assert_eq!(with.claims.len(), without.claims.len());
            for (claim, expected) in with.claims.iter().zip(&without.claims) {
                assert_eq!(claim.canonical_bytes, expected.canonical_bytes, "{path}");
                assert_eq!(claim.fact_digest, expected.fact_digest, "{path}");
                assert_eq!(claim.envelope.relative_path, "src/A.js");
            }
        }
    }
    #[test]
    fn signature_document_other_unknown_fields_still_reject() {
        let occ = hex::decode("0a0300090a120241231801").unwrap();
        let sig_with = |extra: Vec<u8>| {
            let mut sig = message(1, b"src/A.js");
            sig.extend(message(5, b"f()"));
            sig.extend(extra);
            base_doc(
                [
                    message(2, &occ),
                    message(3, &[message(1, b"A#"), message(7, &sig)].concat()),
                ]
                .concat(),
                1,
            )
        };
        let accepted = sig_with(vec![]);
        custom(&accepted, b"function f(){g();}\n", 1).expect("relative_path admits");
        for (name, extra) in [
            ("document symbols", message(3, &message(1, b"A#"))),
            ("position encoding", vec![0x30, 0x01]),
            ("unknown", vec![0xf8, 0x07, 0x01]),
            ("relative_path wire type", vec![0x08, 0x01]),
        ] {
            assert!(
                matches!(
                    custom(&sig_with(extra), b"function f(){g();}\n", 1),
                    Err(ScipCaptureError::InvalidArtifact(_))
                ),
                "{name}"
            );
        }
        assert_eq!(
            custom(
                &sig_with(message(1, b"src/A.js")),
                b"function f(){g();}\n",
                1
            )
            .unwrap_err(),
            limit("repeated field"),
            "repeated relative_path"
        );
    }
    #[test]
    fn signature_relative_path_invalid_utf8_rejects() {
        let occ = hex::decode("0a0300090a120241231801").unwrap();
        let mut sig = message(1, &[0x73, 0xff, 0xfe]);
        sig.extend(message(5, b"f()"));
        let symbol = [message(1, b"A#"), message(7, &sig)].concat();
        let document = base_doc([message(2, &occ), message(3, &symbol)].concat(), 1);
        assert_eq!(
            custom(&document, b"function f(){g();}\n", 1).unwrap_err(),
            invalid("signature relative path")
        );
        assert_eq!(
            custom(&message(3, &symbol), b"function f(){g();}\n", 1).unwrap_err(),
            invalid("signature relative path"),
            "external symbol signature"
        );
    }
    /// A minimal scip-java-shaped Java document: the signature `Document` carries
    /// `relative_path`, `language` and `text` with a signature-relative occurrence.
    #[test]
    fn scip_java_shaped_signature_document_admits() {
        let source: &[u8] = b"class A { void f() {} }\n";
        let path = "src/main/java/A.java";
        let class = "semanticdb maven . . A#";
        let method = "semanticdb maven . . A#f().";
        // Wire order as protobuf-java (scip-java) emits it: fields in number order, so the
        // signature `Document` starts with `relative_path` (1), then occurrences, language, text.
        let occurrence = |range: &[u8], symbol: &str| {
            [
                message(1, range),
                message(2, symbol.as_bytes()),
                vec![0x18, 0x01],
            ]
            .concat()
        };
        let artifact = |sig_path: &str| {
            let mut signature = Vec::new();
            if !sig_path.is_empty() {
                signature.extend(message(1, sig_path.as_bytes()));
            }
            signature.extend(message(2, &occurrence(&[0, 5, 6], method)));
            signature.extend(message(4, b"java"));
            signature.extend(message(5, b"void f()"));
            let info = [
                message(1, method.as_bytes()),
                vec![0x28, 26],
                message(6, b"f"),
                message(7, &signature),
                message(8, class.as_bytes()),
            ]
            .concat();
            let document = [
                message(1, path.as_bytes()),
                message(2, &occurrence(&[0, 6, 7], class)),
                message(2, &occurrence(&[0, 15, 16], method)),
                message(3, &info),
                message(4, b"java"),
            ]
            .concat();
            message(2, &document)
        };
        let with = custom_document(&artifact(path), source, 1, path, "java").unwrap();
        let without = custom_document(&artifact(""), source, 1, path, "java").unwrap();
        assert_eq!(with.claims.len(), 3);
        assert_eq!(
            with.claims
                .iter()
                .map(|c| (&c.canonical_bytes, &c.fact_digest))
                .collect::<Vec<_>>(),
            without
                .claims
                .iter()
                .map(|c| (&c.canonical_bytes, &c.fact_digest))
                .collect::<Vec<_>>()
        );
    }
}
