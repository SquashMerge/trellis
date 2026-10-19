//! Canonical native syntax and occurrence identity. Digest inputs never include revision or
//! source body; occurrence identity binds the document's content hash instead (Decision 0003).
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub fn canonical(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    canonical_into(value, &mut |bytes| out.extend_from_slice(bytes));
    out
}

/// The canonical encoding of `value`, written to `out` piece by piece (for
/// example straight into a hasher) instead of collected.
pub fn canonical_into(value: &Value, out: &mut dyn FnMut(&[u8])) {
    match value {
        Value::Null => out(b"null"),
        Value::Bool(b) => out(if *b { b"true" } else { b"false" }),
        Value::Number(n) => out(n.to_string().as_bytes()),
        Value::String(s) => canonical_string_into(s, out),
        Value::Array(items) => {
            out(b"[");
            for (i, v) in items.iter().enumerate() {
                if i != 0 {
                    out(b",");
                }
                canonical_into(v, out);
            }
            out(b"]");
        }
        Value::Object(map) => {
            out(b"{");
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            for (i, key) in keys.iter().enumerate() {
                if i != 0 {
                    out(b",");
                }
                canonical_string_into(key, out);
                out(b":");
                canonical_into(&map[*key], out);
            }
            out(b"}");
        }
    }
}

/// The canonical encoding of one string.
pub fn canonical_string_into(s: &str, out: &mut dyn FnMut(&[u8])) {
    out(b"\"");
    for c in s.chars() {
        match c {
            '"' => out(b"\\\""),
            '\\' => out(b"\\\\"),
            '\u{0}'..='\u{1f}' => out(format!("\\u{:04x}", c as u32).as_bytes()),
            _ => {
                let mut bytes = [0; 4];
                out(c.encode_utf8(&mut bytes).as_bytes());
            }
        }
    }
    out(b"\"");
}

pub fn digest(domain: &[u8], bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(bytes);
    hex::encode(hash.finalize())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StableInput {
    #[serde(rename = "sourceSet")]
    source_set: String,
    path: String,
    language: String,
    ancestors: Vec<StableKey>,
    declaration: StableKey,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StableKey {
    kind: String,
    name: Option<String>,
    signature: Option<StableSignature>,
    ordinal: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StableSignature {
    #[serde(rename = "parameterTypes")]
    parameter_types: Vec<String>,
    #[serde(rename = "typeParameterCount")]
    type_parameter_count: u64,
    variadic: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OccurrenceInput {
    #[serde(rename = "contentHash")]
    content_hash: String,
    #[serde(rename = "extractionContext")]
    extraction_context: String,
    #[serde(rename = "nativeProducerId")]
    native_producer_id: String,
    #[serde(rename = "nativeProducerVersion")]
    native_producer_version: String,
    #[serde(rename = "ownerSyntaxId")]
    owner_syntax_id: String,
    kind: String,
    ordinal: u64,
}
fn exact_keys(value: &Value, expected: &[&str]) -> Result<()> {
    let map = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("native ID input is not an object"))?;
    ensure!(
        map.len() == expected.len() && expected.iter().all(|k| map.contains_key(*k)),
        "missing or unknown native ID input field"
    );
    Ok(())
}
fn check_text(s: &str) -> Result<()> {
    ensure!(!s.is_empty(), "empty native identity text");
    Ok(())
}
fn check_key(key: &StableKey, language: &str) -> Result<()> {
    ensure!(
        matches!(
            key.kind.as_str(),
            "module"
                | "namespace"
                | "type"
                | "implementation"
                | "function"
                | "method"
                | "constructor"
                | "field"
                | "variable"
                | "parameter"
                | "typeParameter"
                | "alias"
                | "anonymousFunction"
        ),
        "unknown native kind"
    );
    if let Some(name) = &key.name {
        check_text(name)?;
    }
    ensure!(
        key.ordinal <= 9_007_199_254_740_991,
        "native ordinal exceeds UInt"
    );
    if let Some(signature) = &key.signature {
        ensure!(
            language == "java"
                && matches!(key.kind.as_str(), "method" | "constructor")
                && signature.type_parameter_count <= 9_007_199_254_740_991,
            "invalid native Java signature"
        );
        for param in &signature.parameter_types {
            check_text(param)?;
        }
        let _ = signature.variadic;
    } else {
        ensure!(
            language != "java" || !matches!(key.kind.as_str(), "method" | "constructor"),
            "missing native Java signature"
        );
    }
    Ok(())
}
fn validate_stable(value: &Value) -> Result<()> {
    exact_keys(
        value,
        &["sourceSet", "path", "language", "ancestors", "declaration"],
    )?;
    for key in value["ancestors"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("ancestors not an array"))?
        .iter()
        .chain(std::iter::once(&value["declaration"]))
    {
        exact_keys(key, &["kind", "name", "signature", "ordinal"])?;
        if !key["signature"].is_null() {
            exact_keys(
                &key["signature"],
                &["parameterTypes", "typeParameterCount", "variadic"],
            )?;
        }
    }
    let input: StableInput = serde_json::from_value(value.clone())?;
    check_text(&input.source_set)?;
    ensure!(
        !input.path.starts_with('/')
            && !input.path.contains(['\\', '\0'])
            && input
                .path
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".."),
        "invalid native identity path"
    );
    ensure!(
        matches!(
            input.language.as_str(),
            "java" | "rust" | "python" | "javascript"
        ),
        "invalid native identity language"
    );
    for key in input
        .ancestors
        .iter()
        .chain(std::iter::once(&input.declaration))
    {
        check_key(key, &input.language)?;
    }
    Ok(())
}
fn check_hash(s: &str) -> Result<()> {
    ensure!(
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid native identity hash"
    );
    Ok(())
}
fn validate_occurrence(value: &Value) -> Result<()> {
    exact_keys(
        value,
        &[
            "contentHash",
            "extractionContext",
            "nativeProducerId",
            "nativeProducerVersion",
            "ownerSyntaxId",
            "kind",
            "ordinal",
        ],
    )?;
    let input: OccurrenceInput = serde_json::from_value(value.clone())?;
    check_hash(&input.content_hash)?;
    check_hash(&input.extraction_context)?;
    check_text(&input.native_producer_id)?;
    check_text(&input.native_producer_version)?;
    ensure!(
        input.owner_syntax_id.len() == 39
            && input.owner_syntax_id.starts_with("sid:v1:")
            && input.owner_syntax_id[7..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid occurrence owner SyntaxId"
    );
    ensure!(
        matches!(input.kind.as_str(), "call" | "control" | "reference")
            && input.ordinal <= 9_007_199_254_740_991,
        "invalid native occurrence input"
    );
    Ok(())
}

#[derive(Default)]
pub struct IdentityRegistry {
    handles: BTreeMap<String, (Vec<u8>, String)>,
    /// First-insertion order, kept only by a per-document registry that is
    /// later absorbed into a full one (`IdentityRegistry::logged`).
    order: Option<Vec<String>>,
}

/// The pure part of one identity: its validated canonical bytes, full
/// digest and handle. `IdentityRegistry::admit` applies the collision rule.
pub(crate) struct Minted {
    handle: String,
    bytes: Vec<u8>,
    full: String,
}

const STABLE_DOMAIN: &[u8] = b"trellis.syntax.v1\0";
const OCCURRENCE_DOMAIN: &[u8] = b"trellis.occurrence.v2\0";

fn mint(domain: &[u8], value: &Value, prefix: &str) -> Minted {
    let bytes = canonical(value);
    let full = digest(domain, &bytes);
    let handle = format!("{}{}", prefix, &full[..32]);
    Minted {
        handle,
        bytes,
        full,
    }
}

/// Validate and mint a SyntaxId without registering it.
pub(crate) fn mint_stable(input: &Value) -> Result<Minted> {
    validate_stable(input)?;
    Ok(mint(STABLE_DOMAIN, input, "sid:v1:"))
}

/// Validate and mint an occurrence ID without registering it.
pub(crate) fn mint_occurrence(input: &Value) -> Result<Minted> {
    validate_occurrence(input)?;
    Ok(mint(OCCURRENCE_DOMAIN, input, "occ:v2:"))
}

impl IdentityRegistry {
    /// A registry that records first-insertion order, for `absorb`.
    pub(crate) fn logged() -> Self {
        Self {
            handles: BTreeMap::new(),
            order: Some(Vec::new()),
        }
    }
    pub fn register(&mut self, domain: &[u8], value: &Value, prefix: &str) -> Result<String> {
        self.admit(mint(domain, value, prefix))
    }
    /// Register a minted identity; a handle seen before must carry the
    /// same canonical bytes and digest.
    pub(crate) fn admit(&mut self, minted: Minted) -> Result<String> {
        let Minted {
            handle,
            bytes,
            full,
        } = minted;
        if let Some((prior, prior_full)) = self.handles.get(&handle) {
            ensure!(
                prior == &bytes && prior_full == &full,
                "native identity handle collision: {handle}"
            );
        } else {
            if let Some(order) = &mut self.order {
                order.push(handle.clone());
            }
            self.handles.insert(handle.clone(), (bytes, full));
        }
        Ok(handle)
    }
    /// Admit every identity of a logged registry in its first-insertion
    /// order, as if each had been registered here directly in that order.
    pub(crate) fn absorb(&mut self, mut other: IdentityRegistry) -> Result<()> {
        for handle in other.order.take().unwrap_or_default() {
            let (bytes, full) = other
                .handles
                .remove(&handle)
                .expect("logged identity is registered");
            self.admit(Minted {
                handle,
                bytes,
                full,
            })?;
        }
        Ok(())
    }
    pub fn stable(&mut self, input: &Value) -> Result<String> {
        self.admit(mint_stable(input)?)
    }
    pub fn occurrence(&mut self, input: &Value) -> Result<String> {
        self.admit(mint_occurrence(input)?)
    }
}

/// Decision 0003 extraction context: the full SHA-256 over
/// `trellis.extraction-context.v1\0` and canonical `{language,components}`.
/// Components must already be sorted by name then hash and unique.
pub fn extraction_context(language: &str, components: &[(String, String)]) -> Result<String> {
    ensure!(
        matches!(language, "java" | "rust" | "python" | "javascript"),
        "invalid extraction-context language"
    );
    for (name, hash) in components {
        check_text(name)?;
        check_hash(hash)?;
    }
    ensure!(
        components.windows(2).all(|w| {
            (w[0].0.as_bytes(), w[0].1.as_bytes()) < (w[1].0.as_bytes(), w[1].1.as_bytes())
        }),
        "unsorted or duplicate extraction-context components"
    );
    let components: Vec<Value> = components
        .iter()
        .map(|(name, hash)| serde_json::json!({"name":name,"hash":hash}))
        .collect();
    Ok(digest(
        b"trellis.extraction-context.v1\0",
        &canonical(&serde_json::json!({"language":language,"components":components})),
    ))
}
