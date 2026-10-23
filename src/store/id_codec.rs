//! Lossless 16-byte database form of native syntax and occurrence handles.
//!
//! The wire representation remains the canonical lowercase 32-hex-digit ID.
use anyhow::{Result, bail};

const HANDLE_BYTES: usize = 16;

fn encode(value: &str, prefix: &str) -> Option<Vec<u8>> {
    let hex = value.strip_prefix(prefix)?.as_bytes();
    if hex.len() != HANDLE_BYTES * 2 {
        return None;
    }
    let mut bytes = Vec::with_capacity(HANDLE_BYTES);
    for pair in hex.as_chunks::<2>().0 {
        fn nibble(byte: u8) -> Option<u8> {
            match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            }
        }
        bytes.push(nibble(pair[0])? << 4 | nibble(pair[1])?);
    }
    Some(bytes)
}

fn decode(value: &[u8], prefix: &str) -> Result<String> {
    if value.len() != HANDLE_BYTES {
        bail!("incompatible_index: invalid native ID BLOB length");
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(prefix.len() + 2 * HANDLE_BYTES);
    result.push_str(prefix);
    for &byte in value {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
                assert!(
                    decode(&vec![0; length])
                        .unwrap_err()
                        .to_string()
                        .starts_with("incompatible_index:")
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
}
