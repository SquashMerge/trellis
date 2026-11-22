//! Lossless database forms of native syntax and occurrence handles (16 bytes) and of the
//! semantic tables' 64-hex digests (32 bytes).
//!
//! The wire representation remains the canonical lowercase hex text. A BLOB compares
//! byte-wise, so stored order is the order of its canonical lowercase hex text.
use anyhow::{Context, Result};

use super::SelectedIntegrity;

const HANDLE_BYTES: usize = 16;
const DIGEST_BYTES: usize = 32;

/// The bytes of exactly `bytes * 2` lowercase hex digits; anything else is `None`.
fn hex_bytes(hex: &[u8], bytes: usize) -> Option<Vec<u8>> {
    if hex.len() != bytes * 2 {
        return None;
    }
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }
    hex.as_chunks::<2>()
        .0
        .iter()
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

fn encode(value: &str, prefix: &str) -> Option<Vec<u8>> {
    hex_bytes(value.strip_prefix(prefix)?.as_bytes(), HANDLE_BYTES)
}

fn decode(value: &[u8], prefix: &str) -> Result<String> {
    if value.len() != HANDLE_BYTES {
        return Err(
            SelectedIntegrity("incompatible_index: invalid native ID BLOB length".into()).into(),
        );
    }
    let mut result = String::with_capacity(prefix.len() + 2 * HANDLE_BYTES);
    result.push_str(prefix);
    push_hex(&mut result, value);
    Ok(result)
}

/// Malformed caller text does not name a stored syntax handle.
pub(crate) fn encode_sid(value: &str) -> Option<Vec<u8>> {
    encode(value, "sid:v1:")
}

/// Malformed caller text does not name a stored occurrence handle.
pub(crate) fn encode_occ(value: &str) -> Option<Vec<u8>> {
    encode(value, "occ:v2:")
}

/// Decode a stored syntax handle, rejecting corrupt BLOBs.
pub(crate) fn decode_sid(value: &[u8]) -> Result<String> {
    decode(value, "sid:v1:")
}

/// Decode a stored occurrence handle, rejecting corrupt BLOBs.
pub(crate) fn decode_occ(value: &[u8]) -> Result<String> {
    decode(value, "occ:v2:")
}

/// Text that is not 64 lowercase hex digits names no stored digest.
pub(crate) fn encode_digest(value: &str) -> Option<Vec<u8>> {
    hex_bytes(value.as_bytes(), DIGEST_BYTES)
}

/// The stored form of a digest about to be written: malformed text fails the write.
pub(crate) fn digest(value: &str) -> Result<Vec<u8>> {
    encode_digest(value).context("invalid semantic digest")
}

/// The stored form of a digest keyset cursor. The empty cursor is the empty BLOB, which sorts
/// before every stored digest; any other malformed cursor fails rather than ending a walk.
pub(crate) fn digest_cursor(after: &str) -> Result<Vec<u8>> {
    if after.is_empty() {
        Ok(Vec::new())
    } else {
        encode_digest(after).context("invalid semantic digest cursor")
    }
}

/// Decode a stored digest, rejecting corrupt BLOBs.
pub(crate) fn decode_digest(value: &[u8]) -> Result<String> {
    if value.len() != DIGEST_BYTES {
        return Err(
            SelectedIntegrity("incompatible_index: invalid digest BLOB length".into()).into(),
        );
    }
    let mut result = String::with_capacity(2 * DIGEST_BYTES);
    push_hex(&mut result, value);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    #[test]
    fn round_trip_and_reject_noncanonical_text() {
        for (prefix, encode, decode) in [
            (
                "sid:v1:",
                encode_sid as fn(&str) -> Option<Vec<u8>>,
                decode_sid as fn(&[u8]) -> Result<String>,
            ),
            (
                "occ:v2:",
                encode_occ as fn(&str) -> Option<Vec<u8>>,
                decode_occ as fn(&[u8]) -> Result<String>,
            ),
        ] {
            let id = format!("{prefix}00112233445566778899aabbccddeeff");
            let bytes = encode(&id).unwrap();
            assert_eq!(bytes.len(), 16);
            assert_eq!(decode(&bytes).unwrap(), id);
            for malformed in [
                format!("{prefix}001122"),
                id.to_uppercase(),
                format!("{prefix}00112233445566778899aabbccddeefg"),
                format!("{id}0"),
            ] {
                assert!(encode(&malformed).is_none(), "{malformed}");
            }
            for length in [0, 1, 15, 17, 32] {
                let error = decode(&vec![0; length]).unwrap_err();
                assert!(error.to_string().starts_with("incompatible_index:"));
                assert_eq!(
                    super::super::recovery_class(&error),
                    super::super::RecoveryClass::Rebuild
                );
            }
        }
        assert!(encode_sid("occ:v2:00112233445566778899aabbccddeeff").is_none());
        assert!(encode_occ("sid:v1:00112233445566778899aabbccddeeff").is_none());
    }

    #[test]
    fn blob_order_matches_canonical_wire_text_order() {
        let blobs = [
            [0x00; 16],
            [0x0f; 16],
            [0x10; 16],
            [0x11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            [0x11; 16],
            [0xaa; 16],
            [0xff; 16],
        ];
        for decode in [
            decode_sid as fn(&[u8]) -> Result<String>,
            decode_occ as fn(&[u8]) -> Result<String>,
        ] {
            let mut wire: Vec<_> = blobs.iter().map(|v| decode(v).unwrap()).collect();
            let mut sorted = wire.clone();
            sorted.sort();
            assert_eq!(wire, sorted);
            wire.reverse();
            wire.sort();
            assert_eq!(wire, sorted);
        }
    }

    #[test]
    fn digest_round_trip_and_reject_noncanonical_text() {
        let id = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let bytes = encode_digest(id).unwrap();
        assert_eq!(bytes.len(), 32);
        assert_eq!(digest(id).unwrap(), bytes);
        assert_eq!(digest_cursor(id).unwrap(), bytes);
        assert_eq!(decode_digest(&bytes).unwrap(), id);
        for malformed in [
            "",
            &id[..62],
            &id.to_uppercase(),
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeefg",
            &format!("{id}0"),
            &format!("sid:v1:{}", &id[..32]),
        ] {
            assert!(encode_digest(malformed).is_none(), "{malformed}");
            let error = digest(malformed).unwrap_err();
            assert_eq!(error.to_string(), "invalid semantic digest");
            if !malformed.is_empty() {
                assert!(digest_cursor(malformed).is_err(), "{malformed}");
            }
        }
        assert_eq!(digest_cursor("").unwrap(), Vec::<u8>::new());
        for length in [0, 1, 16, 31, 33, 64] {
            let error = decode_digest(&vec![0; length]).unwrap_err();
            assert!(error.to_string().starts_with("incompatible_index:"));
            assert_eq!(
                super::super::recovery_class(&error),
                super::super::RecoveryClass::Rebuild
            );
        }
    }

    /// Keyset cursors and `ORDER BY` over a stored digest keep the canonical text's order,
    /// and the empty cursor precedes every digest.
    #[test]
    fn digest_blob_order_matches_canonical_wire_text_order() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE d(v BLOB NOT NULL CHECK(length(v)=32))")
            .unwrap();
        let mut texts: Vec<String> = [0x00u8, 0x0f, 0x10, 0x9f, 0xa0, 0xaa, 0xff]
            .iter()
            .flat_map(|&first| {
                [0x00u8, 0x01, 0xfe].map(|last| {
                    let mut bytes = [first; 32];
                    bytes[31] = last;
                    decode_digest(&bytes).unwrap()
                })
            })
            .collect();
        for text in texts.iter().rev() {
            db.execute("INSERT INTO d VALUES(?1)", [digest(text).unwrap()])
                .unwrap();
        }
        texts.sort();
        let ordered: Vec<String> = db
            .prepare("SELECT lower(hex(v)) FROM d ORDER BY v")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ordered, texts);
        let mut walked = Vec::new();
        let mut after = String::new();
        while let Some(next) = db
            .query_row(
                "SELECT lower(hex(v)) FROM d WHERE v>?1 ORDER BY v LIMIT 1",
                [digest_cursor(&after).unwrap()],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .unwrap()
        {
            walked.push(next.clone());
            after = next;
        }
        assert_eq!(walked, texts);
    }
}
