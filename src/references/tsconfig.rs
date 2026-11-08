//! The root `tsconfig.json` (else `jsconfig.json`) options that steer import resolution.
//! JSONC is read with `serde_json` after comments and trailing commas are removed. A
//! config this cannot read is a typed reason, never a crash. `extends` is not followed.
use serde_json::Value;
use std::collections::BTreeMap;

/// The captured bytes of the selected root config of one source set.
#[derive(Clone, Debug)]
pub struct RootTsconfigInput {
    pub source_set_id: String,
    pub path: String,
    pub bytes: Vec<u8>,
}

/// The reason reported for bare specifiers when the root config cannot be read.
pub(super) const UNREADABLE: &str = "tsconfig_unreadable";

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct RootTsconfig {
    pub(super) base_url: Option<String>,
    pub(super) paths: BTreeMap<String, Vec<String>>,
    pub(super) root_dirs: Vec<String>,
}

/// Parse JSONC bytes. `serde_json` bounds nesting at 128 levels.
pub(super) fn parse(bytes: &[u8]) -> Result<RootTsconfig, &'static str> {
    let root = parse_jsonc(bytes).ok_or(UNREADABLE)?;
    let root = root.as_object().ok_or(UNREADABLE)?;
    let mut config = RootTsconfig::default();
    let Some(options) = root.get("compilerOptions").filter(|v| !v.is_null()) else {
        return Ok(config);
    };
    let options = options.as_object().ok_or(UNREADABLE)?;
    let strings = |value: &Value| -> Result<Vec<String>, &'static str> {
        value
            .as_array()
            .ok_or(UNREADABLE)?
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or(UNREADABLE))
            .collect()
    };
    if let Some(base_url) = options.get("baseUrl").filter(|v| !v.is_null()) {
        config.base_url = Some(base_url.as_str().ok_or(UNREADABLE)?.to_owned());
    }
    if let Some(paths) = options.get("paths").filter(|v| !v.is_null()) {
        for (key, targets) in paths.as_object().ok_or(UNREADABLE)? {
            config.paths.insert(key.clone(), strings(targets)?);
        }
    }
    if let Some(root_dirs) = options.get("rootDirs").filter(|v| !v.is_null()) {
        config.root_dirs = strings(root_dirs)?;
    }
    Ok(config)
}

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

/// A root-relative path with `.` and `..` resolved, or `None` when it is absolute or
/// leaves the source root. The root itself is the empty string.
pub(super) fn normalize(path: &str) -> Option<String> {
    if path.starts_with('/') {
        return None;
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_compiler_options_through_comments_and_trailing_commas() {
        let config = parse(
            br#"{
              // line comment with "quotes" and /* a block opener
              "compilerOptions": {
                /* block */ "baseUrl": "./src", // trailing
                "paths": { "@app/*": ["app/*", "legacy/*",], "url": ["http://x//y"] },
                "rootDirs": ["src", "generated",],
              },
              "extends": "./base.json",
            }"#,
        )
        .unwrap();
        assert_eq!(config.base_url.as_deref(), Some("./src"));
        assert_eq!(config.paths["@app/*"], ["app/*", "legacy/*"]);
        assert_eq!(config.paths["url"], ["http://x//y"]);
        assert_eq!(config.root_dirs, ["src", "generated"]);
    }

    #[test]
    fn strips_a_byte_order_mark_and_accepts_missing_options() {
        let config = parse(b"\xEF\xBB\xBF{\"compilerOptions\":{}}").unwrap();
        assert_eq!(config, RootTsconfig::default());
        assert_eq!(parse(b"{}").unwrap(), RootTsconfig::default());
    }

    #[test]
    fn commas_inside_strings_are_kept() {
        let config = parse(br#"{"compilerOptions":{"paths":{"a,}":["b,]"]}}}"#).unwrap();
        assert_eq!(config.paths["a,}"], ["b,]"]);
    }

    #[test]
    fn wrong_types_and_malformed_json_are_unreadable() {
        for bad in [
            &b"{\"compilerOptions\":{\"baseUrl\":1}}"[..],
            b"{\"compilerOptions\":{\"paths\":{\"a\":\"b\"}}}",
            b"{\"compilerOptions\":{\"rootDirs\":\"src\"}}",
            b"{\"compilerOptions\":[]}",
            b"[]",
            b"",
            b"{ invalid",
            b"{\"a\": /* never closed }",
        ] {
            assert_eq!(
                parse(bad),
                Err(UNREADABLE),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn deep_nesting_is_unreadable_not_a_crash() {
        let depth = 200;
        let deep = format!(
            "{{\"compilerOptions\":{{}},\"x\":{}{}}}",
            "[".repeat(depth),
            "]".repeat(depth)
        );
        assert_eq!(parse(deep.as_bytes()), Err(UNREADABLE));
        let shallow = format!("{{\"x\":{}{}}}", "[".repeat(100), "]".repeat(100));
        assert!(parse(shallow.as_bytes()).is_ok());
    }

    #[test]
    fn normalize_resolves_dots_and_rejects_escapes() {
        assert_eq!(normalize("./a/../b/").as_deref(), Some("b"));
        assert_eq!(normalize(".").as_deref(), Some(""));
        assert_eq!(normalize("../a"), None);
        assert_eq!(normalize("/abs"), None);
    }
}
