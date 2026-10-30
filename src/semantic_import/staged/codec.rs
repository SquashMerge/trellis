//! Stored forms of the stage's join columns. A canonical native syntax ID (`sid:v1:` and 32
//! lowercase hex digits) is stored as its 16 bytes and a 64-hex digest as its 32 bytes; any
//! other text is stored as text. The two forms never compare equal in SQLite, so lookups by
//! the encoded value are exact, and decoding returns the original text. A BLOB of the wrong
//! length is a corrupt stage row.
use crate::semantic_evidence as semantic;
use anyhow::{Result, ensure};
use rusqlite::types::{Value, ValueRef};

const SID_PREFIX: &str = "sid:v1:";

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn hex_bytes(hex: &str, bytes: usize) -> Option<Vec<u8>> {
    let hex = hex.as_bytes();
    if hex.len() != bytes * 2 {
        return None;
    }
    hex.chunks(2)
        .map(|pair| Some(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}

fn push_hex(text: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &byte in bytes {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 0x0f) as usize] as char);
    }
}

fn corrupt() -> anyhow::Error {
    anyhow::anyhow!("corrupt staged join row")
}

/// The stored form of a native syntax ID.
pub(crate) fn sid(id: &str) -> Value {
    match id
        .strip_prefix(SID_PREFIX)
        .and_then(|hex| hex_bytes(hex, 16))
    {
        Some(bytes) => Value::Blob(bytes),
        None => Value::Text(id.to_owned()),
    }
}

/// The byte length of the syntax ID a stored value decodes to, without decoding it.
pub(crate) fn sid_len(value: ValueRef<'_>) -> Result<u64> {
    match value {
        ValueRef::Blob(bytes) if bytes.len() == 16 => Ok((SID_PREFIX.len() + 32) as u64),
        ValueRef::Text(text) => Ok(text.len() as u64),
        _ => Err(corrupt()),
    }
}

/// Decodes a stored native syntax ID.
pub(crate) fn sid_text(value: ValueRef<'_>) -> Result<String> {
    match value {
        ValueRef::Blob(bytes) => {
            ensure!(bytes.len() == 16, corrupt());
            let mut text = String::with_capacity(SID_PREFIX.len() + 32);
            text.push_str(SID_PREFIX);
            push_hex(&mut text, bytes);
            Ok(text)
        }
        ValueRef::Text(text) => Ok(std::str::from_utf8(text)?.to_owned()),
        _ => Err(corrupt()),
    }
}

/// The stored form of a 64-hex digest such as a provenance ID.
pub(crate) fn digest(id: &str) -> Value {
    match hex_bytes(id, 32) {
        Some(bytes) => Value::Blob(bytes),
        None => Value::Text(id.to_owned()),
    }
}

/// Decodes a stored digest.
pub(crate) fn digest_text(value: ValueRef<'_>) -> Result<String> {
    match value {
        ValueRef::Blob(bytes) => {
            ensure!(bytes.len() == 32, corrupt());
            let mut text = String::with_capacity(64);
            push_hex(&mut text, bytes);
            Ok(text)
        }
        ValueRef::Text(text) => Ok(std::str::from_utf8(text)?.to_owned()),
        _ => Err(corrupt()),
    }
}

/// The stored rank of an evidence kind: its declared (`Ord`) position.
pub(crate) fn kind_rank(kind: semantic::EvidenceKind) -> i64 {
    kind as i64
}

/// Decodes a stored evidence-kind rank.
pub(crate) fn kind_of(rank: i64) -> Result<semantic::EvidenceKind> {
    use semantic::EvidenceKind::*;
    [
        MeasuredSyntax,
        DeclarationBinding,
        SemanticReference,
        TypeRelationship,
    ]
    .into_iter()
    .find(|kind| kind_rank(*kind) == rank)
    .ok_or_else(corrupt)
}

/// A fact row whose stored body carries an empty provenance ID: the row's column holds it.
pub(crate) trait ProvenanceOwned: serde::Serialize + serde::de::DeserializeOwned {
    fn provenance_id(&self) -> &str;
    fn provenance_id_mut(&mut self) -> &mut String;
}

macro_rules! provenance_owned {
    ($($ty:ty),*) => {$(
        impl ProvenanceOwned for $ty {
            fn provenance_id(&self) -> &str {
                &self.provenance_id
            }
            fn provenance_id_mut(&mut self) -> &mut String {
                &mut self.provenance_id
            }
        }
    )*};
}
provenance_owned!(
    semantic::Symbol,
    semantic::DeclarationBinding,
    semantic::Reference,
    semantic::CallBinding,
    semantic::TypeRelationship
);

/// The stored body of a fact row: the row serialized with its provenance ID left empty. The
/// ID is moved out for the serialization and back, so no copy of the row is made; the row is
/// unchanged on return, also on error.
pub(crate) fn fact_body<T: ProvenanceOwned>(row: &mut T) -> Result<String> {
    let id = std::mem::take(row.provenance_id_mut());
    let body = serde_json::to_string(&*row);
    *row.provenance_id_mut() = id;
    Ok(body?)
}

/// Decodes a stored fact row and restores its provenance ID from the row's column.
pub(crate) fn fact_row<T: ProvenanceOwned>(body: &str, provenance: ValueRef<'_>) -> Result<T> {
    let mut row: T = serde_json::from_str(body)?;
    ensure!(row.provenance_id_mut().is_empty(), corrupt());
    *row.provenance_id_mut() = digest_text(provenance)?;
    Ok(row)
}

/// The stored provenance template of one document: every field its provenance rows share,
/// with an empty ID, a placeholder kind and no derivation.
pub(crate) fn provenance_template(provenance: &semantic::Provenance) -> Result<String> {
    let mut template = provenance.clone();
    template.id.clear();
    template.evidence_kind = semantic::EvidenceKind::MeasuredSyntax;
    template.derived_from = None;
    Ok(serde_json::to_string(&template)?)
}

/// Rebuilds one provenance row from its document's template and its own columns.
pub(crate) fn provenance_row(
    template: &semantic::Provenance,
    id: ValueRef<'_>,
    kind: i64,
    proof: String,
    record_kind: &str,
    fact_digest: String,
) -> Result<semantic::Provenance> {
    ensure!(
        template.id.is_empty() && template.derived_from.is_none(),
        corrupt()
    );
    let mut row = template.clone();
    row.id = digest_text(id)?;
    row.evidence_kind = kind_of(kind)?;
    row.derived_from = Some(semantic::DerivedFrom {
        provenance_id: proof,
        record_kind: serde_json::from_value(serde_json::Value::String(record_kind.to_owned()))?,
        fact_digest,
    });
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(value: &Value) -> ValueRef<'_> {
        ValueRef::from(value)
    }

    #[test]
    fn canonical_ids_round_trip_through_their_bytes() {
        let id = format!("{SID_PREFIX}{}", "0123456789abcdef".repeat(2));
        let value = sid(&id);
        assert!(matches!(&value, Value::Blob(bytes) if bytes.len() == 16));
        assert_eq!(sid_len(stored(&value)).unwrap(), id.len() as u64);
        assert_eq!(sid_text(stored(&value)).unwrap(), id);
        let hash = "f".repeat(64);
        let value = digest(&hash);
        assert!(matches!(&value, Value::Blob(bytes) if bytes.len() == 32));
        assert_eq!(digest_text(stored(&value)).unwrap(), hash);
    }

    #[test]
    fn noncanonical_ids_stay_text_and_never_alias_bytes() {
        for id in [
            "sid:v1:ABCDEF0123456789abcdef0123456789",
            "sid:v1:short",
            "sid:v2:0123456789abcdef0123456789abcdef",
            "",
        ] {
            let value = sid(id);
            assert_eq!(value, Value::Text(id.to_owned()));
            assert_eq!(sid_text(stored(&value)).unwrap(), id);
            assert_eq!(sid_len(stored(&value)).unwrap(), id.len() as u64);
        }
        for hash in ["F".repeat(64), "f".repeat(63), "g".repeat(64)] {
            assert_eq!(digest(&hash), Value::Text(hash.clone()));
            assert_eq!(digest_text(stored(&digest(&hash))).unwrap(), hash);
        }
    }

    #[test]
    fn corrupt_stored_ids_are_typed_failures() {
        for value in [
            Value::Blob(vec![0; 15]),
            Value::Blob(vec![0; 32]),
            Value::Integer(1),
            Value::Null,
        ] {
            assert!(sid_text(stored(&value)).is_err());
            assert!(sid_len(stored(&value)).is_err());
        }
        for value in [Value::Blob(vec![0; 16]), Value::Real(1.0), Value::Null] {
            assert!(digest_text(stored(&value)).is_err());
        }
        assert!(kind_of(4).is_err());
        assert!(kind_of(-1).is_err());
        for kind in [
            semantic::EvidenceKind::MeasuredSyntax,
            semantic::EvidenceKind::DeclarationBinding,
            semantic::EvidenceKind::SemanticReference,
            semantic::EvidenceKind::TypeRelationship,
        ] {
            assert_eq!(kind_of(kind_rank(kind)).unwrap(), kind);
        }
    }

    fn symbol() -> semantic::Symbol {
        semantic::Symbol {
            key: semantic::SymbolKey {
                scheme: semantic::SymbolScheme::Scip,
                symbol: "pkg A#".into(),
                scope: semantic::SymbolScope::Global,
                document: None,
            },
            display_name: Some("A".into()),
            declarations: vec![],
            provenance_id: "a".repeat(64),
        }
    }

    #[test]
    fn fact_bodies_round_trip_and_reject_a_stored_provenance_id() {
        let mut row = symbol();
        let body = fact_body(&mut row).unwrap();
        // Serialized from the row itself, which keeps its ID.
        assert_eq!(row, symbol());
        assert!(!body.contains(&row.provenance_id));
        let column = digest(&row.provenance_id);
        let decoded: semantic::Symbol = fact_row(&body, stored(&column)).unwrap();
        assert_eq!(decoded, row);
        // A body that still names a provenance ID is not one this stage wrote.
        let full = serde_json::to_string(&row).unwrap();
        assert!(fact_row::<semantic::Symbol>(&full, stored(&column)).is_err());
        assert!(fact_row::<semantic::Symbol>("{", stored(&column)).is_err());
        assert!(fact_row::<semantic::Symbol>(&body, stored(&Value::Blob(vec![1; 3]))).is_err());
    }

    #[test]
    fn provenance_rows_round_trip_through_their_template() {
        let document = crate::native_evidence::DocumentKey {
            source_set_id: "source".into(),
            language: "java".into(),
            path: "A.java".into(),
        };
        let row = semantic::Provenance {
            id: "b".repeat(64),
            producer_id: "producer".into(),
            document,
            revision_id: "revision".into(),
            content_hash: "c".repeat(64),
            evidence_kind: semantic::EvidenceKind::SemanticReference,
            basis: None,
            freshness: semantic::Freshness::Fresh,
            derived_from: Some(semantic::DerivedFrom {
                provenance_id: "d".repeat(64),
                record_kind: semantic::RecordKind::Occurrence,
                fact_digest: "e".repeat(64),
            }),
        };
        let template: semantic::Provenance =
            serde_json::from_str(&provenance_template(&row).unwrap()).unwrap();
        let id = digest(&row.id);
        let rebuilt = provenance_row(
            &template,
            stored(&id),
            kind_rank(row.evidence_kind),
            "d".repeat(64),
            "occurrence",
            "e".repeat(64),
        )
        .unwrap();
        assert_eq!(rebuilt, row);
        assert!(
            provenance_row(
                &template,
                stored(&id),
                9,
                "d".into(),
                "occurrence",
                "e".into()
            )
            .is_err()
        );
        assert!(
            provenance_row(&template, stored(&id), 2, "d".into(), "bogus", "e".into()).is_err()
        );
        assert!(
            provenance_row(&row, stored(&id), 2, "d".into(), "occurrence", "e".into()).is_err()
        );
    }
}
