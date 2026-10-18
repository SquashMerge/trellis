//! Bounded newline-delimited JSON-RPC framing and exact ID admission.
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

pub const REQUEST_BYTES: usize = 16_384;
pub const RESPONSE_BYTES: usize = 65_536;
/// The static, server-authored `tools/list` catalog has its own cap (#165);
/// every other response, including every tool result, stays within `RESPONSE_BYTES`.
pub const CATALOG_BYTES: usize = 262_144;
pub const SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// A frame is complete only after LF. Oversized frames are drained, not retained.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    Line(Vec<u8>),
    Oversized,
    Eof,
}
pub fn read_frame(reader: &mut impl BufRead) -> io::Result<Frame> {
    read_capped_frame(reader, REQUEST_BYTES)
}

/// The relay's own ID before each frame on the relayed daemon transport
/// (Decision 0009 §7): `<relay id> <frame>`. It routes replies; the frame
/// itself, with the client's ID, is forwarded byte for byte.
const RELAY_PREFIX_BYTES: usize = 21;

/// Read one relayed frame and its relay ID.
pub fn read_relayed_frame(reader: &mut impl BufRead) -> io::Result<(Option<u64>, Frame)> {
    match read_capped_frame(reader, REQUEST_BYTES + RELAY_PREFIX_BYTES)? {
        Frame::Line(bytes) => {
            let (relay, line) = split_relay_prefix(&bytes)?;
            Ok((
                Some(relay),
                if line.len() > REQUEST_BYTES {
                    Frame::Oversized
                } else {
                    Frame::Line(line.to_vec())
                },
            ))
        }
        frame => Ok((None, frame)),
    }
}

/// Split `<relay id> <rest>` into the relay ID and the rest.
pub fn split_relay_prefix(bytes: &[u8]) -> io::Result<(u64, &[u8])> {
    let space = bytes
        .iter()
        .position(|&b| b == b' ')
        .filter(|&at| at > 0 && at < RELAY_PREFIX_BYTES)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing relay prefix"))?;
    let relay = std::str::from_utf8(&bytes[..space])
        .ok()
        .filter(|digits| digits.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|digits| digits.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid relay prefix"))?;
    Ok((relay, &bytes[space + 1..]))
}

/// Write one response, prefixed with its relay ID on the relayed transport.
pub fn write_relayed_response(
    writer: &mut impl Write,
    relay: Option<u64>,
    response: &Value,
) -> io::Result<()> {
    write_relayed_capped(writer, relay, response, RESPONSE_BYTES)
}

/// As [`write_relayed_response`], under an explicit cap (the catalog's).
pub fn write_relayed_capped(
    writer: &mut impl Write,
    relay: Option<u64>,
    response: &Value,
    cap: usize,
) -> io::Result<()> {
    if let Some(relay) = relay {
        write!(writer, "{relay} ")?;
    }
    write_capped(writer, response, cap)
}

fn read_capped_frame(reader: &mut impl BufRead, cap: usize) -> io::Result<Frame> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(Frame::Eof);
        }
        let end = available.iter().position(|&b| b == b'\n');
        let n = end.unwrap_or(available.len());
        if !oversized {
            let keep = (cap.saturating_sub(bytes.len())).min(n);
            bytes.extend_from_slice(&available[..keep]);
            oversized = keep < n;
        }
        reader.consume(n + usize::from(end.is_some()));
        if end.is_some() {
            return Ok(if oversized {
                Frame::Oversized
            } else {
                Frame::Line(bytes)
            });
        }
    }
}

/// Decode a JSON number without ever converting through floating point.
/// Accept exact integral decimal/exponent spellings in the JSON safe-integer range.
pub fn exact_safe_integer(raw: &str) -> Option<i64> {
    let (negative, text) = if let Some(s) = raw.strip_prefix('-') {
        (true, s)
    } else {
        (false, raw)
    };
    let (mantissa, exponent) = text
        .split_once(['e', 'E'])
        .map_or((text, "0"), |(a, b)| (a, b));
    let (integer, fraction) = mantissa
        .split_once('.')
        .map_or((mantissa, ""), |(a, b)| (a, b));
    if integer.is_empty()
        || !integer.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{integer}{fraction}");
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return Some(0);
    }
    let scale = (exponent.parse::<i64>().ok()? as i128) - (fraction.len() as i128);
    let (significant, zeros) = if scale < 0 {
        let remove = usize::try_from(-scale).ok()?;
        if remove > significant.len() - significant.trim_end_matches('0').len() {
            return None;
        }
        (&significant[..significant.len() - remove], 0usize)
    } else {
        (significant, usize::try_from(scale).ok()?)
    };
    if significant.len().checked_add(zeros)? > 16 {
        return None;
    }
    let mut value: i64 = 0;
    for b in significant.bytes() {
        value = value.checked_mul(10)?.checked_add(i64::from(b - b'0'))?;
    }
    for _ in 0..zeros {
        value = value.checked_mul(10)?;
    }
    (value <= SAFE_INTEGER).then_some(if negative { -value } else { value })
}
/// JSON Schema integer for opaque protocol fields; unlike RPC IDs, no safe bound.
pub fn exact_json_integer(raw: &str) -> bool {
    let text = raw.strip_prefix('-').unwrap_or(raw);
    let (mantissa, exponent) = text
        .split_once(['e', 'E'])
        .map_or((text, "0"), |(a, b)| (a, b));
    let (whole, fraction) = mantissa
        .split_once('.')
        .map_or((mantissa, ""), |(a, b)| (a, b));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return false;
    }
    let digits = format!("{whole}{fraction}");
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return true;
    }
    let trailing = significant.len() - significant.trim_end_matches('0').len();
    let exp = exponent.parse::<i128>();
    let Ok(exp) = exp else {
        return !exponent.starts_with('-');
    };
    exp.saturating_add(trailing as i128) >= (fraction.len() as i128)
}
pub fn uint(value: &Value) -> Option<u64> {
    let number = value.as_number()?;
    exact_safe_integer(&number.to_string()).and_then(|n| u64::try_from(n).ok())
}

pub fn compact_string_token_len(s: &str) -> usize {
    2 + s
        .chars()
        .map(|c| match c {
            '\"' | '\\' | '\u{0008}' | '\t' | '\n' | '\u{000c}' | '\r' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => c.len_utf8(),
        })
        .sum::<usize>()
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum IdKey {
    String(String),
    Integer(i64),
}
#[derive(Clone, Debug)]
pub struct Request {
    pub id: Option<Value>,
    pub key: Option<IdKey>,
    pub method: String,
    pub params: Option<Value>,
}
#[derive(Clone, Debug)]
pub struct ProtocolError {
    pub code: i32,
    pub message: &'static str,
    pub id: Value,
    pub data: Option<Value>,
}
impl ProtocolError {
    pub fn new(code: i32, id: Value) -> Self {
        let message = match code {
            -32700 => "Parse error",
            -32600 => "Invalid Request",
            -32601 => "Method not found",
            -32602 => "Invalid params",
            -32603 => "Internal error",
            -32022 => "Unsupported protocol version",
            _ => "Internal error",
        };
        Self {
            code,
            message,
            id,
            data: None,
        }
    }
    pub fn response(&self) -> Value {
        let mut error = json!({"code":self.code,"message":self.message});
        if let Some(data) = &self.data {
            error["data"] = data.clone();
        }
        json!({"jsonrpc":"2.0","id":self.id,"error":error})
    }
}
pub fn decode(frame: Frame) -> Result<Option<Request>, ProtocolError> {
    let bytes = match frame {
        Frame::Eof => return Ok(None),
        Frame::Oversized => return Err(ProtocolError::new(-32600, Value::Null)),
        Frame::Line(bytes) => bytes,
    };
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| ProtocolError::new(-32700, Value::Null))?;
    let obj = value
        .as_object()
        .ok_or_else(|| ProtocolError::new(-32600, Value::Null))?;
    let (id, key) = match obj.get("id") {
        None => (None, None),
        Some(Value::String(s)) if compact_string_token_len(s) <= 256 => (
            Some(Value::String(s.clone())),
            Some(IdKey::String(s.clone())),
        ),
        Some(Value::Number(n)) => {
            let number = exact_safe_integer(&n.to_string())
                .ok_or_else(|| ProtocolError::new(-32600, Value::Null))?;
            (Some(json!(number)), Some(IdKey::Integer(number)))
        }
        _ => return Err(ProtocolError::new(-32600, Value::Null)),
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || !obj.get("method").is_some_and(Value::is_string)
        || obj
            .get("params")
            .is_some_and(|p| !p.is_object() && !p.is_array())
        || obj.contains_key("result")
        || obj.contains_key("error")
    {
        return Err(ProtocolError::new(-32600, id.unwrap_or(Value::Null)));
    }
    Ok(Some(Request {
        id,
        key,
        method: obj["method"].as_str().unwrap().to_owned(),
        params: obj.get("params").cloned(),
    }))
}
/// The only stdout writer; includes LF in the complete response budget.
pub fn write_response(writer: &mut impl Write, response: &Value) -> io::Result<()> {
    write_capped(writer, response, RESPONSE_BYTES)
}
fn write_capped(writer: &mut impl Write, response: &Value, cap: usize) -> io::Result<()> {
    let mut encoded = serde_json::to_vec(response).map_err(io::Error::other)?;
    if encoded.len() + 1 > cap {
        return Err(io::Error::other("response too large"));
    }
    encoded.push(b'\n');
    writer.write_all(&encoded)?;
    writer.flush()
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;
    #[test]
    fn only_an_explicit_catalog_cap_admits_frames_past_the_response_cap() {
        // A frame of exactly RESPONSE_BYTES + 1 bytes including LF.
        let over = json!("x".repeat(RESPONSE_BYTES - 2));
        let mut out = Vec::new();
        assert!(write_response(&mut out, &over).is_err());
        assert!(write_relayed_response(&mut out, None, &over).is_err());
        assert!(out.is_empty());
        write_relayed_capped(&mut out, Some(7), &over, CATALOG_BYTES).unwrap();
        assert_eq!(out.len(), "7 ".len() + RESPONSE_BYTES + 1);
        let past = json!("x".repeat(CATALOG_BYTES - 2));
        assert!(write_relayed_capped(&mut Vec::new(), None, &past, CATALOG_BYTES).is_err());
    }
    #[test]
    fn framing_and_recovery() {
        let input = format!(
            "{}\n{}\nx\n",
            "a".repeat(REQUEST_BYTES),
            "a".repeat(REQUEST_BYTES + 1)
        );
        let mut reader = BufReader::new(input.as_bytes());
        assert_eq!(
            read_frame(&mut reader).unwrap(),
            Frame::Line(vec![b'a'; REQUEST_BYTES])
        );
        assert_eq!(read_frame(&mut reader).unwrap(), Frame::Oversized);
        assert_eq!(read_frame(&mut reader).unwrap(), Frame::Line(b"x".to_vec()));
        assert_eq!(read_frame(&mut reader).unwrap(), Frame::Eof);
        let mut reader = BufReader::new(b"unfinished".as_slice());
        assert_eq!(read_frame(&mut reader).unwrap(), Frame::Eof);
    }
    #[test]
    fn exact_ids() {
        for s in [
            "1",
            "1.0",
            "1e0",
            "10e-1",
            "1.0000000000000000000000000000000",
        ] {
            assert_eq!(exact_safe_integer(s), Some(1));
        }
        for s in [
            "9007199254740992",
            "9007199254740991.1",
            "1e100",
            "1e-999",
            "-1.25",
        ] {
            assert_eq!(exact_safe_integer(s), None);
        }
        assert_eq!(exact_safe_integer("-0e999"), Some(0));
        assert_eq!(exact_safe_integer("9007199254740991"), Some(SAFE_INTEGER));
        assert_eq!(compact_string_token_len(&"\"".repeat(127)), 256);
        assert_eq!(compact_string_token_len(&"😀".repeat(63)), 254);
        for s in ["\n", "\t", "\r", "\u{0008}", "\u{000c}", "\"", "\\"] {
            assert_eq!(compact_string_token_len(s), 4);
        }
        assert_eq!(compact_string_token_len("\u{0000}"), 8);
    }
    #[test]
    fn parse_precedence() {
        assert_eq!(
            decode(Frame::Line(b"[1]".to_vec())).unwrap_err().code,
            -32600
        );
        assert_eq!(
            decode(Frame::Line(b"{not json}".to_vec()))
                .unwrap_err()
                .code,
            -32700
        );
        assert_eq!(decode(Frame::Line(vec![255])).unwrap_err().code, -32700);
        let raw = decode(Frame::Line(
            br#"{"jsonrpc":"2.0","id":1e0,"method":"x"}"#.to_vec(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(raw.key, Some(IdKey::Integer(1)));
    }
    #[test]
    fn canonical_string_id_boundaries_and_aliases() {
        for (unit, valid, invalid) in [
            ("a", 254, 255),
            ("\"", 127, 128),
            ("\\", 127, 128),
            ("\u{0000}", 42, 43),
            ("😀", 63, 64),
            ("é", 127, 128),
        ] {
            let a = unit.repeat(valid);
            let b = unit.repeat(invalid);
            assert!(compact_string_token_len(&a) <= 256);
            assert!(compact_string_token_len(&b) > 256);
            let encode = |s: &str| serde_json::to_string(s).unwrap();
            let frame = |s: &str| {
                Frame::Line(
                    format!(r#"{{"jsonrpc":"2.0","method":"probe","id":{}}}"#, encode(s))
                        .into_bytes(),
                )
            };
            assert_eq!(
                decode(frame(&a)).unwrap().unwrap().key,
                Some(IdKey::String(a))
            );
            let error = decode(frame(&b)).unwrap_err();
            assert_eq!(error.code, -32600);
            assert_eq!(error.id, Value::Null);
        }
        let variants = [
            r#"{"jsonrpc":"2.0","method":"probe","id":"é"}"#,
            r#"{"jsonrpc":"2.0","method":"probe","id":"\u00e9"}"#,
        ];
        assert_eq!(
            decode(Frame::Line(variants[0].as_bytes().to_vec()))
                .unwrap()
                .unwrap()
                .key,
            decode(Frame::Line(variants[1].as_bytes().to_vec()))
                .unwrap()
                .unwrap()
                .key
        );
        assert_eq!(compact_string_token_len("e\u{0301}"), 5);
        assert_eq!(compact_string_token_len("é"), 4);
        assert_eq!(compact_string_token_len("/\u{2028}"), 6);
        for invalid in ["null", "true", "[]", "{}", "1.2", "9007199254740992"] {
            let bytes = format!(r#"{{"jsonrpc":"2.0","method":"probe","id":{invalid}}}"#);
            let error = decode(Frame::Line(bytes.into_bytes())).unwrap_err();
            assert_eq!(error.code, -32600);
            assert_eq!(error.id, Value::Null);
        }
    }
    /// Spelling independent of serde_json's encoder, including surrogate pairs.
    fn escaped_token(s: &str) -> String {
        let mut out = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\u0022"),
                '\\' => out.push_str("\\u005c"),
                c if (c as u32) < 0x80 && !c.is_control() => out.push(c),
                c if (c as u32) <= 0xffff => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => {
                    let scalar = (c as u32) - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (scalar >> 10),
                        0xdc00 + (scalar & 0x3ff)
                    ));
                }
            }
        }
        out.push('"');
        out
    }
    #[test]
    fn id_boundaries() {
        let mut units: Vec<(char, usize)> = vec![
            ('a', 1),
            ('"', 2),
            ('\\', 2),
            ('é', 2),
            ('€', 3),
            ('😀', 4),
            ('/', 1),
            ('\u{2028}', 3),
        ];
        for c in ['\u{0008}', '\t', '\n', '\u{000c}', '\r'] {
            units.push((c, 2));
        }
        for n in 0..=31 {
            let c = char::from_u32(n).unwrap();
            if !['\u{0008}', '\t', '\n', '\u{000c}', '\r'].contains(&c) {
                units.push((c, 6));
            }
        }
        for (unit, cost) in units {
            // The independent cost arithmetic never calls the implementation under test.
            let repeats = 254 / cost;
            let tail = 254 - (repeats * cost);
            let admitted = format!("{}{}", unit.to_string().repeat(repeats), "a".repeat(tail));
            let rejected = format!("{admitted}a");
            let expected_valid = 2 + repeats * cost + tail;
            let expected_invalid = expected_valid + 1;
            assert_eq!(expected_valid, 256, "{unit:?}");
            assert_eq!(expected_invalid, 257, "{unit:?}");
            for (value, expected) in [(&admitted, 256), (&rejected, 257)] {
                assert_eq!(compact_string_token_len(value), expected, "{unit:?}");
                for token in [serde_json::to_string(value).unwrap(), escaped_token(value)] {
                    let bytes = format!(r#"{{"jsonrpc":"2.0","id":{token},"method":"probe"}}"#)
                        .into_bytes();
                    assert!(bytes.len() < REQUEST_BYTES);
                    if expected == 256 {
                        let admitted = decode(Frame::Line(bytes)).unwrap().unwrap();
                        assert_eq!(admitted.id, Some(json!(value)), "{unit:?}");
                        assert_eq!(
                            admitted.key,
                            Some(IdKey::String(value.to_owned())),
                            "{unit:?}"
                        );
                    } else {
                        let err = decode(Frame::Line(bytes)).unwrap_err();
                        assert_eq!((err.code, err.id), (-32600, Value::Null), "{unit:?}");
                    }
                }
            }
        }
        // Mixed spelling: an escaped scalar and literal scalar normalize identically.
        for (raw, escaped) in [
            (r#""é😀""#, r#""\u00e9\ud83d\ude00""#),
            (r#""\"\\""#, r#""\u0022\u005c""#),
            (r#""\n\u0000""#, r#""\u000a\u0000""#),
        ] {
            let frame = |token: &str| {
                Frame::Line(
                    format!(r#"{{"jsonrpc":"2.0","id":{token},"method":"probe"}}"#).into_bytes(),
                )
            };
            assert_eq!(
                decode(frame(raw)).unwrap().unwrap().key,
                decode(frame(escaped)).unwrap().unwrap().key
            );
        }
    }
}
