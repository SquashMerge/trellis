//! A bounded JSONC reader: JSON with comments, trailing commas and a byte order mark.
use serde_json::Value;

/// The JSONC value of `bytes` (a leading byte order mark, comments and trailing commas
/// allowed), or `None` when it is not JSONC. `serde_json` bounds nesting at 128 levels.
pub(crate) fn parse_jsonc(bytes: &[u8]) -> Option<Value> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    serde_json::from_slice(&strip(bytes)?).ok()
}

/// Remove comments outside strings, then commas directly before `}` or `]`. An
/// unterminated block comment is unreadable.
fn strip(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut plain = Vec::with_capacity(bytes.len());
    let mut at = 0;
    let mut string = false;
    while at < bytes.len() {
        let byte = bytes[at];
        if string {
            plain.push(byte);
            if byte == b'\\' {
                if let Some(&escaped) = bytes.get(at + 1) {
                    plain.push(escaped);
                }
                at += 1;
            } else if byte == b'"' {
                string = false;
            }
            at += 1;
        } else if byte == b'"' {
            string = true;
            plain.push(byte);
            at += 1;
        } else if byte == b'/' && bytes.get(at + 1) == Some(&b'/') {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
        } else if byte == b'/' && bytes.get(at + 1) == Some(&b'*') {
            let end = bytes[at + 2..].windows(2).position(|w| w == b"*/")?;
            plain.push(b' ');
            at += end + 4;
        } else {
            plain.push(byte);
            at += 1;
        }
    }
    let mut result = Vec::with_capacity(plain.len());
    string = false;
    let mut at = 0;
    while at < plain.len() {
        let byte = plain[at];
        if string {
            result.push(byte);
            if byte == b'\\' {
                if let Some(&escaped) = plain.get(at + 1) {
                    result.push(escaped);
                }
                at += 1;
            } else if byte == b'"' {
                string = false;
            }
        } else if byte == b'"' {
            string = true;
            result.push(byte);
        } else if byte != b','
            || !plain[at + 1..]
                .iter()
                .find(|b| !b.is_ascii_whitespace())
                .is_some_and(|b| matches!(b, b'}' | b']'))
        {
            result.push(byte);
        }
        at += 1;
    }
    Some(result)
}
