//! Authenticated, document-only SCIP 0.10.0 capture. No destination coordinates are emitted.
use crate::{
    model::CancelFlag,
    native_ids::{canonical, digest},
};
use protobuf::Message;
use scip::types::{self, occurrence};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

pub const DECODER_VERSION: &str = "scip-0.10.0/CapturedScipFactV1/1";
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
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
pub(crate) struct AuthenticatedScipInput<'a> {
    pub artifact_bytes: &'a [u8],
    pub artifact_hash: String,
    pub producer_id: String,
    pub profile_id: String,
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
}
#[derive(Debug, Clone)]
pub struct CapturedScipInventory {
    pub artifact_hash: String,
    pub decoder_version: String,
    pub source_manifest_hash: String,
    pub completion: Completion,
    pub exclusions: Vec<Exclusion>,
    pub claims: Vec<CapturedClaim>,
}
impl CapturedScipInventory {
    pub fn lookup(
        &self,
        proof_id: &str,
        record_kind: &str,
        fact_digest: &str,
    ) -> Result<&CapturedClaim, ScipCaptureError> {
        let mut found = None;
        for claim in &self.claims {
            if claim.captured_proof_id == proof_id {
                if claim.envelope.record_kind != record_kind || claim.fact_digest != fact_digest {
                    continue;
                }
                if found.is_some() {
                    return Err(ScipCaptureError::NonUniqueProof);
                }
                found = Some(claim);
            }
        }
        found.ok_or(ScipCaptureError::MissingProof)
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
        (Index, 1) => (2, Message(Metadata), 1),
        (Index, 2) => (2, Message(Document), 1000),
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
        (Signature, 4 | 5) => (2, Bytes, 1),
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
#[derive(Default)]
struct WireBudget {
    fields: usize,
    relationships: usize,
    candidates: usize,
}
fn wire(
    b: &[u8],
    node: Node,
    depth: usize,
    budget: &mut WireBudget,
    flag: &CancelFlag,
) -> Result<(), ScipCaptureError> {
    if depth > 16 {
        return Err(limit("message depth"));
    }
    let mut i = 0;
    let mut counts = BTreeMap::<u64, usize>::new();
    let mut ranges = 0u8;
    while i < b.len() {
        budget.fields += 1;
        if budget.fields > 1_000_000 {
            return Err(limit("wire fields"));
        }
        if budget.fields.is_multiple_of(1024) {
            cancelled(flag)?;
        }
        let tag = varint(b, &mut i)?;
        let n = tag >> 3;
        let wt = (tag & 7) as u8;
        let (expected, kind, max) = field(node, n).ok_or_else(|| invalid("unknown field"))?;
        if wt != expected && !(matches!(kind, Wire::Packed) && wt == 2) {
            return Err(invalid("field wire type"));
        }
        if matches!(node, Node::Occurrence) && matches!(n, 1 | 7 | 8 | 9 | 10 | 11) {
            let mask = if matches!(n, 1 | 8 | 9) { 1 } else { 2 };
            if n >= 8 && ranges & mask != 0
                || n == 1 && ranges & 4 != 0
                || n == 7 && ranges & 8 != 0
            {
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
            if budget.relationships > 20000 || budget.candidates > 100000 {
                return Err(limit("global entries"));
            }
        }
        let count = counts.entry(n).or_default();
        if wt == 0 || !matches!(kind, Wire::Packed) {
            *count += 1;
        }
        if *count > max {
            return Err(limit("repeated field"));
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
                if budget.fields > 1_000_000 || *count > max {
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
fn next_newline(bytes: &[u8], flag: &CancelFlag) -> Result<Option<usize>, ScipCaptureError> {
    for (chunk_index, chunk) in bytes.chunks(65536).enumerate() {
        cancelled(flag)?;
        if let Some(at) = chunk.iter().position(|b| *b == b'\n') {
            return Ok(Some(chunk_index * 65536 + at));
        }
    }
    Ok(None)
}
fn validate_positions(
    text: &str,
    enc: u8,
    positions: &mut Vec<(i32, i32)>,
    flag: &CancelFlag,
) -> Result<(), ScipCaptureError> {
    cancelled(flag)?;
    if positions.is_empty() {
        return Ok(());
    }
    positions.sort_unstable();
    positions.dedup();
    let bytes = text.as_bytes();
    let mut current_line = 0usize;
    let mut line_start = 0usize;
    let mut loaded_line = None;
    let mut chars = "".chars();
    let mut measured = 0usize;
    let mut steps = 0usize;
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
            loaded_line = Some(target);
        }
        while measured < column as usize {
            let c = chars
                .next()
                .ok_or_else(|| invalid("range column outside source"))?;
            steps += 1;
            if steps.is_multiple_of(1024) {
                #[cfg(test)]
                position_walk_hook(flag);
                cancelled(flag)?;
            }
            measured += match enc {
                1 => c.len_utf8(),
                2 => c.len_utf16(),
                3 => 1,
                _ => return Err(invalid("position encoding")),
            };
        }
        if measured != column as usize {
            return Err(invalid("split codepoint"));
        }
    }
    Ok(())
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
// Conservative envelope expansion bound before any relationship JSON or canonical bytes exist.
// Strings may be copied into JSON and canonical output; fixed overhead covers keys/escaping.
const MAX_CLAIM_BYTES: usize = 128 * 1024 * 1024;
fn reserve_claim(total: &mut usize, strings: &[&str]) -> Result<(), ScipCaptureError> {
    let bytes = strings
        .iter()
        .try_fold(512usize, |size, text| {
            size.checked_add(text.len().checked_mul(6)?)
        })
        .ok_or_else(|| limit("claim expansion"))?;
    *total = total
        .checked_add(bytes)
        .filter(|v| *v <= MAX_CLAIM_BYTES)
        .ok_or_else(|| limit("claim expansion"))?;
    Ok(())
}
fn claim_budget(index: &types::Index, flag: &CancelFlag) -> Result<(), ScipCaptureError> {
    let mut total = 0usize;
    let mut traversed = 0usize;
    for d in &index.documents {
        for o in &d.occurrences {
            reserve_claim(&mut total, &[&d.relative_path, &d.language, &o.symbol])?;
            traversed += 1;
            if traversed.is_multiple_of(1024) {
                cancelled(flag)?;
            }
        }
        for s in &d.symbols {
            reserve_claim(
                &mut total,
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
                    &[&d.relative_path, &d.language, &s.symbol, &r.symbol],
                )?;
                traversed += 1;
                if traversed.is_multiple_of(1024) {
                    cancelled(flag)?;
                }
            }
        }
    }
    Ok(())
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
        unknown(&sig.special_fields)?;
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
fn emit(
    claims: &mut Vec<CapturedClaim>,
    input: &AuthenticatedScipInput<'_>,
    d: &types::Document,
    kind: &str,
    raw: Value,
) {
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
            &json!({"producerId":input.producer_id,"artifactHash":input.artifact_hash,"sourceSetId":input.source_set_id,"sourceRevision":input.source_revision,"language":d.language,"relativePath":d.relative_path,"factDigest":fact}),
        ),
    );
    claims.push(CapturedClaim {
        envelope,
        canonical_bytes: bytes,
        fact_digest: fact,
        captured_proof_id: proof,
        language: d.language.clone(),
    });
}
pub(crate) fn decode_authenticated(
    input: &AuthenticatedScipInput<'_>,
    flag: &CancelFlag,
) -> Result<CapturedScipInventory, ScipCaptureError> {
    cancelled(flag)?;
    if input.artifact_bytes.len() > 64 * 1024 * 1024 {
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
    cancelled(flag)?;
    wire(
        input.artifact_bytes,
        Node::Index,
        0,
        &mut WireBudget::default(),
        flag,
    )?;
    cancelled(flag)?;
    let index = types::Index::parse_from_bytes(input.artifact_bytes)
        .map_err(|_| invalid("protobuf parse"))?;
    unknown(&index.special_fields)?;
    if let Some(meta) = index.metadata.as_ref() {
        unknown(&meta.special_fields)?;
        if known(meta.version)? != input.protocol_version
            || known(meta.text_document_encoding)? != input.text_document_encoding
            || !meta.project_root.is_empty() && meta.project_root != input.project_root_uri
        {
            return Err(invalid("metadata binding"));
        }
        if let Some(tool) = meta.tool_info.as_ref() {
            unknown(&tool.special_fields)?;
            if !tool.name.is_empty() && tool.name != input.producer_id
                || !tool.version.is_empty() && tool.version != input.producer_version
            {
                return Err(invalid("tool binding"));
            }
        }
    }
    if index.documents.len() > 1000 || index.external_symbols.len() > 10000 {
        return Err(limit("index entries"));
    }
    claim_budget(&index, flag)?;
    let mut count = 0usize;
    let mut traversed = 0usize;
    let mut relationships = 0usize;
    for s in &index.external_symbols {
        traversed += 1;
        if traversed.is_multiple_of(1024) {
            cancelled(flag)?;
        }
        let _ = symbol(s, input.position_encoding, flag, &mut traversed, false)?;
        relationships += s.relationships.len();
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
    let mut seen = BTreeSet::new();
    let mut claims = Vec::new();
    for d in &index.documents {
        cancelled(flag)?;
        unknown(&d.special_fields)?;
        if !seen.insert(d.relative_path.as_str()) {
            return Err(invalid("duplicate document path"));
        }
        let source = admitted
            .get(d.relative_path.as_str())
            .ok_or_else(|| invalid("unlisted document"))?;
        if source.language != d.language
            || d.text.as_bytes() != source.source_bytes && !d.text.is_empty()
        {
            return Err(invalid("document source binding"));
        }
        let raw = known(d.position_encoding)?;
        if !(0..=3).contains(&raw) || raw != 0 && raw != i32::from(input.position_encoding) {
            return Err(invalid("position encoding"));
        }
        if d.occurrences.len() > 10000 || d.symbols.len() > 10000 {
            return Err(limit("document entries"));
        }
        let text =
            std::str::from_utf8(source.source_bytes).map_err(|_| invalid("non-UTF8 source"))?;
        let mut positions = Vec::with_capacity(d.occurrences.len() * 4);
        for o in &d.occurrences {
            count += 1;
            if count > 100000 {
                return Err(limit("candidate facts"));
            }
            if count.is_multiple_of(1024) {
                cancelled(flag)?;
            }
            emit(
                &mut claims,
                input,
                d,
                "occurrence",
                occurrence(o, &mut positions)?,
            );
        }
        validate_positions(text, input.position_encoding, &mut positions, flag)?;
        for s in &d.symbols {
            traversed += 1;
            if traversed.is_multiple_of(1024) {
                cancelled(flag)?;
            }
            let (info, rels) = symbol(s, input.position_encoding, flag, &mut traversed, true)?;
            count += 1;
            relationships += rels.len();
            if count + rels.len() > 100000 {
                return Err(limit("candidate facts"));
            }
            emit(&mut claims, input, d, "symbolInformation", info);
            for r in rels {
                count += 1;
                if count.is_multiple_of(1024) {
                    cancelled(flag)?;
                }
                emit(&mut claims, input, d, "relationship", r);
            }
        }
    }
    if relationships > 20000 {
        return Err(limit("relationships"));
    }
    if seen.len() != admitted.len() {
        return Err(invalid("missing artifact document"));
    }
    claims.sort_by(|a, b| {
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
    claims.dedup_by(|a, b| a.language == b.language && a.canonical_bytes == b.canonical_bytes);
    let mut digests = BTreeMap::new();
    for claim in &claims {
        if let Some(bytes) = digests.insert(&claim.fact_digest, &claim.canonical_bytes)
            && bytes != &claim.canonical_bytes
        {
            return Err(ScipCaptureError::DigestCollision);
        }
    }
    Ok(CapturedScipInventory {
        artifact_hash: input.artifact_hash.clone(),
        decoder_version: input.decoder_version.into(),
        source_manifest_hash: input.source_manifest_hash.clone(),
        completion: input.completion,
        exclusions: input.exclusions.clone(),
        claims,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicBool};
    const FULL: &str = "12580a087372632f412e6a73220a6a617661736372697074120b0a0300090a120241231801120c42060800100d180e12024223120e4a080800100d1800200e120242231a130a02412322080a024223100118012811320166300112180a087372632f422e6a73220a6a6176617363726970743001";
    const EXTERNAL: &str = "12580a087372632f412e6a73220a6a617661736372697074120b0a0300090a120241231801120c42060800100d180e12024223120e4a080800100d1800200e120242231a130a02412322080a024223100118012811320166300112180a087372632f422e6a73220a6a61766173637269707430011a0b0a0965787465726e616c23";
    const SIGNATURE: &str = "124c0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a270a02412328113201663a1c220a6a6176617363726970742a0366282912090a0300000112024123";
    const OMITTED: &str = "122e0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a090a0241232811320166";
    const LEGACY: &str =
        "122a0a087372632f412e6a73220a6a61766173637269707412100a03000a0e12076c6f63616c203018013001";
    const BRANCHES: &str = "12440a087372632f412e6a73220a6a61766173637269707412120a04000d000e1202422352060800100c1810121642060800100d180e120242235a080800100c180020103001";
    fn decoded(wire_hex: &str) -> Result<CapturedScipInventory, ScipCaptureError> {
        let bytes = hex::decode(wire_hex).unwrap();
        let a: &[u8] = if wire_hex == LEGACY {
            b"0123456789call\n"
        } else {
            b"function f(){g();}\n"
        };
        let b = b"function g(){}\n";
        let docs = if wire_hex == FULL || wire_hex == EXTERNAL {
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
        };
        let input = AuthenticatedScipInput {
            artifact_hash: sha(&bytes),
            artifact_bytes: &bytes,
            producer_id: "scip-test".into(),
            profile_id: "synthetic-test-profile".into(),
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
            documents: docs,
        };
        decode_authenticated(&input, &Arc::new(AtomicBool::new(false)))
    }
    fn var(mut x: usize) -> Vec<u8> {
        let mut b = Vec::new();
        while x >= 128 {
            b.push((x as u8) | 128);
            x >>= 7;
        }
        b.push(x as u8);
        b
    }
    fn message(n: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![(n << 3) | 2];
        out.extend(var(body.len()));
        out.extend(body);
        out
    }
    fn base_doc(mut fields: Vec<u8>, encoding: u8) -> Vec<u8> {
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
        let input = AuthenticatedScipInput {
            artifact_hash: sha(bytes),
            artifact_bytes: bytes,
            producer_id: "scip-test".into(),
            profile_id: "synthetic-test-profile".into(),
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
                language: "javascript".into(),
                relative_path: "src/A.js".into(),
                source_bytes: source,
                content_hash: sha(source),
            }],
        };
        decode_authenticated(&input, &Arc::new(AtomicBool::new(false)))
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
    fn scip_unknown_enums_and_duplicate_documents_reject() {
        let mut duplicate = hex::decode(FULL).unwrap();
        duplicate
            .extend(hex::decode("12180a087372632f422e6a73220a6a6176617363726970743001").unwrap());
        assert!(matches!(
            decoded(&hex::encode(duplicate)),
            Err(ScipCaptureError::InvalidArtifact(_))
        ));
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
        let repeated = std::iter::repeat_n([0x12, 0], 100001)
            .flatten()
            .collect::<Vec<_>>();
        let mut budget = WireBudget::default();
        assert!(matches!(
            wire(&repeated, Node::Document, 0, &mut budget, &flag),
            Err(ScipCaptureError::LimitExceeded(_))
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
        let mismatched_tool = [message(1, b"wrong-producer"), message(2, b"1.0.0")].concat();
        let metadata = message(2, &mismatched_tool);
        assert!(matches!(
            custom(
                &[message(1, &metadata), doc.clone()].concat(),
                b"function f(){g();}\n",
                1
            ),
            Err(ScipCaptureError::InvalidArtifact(_))
        ));
        let tool = message(2, b"wrong-version");
        assert!(matches!(
            custom(
                &[message(1, &message(2, &tool)), doc].concat(),
                b"function f(){g();}\n",
                1
            ),
            Err(ScipCaptureError::InvalidArtifact(_))
        ));
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
        let negative = message(2, &[0x0a, 0x03, 0x00, 0x09, 0xff]);
        assert!(custom(&base_doc(negative, 1), source, 1).is_err());
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
        let mut index = types::Index::new();
        index.documents.push(doc);
        assert!(matches!(
            claim_budget(&index, &Arc::new(AtomicBool::new(false))),
            Err(ScipCaptureError::LimitExceeded(_))
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
}
