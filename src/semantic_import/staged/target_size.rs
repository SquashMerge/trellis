//! Field-derived canonical Target and sort scratch, measured before creating JSON values.
use super::group_size::quoted;
use super::*;
use sizes::{map_entries, sum};
use std::mem::size_of;

fn object_length(fields: &[(&str, u64)]) -> Result<u64> {
    fields.iter().try_fold(
        sum(&[
            2,
            fields.len() as u64,
            fields.len().saturating_sub(1) as u64,
        ])?,
        |total, (key, value)| sum(&[total, quoted(key)?, *value]),
    )
}
fn object_storage(keys: &[&str]) -> Result<u64> {
    keys.iter().try_fold(
        sum(&[
            map_entries::<String, serde_json::Value>(keys.len())?,
            (keys.len().max(4) * size_of::<&String>()) as u64,
        ])?,
        |total, key| sum(&[total, key.len() as u64]),
    )
}
fn document_shape(document: &native::DocumentKey) -> Result<(u64, u64)> {
    Ok((
        object_length(&[
            ("sourceSetId", quoted(&document.source_set_id)?),
            ("language", quoted(&document.language)?),
            ("path", quoted(&document.path)?),
        ])?,
        sum(&[
            object_storage(&["sourceSetId", "language", "path"])?,
            sizes::document(document)?,
        ])?,
    ))
}
fn symbol_shape(symbol: &semantic::SymbolKey) -> Result<(u64, u64)> {
    let scope = match symbol.scope {
        semantic::SymbolScope::Global => "global",
        semantic::SymbolScope::Document => "document",
    };
    let (document, storage) = symbol
        .document
        .as_ref()
        .map_or(Ok((4, 0)), document_shape)?;
    Ok((
        object_length(&[
            ("scheme", quoted("scip")?),
            ("symbol", quoted(&symbol.symbol)?),
            ("scope", quoted(scope)?),
            ("document", document),
        ])?,
        sum(&[
            object_storage(&["scheme", "symbol", "scope", "document"])?,
            4,
            symbol.symbol.len() as u64,
            scope.len() as u64,
            storage,
        ])?,
    ))
}
pub(in crate::semantic_import) fn shape(target: &semantic::Target) -> Result<(u64, u64)> {
    let (length, storage) = match target {
        semantic::Target::Internal {
            syntax_id,
            document,
            revision_id,
        } => {
            let (doc, storage) = document_shape(document)?;
            (
                object_length(&[
                    ("kind", quoted("internal")?),
                    ("syntaxId", quoted(syntax_id)?),
                    ("document", doc),
                    ("revisionId", quoted(revision_id)?),
                ])?,
                sum(&[
                    object_storage(&["kind", "syntaxId", "document", "revisionId"])?,
                    8,
                    syntax_id.len() as u64,
                    revision_id.len() as u64,
                    storage,
                ])?,
            )
        }
        semantic::Target::External { symbol } => {
            let (symbol, storage) = symbol_shape(symbol)?;
            (
                object_length(&[("kind", quoted("external")?), ("symbol", symbol)])?,
                sum(&[object_storage(&["kind", "symbol"])?, 8, storage])?,
            )
        }
    };
    Ok((
        length,
        sum(&[
            storage,
            size_of::<serde_json::Value>() as u64,
            size_of::<String>() as u64,
            12,
        ])?,
    ))
}
/// Pinned Rust 1.99.1 driftsort has minimum 48 scratch slots beyond insertion sort.
pub(in crate::semantic_import) fn stable_sort<T>(count: usize) -> Result<u64> {
    if count <= 20 {
        return Ok(0);
    }
    (count.max(48) as u64)
        .checked_mul(size_of::<T>() as u64)
        .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()).into())
}
pub(in crate::semantic_import) fn target_slots(count: usize) -> Result<u64> {
    (count as u64)
        .checked_mul(size_of::<semantic::Target>() as u64)
        .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()).into())
}
pub(in crate::semantic_import) fn sort(targets: &[semantic::Target]) -> Result<u64> {
    if targets.len() < 2 {
        return Ok(0);
    }
    let mut capacity = 0;
    let mut scratch = 0;
    for target in targets {
        let (length, storage) = shape(target)?;
        capacity = capacity.max(sum(&[length, length])?.max(8));
        scratch = scratch.max(storage);
    }
    sum(&[
        capacity,
        capacity,
        // The second output can grow while its old buffer and the first key coexist.
        capacity,
        scratch,
        2 * size_of::<Vec<u8>>() as u64,
        stable_sort::<semantic::Target>(targets.len())?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_shapes_match_canonical_bytes_and_sort_minimum() {
        let document = native::DocumentKey {
            source_set_id: "source\n".into(),
            language: "java".into(),
            path: "é\\\"🙂.java".into(),
        };
        for target in [
            semantic::Target::Internal {
                syntax_id: "id\t".into(),
                document: document.clone(),
                revision_id: "revision".into(),
            },
            semantic::Target::External {
                symbol: super::super::super::symbol_key("pkg é#", &document),
            },
            semantic::Target::External {
                symbol: super::super::super::symbol_key("local \"id", &document),
            },
        ] {
            let (length, scratch) = shape(&target).unwrap();
            assert_eq!(
                length,
                crate::native_ids::canonical(&serde_json::to_value(&target).unwrap()).len() as u64
            );
            assert!(scratch > 0);
            assert_eq!(sort(&[target]).unwrap(), 0);
        }
        assert_eq!(stable_sort::<semantic::Target>(20).unwrap(), 0);
        assert!(stable_sort::<semantic::Target>(21).unwrap() >= target_slots(48).unwrap());
    }
}
