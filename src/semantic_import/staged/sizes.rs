//! Checked logical owned-byte visitors. Inline struct storage is counted only at the root;
//! nested visitors count payload. This is not allocator slack or total process heap.
use super::*;
use std::mem::size_of;

pub(in crate::semantic_import) fn sum(parts: &[u64]) -> Result<u64> {
    parts.iter().try_fold(0u64, |total, part| {
        total
            .checked_add(*part)
            .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()).into())
    })
}
fn slots<T>(count: usize) -> Result<u64> {
    (count as u64)
        .checked_mul(size_of::<T>() as u64)
        .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()).into())
}
pub(in crate::semantic_import) fn map_entries<K, V>(count: usize) -> Result<u64> {
    sum(&[slots::<(K, V)>(count)?, slots::<[usize; 4]>(count)?])
}
fn text(value: &str) -> u64 {
    value.len() as u64
}
fn optional(value: &Option<String>) -> u64 {
    value.as_ref().map_or(0, |s| text(s))
}
fn strings(values: &[String]) -> Result<u64> {
    values
        .iter()
        .try_fold(slots::<String>(values.len())?, |total, s| {
            sum(&[total, text(s)])
        })
}
pub(in crate::semantic_import) fn document(value: &native::DocumentKey) -> Result<u64> {
    sum(&[
        text(&value.source_set_id),
        text(&value.language),
        text(&value.path),
    ])
}
fn signature(value: &native::Signature) -> Result<u64> {
    strings(&value.parameter_types)
}
fn key(value: &native::Key) -> Result<u64> {
    sum(&[
        text(&value.kind),
        optional(&value.name),
        value.signature.as_ref().map_or(Ok(0), signature)?,
    ])
}
fn header(value: &native::Header) -> Result<u64> {
    let parameters = value.parameters.iter().try_fold(
        slots::<native::Parameter>(value.parameters.len())?,
        |total, p| sum(&[total, optional(&p.name), optional(&p.type_name)]),
    )?;
    sum(&[
        text(&value.kind),
        optional(&value.name),
        strings(&value.modifiers)?,
        strings(&value.type_parameters)?,
        parameters,
        optional(&value.result_type),
        strings(&value.bases)?,
    ])
}
fn declaration_payload(value: &native::Declaration) -> Result<u64> {
    let ancestors = value
        .ancestors
        .iter()
        .try_fold(slots::<native::Key>(value.ancestors.len())?, |total, k| {
            sum(&[total, key(k)?])
        })?;
    sum(&[
        text(&value.syntax_id),
        document(&value.document)?,
        text(&value.revision_id),
        text(&value.kind),
        optional(&value.name),
        optional(&value.lookup_key),
        ancestors,
        key(&value.key)?,
        header(&value.header)?,
        text(&value.provenance_id),
    ])
}
pub(in crate::semantic_import) fn declaration(value: &native::Declaration) -> Result<u64> {
    sum(&[
        size_of::<native::Declaration>() as u64,
        declaration_payload(value)?,
    ])
}
pub(in crate::semantic_import) fn native_reference(value: &native::Reference) -> Result<u64> {
    sum(&[
        size_of::<native::Reference>() as u64,
        text(&value.id),
        text(&value.owner_syntax_id),
        document(&value.document)?,
        text(&value.revision_id),
        text(&value.spelling),
        text(&value.lookup_key),
        text(&value.provenance_id),
    ])
}

pub(in crate::semantic_import) fn mutation(
    rows: &crate::store::NativeDocumentRows,
    changes: DestinationMutations,
) -> Result<u64> {
    let declaration = if changes.duplicate_b_target {
        rows.declarations
            .iter()
            .find(|d| d.document.path == "src/B.js" && d.name.as_deref() == Some("target"))
            .map_or(Ok(0), declaration)?
    } else {
        0
    };
    let reference = if changes.duplicate_a_reference {
        rows.references
            .iter()
            .find(|r| r.document.path == "src/A.js" && r.spelling == "target")
            .map_or(Ok(0), |row| {
                sum(&[
                    native_reference(row)?,
                    row.id.len() as u64,
                    "-candidate-only".len() as u64,
                ])
            })?
    } else {
        0
    };
    // Appending a synthetic row can grow the existing vector while its old buffer is live.
    sum(&[
        declaration,
        reference,
        if declaration == 0 {
            0
        } else {
            slots::<native::Declaration>(
                rows.declarations
                    .len()
                    .checked_mul(2)
                    .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?
                    .max(4),
            )?
        },
        if reference == 0 {
            0
        } else {
            slots::<native::Reference>(
                rows.references
                    .len()
                    .checked_mul(2)
                    .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?
                    .max(4),
            )?
        },
    ])
}

pub(in crate::semantic_import) fn method(value: &MethodSummary) -> Result<u64> {
    sum(&[
        size_of::<MethodSummary>() as u64,
        declaration_payload(&value.method)?,
    ])
}
// serde's fresh nested Vecs grow geometrically; this adds their requested spare slots
// to the field visitor, without multiplying unrelated string payloads.
fn decode_spare<T>(length: usize) -> Result<u64> {
    let capacity = if length == 0 {
        0
    } else {
        length
            .checked_mul(2)
            .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?
            .max(4)
    };
    slots::<T>(capacity - length)
}
fn signature_spare(value: &native::Key) -> Result<u64> {
    value.signature.as_ref().map_or(Ok(0), |signature| {
        decode_spare::<String>(signature.parameter_types.len())
    })
}
pub(in crate::semantic_import) fn decoded_method(value: &MethodSummary) -> Result<u64> {
    let declaration = &value.method;
    let ancestor_spare = declaration.ancestors.iter().try_fold(
        decode_spare::<native::Key>(declaration.ancestors.len())?,
        |total, key| sum(&[total, signature_spare(key)?]),
    )?;
    let header = &declaration.header;
    sum(&[
        method(value)?,
        ancestor_spare,
        signature_spare(&declaration.key)?,
        decode_spare::<String>(header.modifiers.len())?,
        decode_spare::<String>(header.type_parameters.len())?,
        decode_spare::<native::Parameter>(header.parameters.len())?,
        decode_spare::<String>(header.bases.len())?,
    ])
}

pub(in crate::semantic_import) fn symbol_key(value: &semantic::SymbolKey) -> Result<u64> {
    sum(&[
        text(&value.symbol),
        value.document.as_ref().map_or(Ok(0), document)?,
    ])
}
pub(in crate::semantic_import) fn target_payload(value: &semantic::Target) -> Result<u64> {
    match value {
        semantic::Target::Internal {
            syntax_id,
            document: doc,
            revision_id,
        } => sum(&[text(syntax_id), document(doc)?, text(revision_id)]),
        semantic::Target::External { symbol } => symbol_key(symbol),
    }
}
pub(in crate::semantic_import) fn target(value: &semantic::Target) -> Result<u64> {
    sum(&[size_of::<semantic::Target>() as u64, target_payload(value)?])
}
pub(in crate::semantic_import) fn targets(values: &[semantic::Target]) -> Result<u64> {
    values
        .iter()
        .try_fold(slots::<semantic::Target>(values.len())?, |total, t| {
            sum(&[total, target_payload(t)?])
        })
}
fn join_payload(value: &semantic::Join) -> Result<u64> {
    sum(&[
        document(&value.anchor.document)?,
        text(&value.anchor.revision_id),
        text(&value.anchor.content_hash),
        strings(&value.candidate_ids)?,
        optional(&value.diagnostic),
    ])
}
pub(in crate::semantic_import) fn join(value: &semantic::Join) -> Result<u64> {
    sum(&[size_of::<semantic::Join>() as u64, join_payload(value)?])
}
pub(in crate::semantic_import) fn occurrence(value: &OccurrenceRow) -> Result<u64> {
    sum(&[
        size_of::<OccurrenceRow>() as u64,
        text(&value.proof_id),
        text(&value.fact_digest),
        document(&value.document)?,
        text(&value.symbol),
        join_payload(&value.binding)?,
        join_payload(&value.reference)?,
        join_payload(&value.call)?,
    ])
}
pub(in crate::semantic_import) fn relationship(value: &RelationshipRow) -> Result<u64> {
    sum(&[
        size_of::<RelationshipRow>() as u64,
        text(&value.proof_id),
        optional(&value.source_symbol),
        optional(&value.target_symbol),
    ])
}
pub(in crate::semantic_import) fn symbol(value: &semantic::Symbol) -> Result<u64> {
    sum(&[
        size_of::<semantic::Symbol>() as u64,
        symbol_key(&value.key)?,
        optional(&value.display_name),
        targets(&value.declarations)?,
        text(&value.provenance_id),
    ])
}
pub(in crate::semantic_import) fn binding(value: &semantic::DeclarationBinding) -> Result<u64> {
    let symbols = value.symbols.iter().try_fold(
        slots::<semantic::SymbolKey>(value.symbols.len())?,
        |total, s| sum(&[total, symbol_key(s)?]),
    )?;
    sum(&[
        size_of::<semantic::DeclarationBinding>() as u64,
        optional(&value.syntax_id),
        symbols,
        join_payload(&value.join)?,
        text(&value.provenance_id),
    ])
}
pub(in crate::semantic_import) fn reference(value: &semantic::Reference) -> Result<u64> {
    sum(&[
        size_of::<semantic::Reference>() as u64,
        text(&value.id),
        text(&value.owner_syntax_id),
        document(&value.document)?,
        text(&value.revision_id),
        text(&value.spelling),
        text(&value.lookup_key),
        slots::<semantic::Role>(value.roles.len())?,
        value
            .declared_target
            .as_ref()
            .map_or(Ok(0), target_payload)?,
        targets(&value.candidates)?,
        text(&value.provenance_id),
    ])
}
pub(in crate::semantic_import) fn call(value: &semantic::CallBinding) -> Result<u64> {
    sum(&[
        size_of::<semantic::CallBinding>() as u64,
        optional(&value.call_id),
        join_payload(&value.join)?,
        value
            .declared_target
            .as_ref()
            .map_or(Ok(0), target_payload)?,
        targets(&value.candidates)?,
        targets(&value.possible_dispatch)?,
        text(&value.provenance_id),
    ])
}
pub(in crate::semantic_import) fn type_relationship(
    value: &semantic::TypeRelationship,
) -> Result<u64> {
    sum(&[
        size_of::<semantic::TypeRelationship>() as u64,
        target_payload(&value.source)?,
        target_payload(&value.target)?,
        text(&value.provenance_id),
    ])
}
