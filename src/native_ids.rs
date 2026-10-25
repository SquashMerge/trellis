//! Canonical native syntax and occurrence identity. Digest inputs never include revision or
//! source body; occurrence identity binds the document's content hash instead (Decision 0003).
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

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

/// Typed occurrence fields, borrowed from the authenticated document and its row.
pub(crate) struct OccurrenceInputRef<'a> {
    pub(crate) content_hash: &'a str,
    pub(crate) extraction_context: &'a str,
    pub(crate) native_producer_id: &'a str,
    pub(crate) native_producer_version: &'a str,
    pub(crate) owner_syntax_id: &'a str,
    pub(crate) kind: &'a str,
    pub(crate) ordinal: u64,
}

impl OccurrenceInput {
    fn as_ref(&self) -> OccurrenceInputRef<'_> {
        OccurrenceInputRef {
            content_hash: &self.content_hash,
            extraction_context: &self.extraction_context,
            native_producer_id: &self.native_producer_id,
            native_producer_version: &self.native_producer_version,
            owner_syntax_id: &self.owner_syntax_id,
            kind: &self.kind,
            ordinal: self.ordinal,
        }
    }
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
    validate_occurrence_fields(&input.as_ref())
}

fn validate_occurrence_fields(input: &OccurrenceInputRef<'_>) -> Result<()> {
    check_hash(input.content_hash)?;
    check_hash(input.extraction_context)?;
    check_text(input.native_producer_id)?;
    check_text(input.native_producer_version)?;
    ensure!(
        input.owner_syntax_id.len() == 39
            && input.owner_syntax_id.starts_with("sid:v1:")
            && input.owner_syntax_id[7..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid occurrence owner SyntaxId"
    );
    ensure!(
        matches!(input.kind, "call" | "control" | "reference")
            && input.ordinal <= 9_007_199_254_740_991,
        "invalid native occurrence input"
    );
    Ok(())
}

/// Namespace is part of the key even if two truncated digests have the same bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Namespace {
    Syntax,
    Occurrence,
}

impl Namespace {
    fn prefix(self) -> &'static str {
        match self {
            Self::Syntax => "sid:v1:",
            Self::Occurrence => "occ:v2:",
        }
    }
}

type Key = (Namespace, [u8; 16]);

#[derive(Default)]
#[cfg_attr(test, derive(Clone))]
pub struct IdentityRegistry {
    handles: HashMap<Key, [u8; 32]>,
    /// First-insertion order only in per-document registries, later absorbed
    /// into the revision-wide registry in sorted file order.
    order: Option<Vec<Key>>,
    #[cfg(test)]
    fail_reservation: bool,
}

/// Hash-based identity over validated canonical input and the original domain.
/// A full digest match is the same identity; the owner accepts the residual
/// full-SHA-256 collision risk. No canonical bytes are retained.
pub(crate) struct Minted {
    key: Key,
    full: [u8; 32],
}

const STABLE_DOMAIN: &[u8] = b"trellis.syntax.v1\0";
const OCCURRENCE_DOMAIN: &[u8] = b"trellis.occurrence.v2\0";

fn mint(domain: &[u8], value: &Value, namespace: Namespace) -> Minted {
    let mut hash = Sha256::new();
    hash.update(domain);
    canonical_into(value, &mut |bytes| hash.update(bytes));
    finish_mint(hash, namespace)
}

fn finish_mint(hash: Sha256, namespace: Namespace) -> Minted {
    let full: [u8; 32] = hash.finalize().into();
    let mut handle = [0; 16];
    handle.copy_from_slice(&full[..16]);
    Minted {
        key: (namespace, handle),
        full,
    }
}

/// Validate and mint a SyntaxId without registering it.
pub(crate) fn mint_stable(input: &Value) -> Result<Minted> {
    validate_stable(input)?;
    Ok(mint(STABLE_DOMAIN, input, Namespace::Syntax))
}

/// Validate and mint an occurrence ID without registering it.
pub(crate) fn mint_occurrence(input: &Value) -> Result<Minted> {
    validate_occurrence(input)?;
    Ok(mint(OCCURRENCE_DOMAIN, input, Namespace::Occurrence))
}

fn occurrence_canonical_into(input: &OccurrenceInputRef<'_>, out: &mut dyn FnMut(&[u8])) {
    out(b"{\"contentHash\":");
    canonical_string_into(input.content_hash, out);
    out(b",\"extractionContext\":");
    canonical_string_into(input.extraction_context, out);
    out(b",\"kind\":");
    canonical_string_into(input.kind, out);
    out(b",\"nativeProducerId\":");
    canonical_string_into(input.native_producer_id, out);
    out(b",\"nativeProducerVersion\":");
    canonical_string_into(input.native_producer_version, out);
    out(b",\"ordinal\":");
    out(input.ordinal.to_string().as_bytes());
    out(b",\"ownerSyntaxId\":");
    canonical_string_into(input.owner_syntax_id, out);
    out(b"}");
}

/// Validate and mint an occurrence without building an owned JSON object.
pub(crate) fn mint_occurrence_ref(input: &OccurrenceInputRef<'_>) -> Result<Minted> {
    validate_occurrence_fields(input)?;
    let mut hash = Sha256::new();
    hash.update(OCCURRENCE_DOMAIN);
    occurrence_canonical_into(input, &mut |bytes| hash.update(bytes));
    Ok(finish_mint(hash, Namespace::Occurrence))
}

impl IdentityRegistry {
    /// A registry that records first-insertion order, for `absorb`.
    pub(crate) fn logged() -> Self {
        Self {
            handles: HashMap::new(),
            order: Some(Vec::new()),
            #[cfg(test)]
            fail_reservation: false,
        }
    }
    /// Admit a minted identity. A typed 128-bit handle with a different
    /// full digest is a detected collision, even across different files.
    pub(crate) fn admit(&mut self, minted: Minted) -> Result<String> {
        let key = minted.key;
        self.register(minted)?;
        Ok(format!("{}{}", key.0.prefix(), hex::encode(key.1)))
    }
    fn register(&mut self, minted: Minted) -> Result<()> {
        let Minted { key, full } = minted;
        if let Some(prior) = self.handles.get(&key) {
            ensure!(
                prior == &full,
                "native identity handle collision: {}{}",
                key.0.prefix(),
                hex::encode(key.1)
            );
        } else {
            #[cfg(test)]
            if std::mem::take(&mut self.fail_reservation) {
                self.handles.try_reserve(usize::MAX)?;
            }
            self.handles.try_reserve(1)?;
            if let Some(order) = &mut self.order {
                order.try_reserve(1)?;
                order.push(key);
            }
            self.handles.insert(key, full);
        }
        Ok(())
    }
    /// Admit the successful document's distinct identities in their original
    /// first-insertion order. Failed documents are never admitted.
    pub(crate) fn absorb(&mut self, mut other: IdentityRegistry) -> Result<()> {
        for key in other.order.take().unwrap_or_default() {
            let full = other
                .handles
                .remove(&key)
                .expect("logged identity is registered");
            self.register(Minted { key, full })?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn fail_next_reservation_for_tests(&mut self) {
        self.fail_reservation = true;
    }
    #[cfg(test)]
    pub(crate) fn same_first_identity_for_tests(&self, other: &IdentityRegistry) -> bool {
        let first = self.order.as_ref().and_then(|order| order.first());
        first.is_some()
            && first == other.order.as_ref().and_then(|order| order.first())
            && self.handles.get(first.unwrap()) == other.handles.get(first.unwrap())
    }
    #[cfg(test)]
    pub(crate) fn inject_collision_for_tests(&mut self, other: &IdentityRegistry) {
        let key = *other.order.as_ref().unwrap().first().unwrap();
        let mut full = *other.handles.get(&key).unwrap();
        full[16] ^= 1;
        self.handles.insert(key, full);
    }
    pub fn stable(&mut self, input: &Value) -> Result<String> {
        self.admit(mint_stable(input)?)
    }
    pub fn occurrence(&mut self, input: &Value) -> Result<String> {
        self.admit(mint_occurrence(input)?)
    }
    pub(crate) fn occurrence_ref(&mut self, input: &OccurrenceInputRef<'_>) -> Result<String> {
        self.admit(mint_occurrence_ref(input)?)
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

#[cfg(test)]
mod registry_tests {
    use super::*;

    fn minted(namespace: Namespace, handle: [u8; 16], suffix: u8) -> Minted {
        let mut full = [suffix; 32];
        full[..16].copy_from_slice(&handle);
        Minted {
            key: (namespace, handle),
            full,
        }
    }

    fn occurrence_value(kind: &str, ordinal: u64) -> Value {
        serde_json::json!({
            "contentHash": "a".repeat(64),
            "extractionContext": "b".repeat(64),
            "nativeProducerId": "native\"\\\n\u{0000}é中",
            "nativeProducerVersion": "native-v9\t\r",
            "ownerSyntaxId": format!("sid:v1:{}", "c".repeat(32)),
            "kind": kind,
            "ordinal": ordinal,
        })
    }

    #[test]
    fn borrowed_occurrence_matches_generic_canonical_bytes_digest_and_id() {
        for kind in ["call", "control", "reference"] {
            for ordinal in [0, 1, 9_007_199_254_740_991] {
                let value = occurrence_value(kind, ordinal);
                let owned: OccurrenceInput = serde_json::from_value(value.clone()).unwrap();
                let input = owned.as_ref();
                let mut bytes = Vec::new();
                occurrence_canonical_into(&input, &mut |part| bytes.extend_from_slice(part));
                assert_eq!(bytes, canonical(&value));
                let borrowed = mint_occurrence_ref(&input).unwrap();
                let generic = mint_occurrence(&value).unwrap();
                assert_eq!(borrowed.key, generic.key);
                assert_eq!(borrowed.full, generic.full);
                let mut registry = IdentityRegistry::logged();
                let id = registry.occurrence_ref(&input).unwrap();
                assert_eq!(registry.occurrence(&value).unwrap(), id);
                assert_eq!(registry.order.as_ref().unwrap().len(), 1);
            }
        }
    }

    #[test]
    fn borrowed_occurrence_preserves_field_validation() {
        for (field, invalid) in [
            ("contentHash", serde_json::json!("a".repeat(63))),
            ("contentHash", serde_json::json!("A".repeat(64))),
            ("extractionContext", serde_json::json!("not-a-hash")),
            ("nativeProducerId", serde_json::json!("")),
            ("nativeProducerVersion", serde_json::json!("")),
            ("ownerSyntaxId", serde_json::json!("sid:v1:missing")),
            (
                "ownerSyntaxId",
                serde_json::json!(format!("occ:v2:{}", "c".repeat(32))),
            ),
            ("kind", serde_json::json!("unknown")),
            ("ordinal", serde_json::json!(9_007_199_254_740_992u64)),
            ("ordinal", serde_json::json!(u64::MAX)),
        ] {
            let mut value = occurrence_value("reference", 0);
            value[field] = invalid;
            let owned: OccurrenceInput = serde_json::from_value(value.clone()).unwrap();
            let generic_error = mint_occurrence(&value).err().unwrap().to_string();
            let borrowed_error = mint_occurrence_ref(&owned.as_ref())
                .err()
                .unwrap()
                .to_string();
            assert_eq!(borrowed_error, generic_error, "{field}");
        }
    }

    #[test]
    fn generic_occurrence_still_rejects_missing_unknown_and_wrong_type_fields() {
        let valid = occurrence_value("reference", 0);
        for field in [
            "contentHash",
            "extractionContext",
            "nativeProducerId",
            "nativeProducerVersion",
            "ownerSyntaxId",
            "kind",
            "ordinal",
        ] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(mint_occurrence(&missing).is_err(), "missing {field}");
            let mut null = valid.clone();
            null[field] = Value::Null;
            assert!(mint_occurrence(&null).is_err(), "null {field}");
        }
        let mut unknown = valid.clone();
        unknown["unknown"] = Value::Bool(true);
        assert!(mint_occurrence(&unknown).is_err());
        for invalid in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("1"),
        ] {
            let mut value = valid.clone();
            value["ordinal"] = invalid;
            assert!(mint_occurrence(&value).is_err());
        }
        let mut wrong_text_type = valid;
        wrong_text_type["contentHash"] = Value::Bool(true);
        assert!(mint_occurrence(&wrong_text_type).is_err());
        assert!(mint_occurrence(&Value::Null).is_err());
    }

    #[test]
    fn absorb_preserves_fallible_reservation() {
        let mut local = IdentityRegistry::logged();
        local
            .admit(minted(Namespace::Occurrence, [0x35; 16], 1))
            .unwrap();
        let mut global = IdentityRegistry::default();
        global.fail_next_reservation_for_tests();
        assert!(global.absorb(local).is_err());
        assert!(global.handles.is_empty());
    }

    #[test]
    fn both_namespaces_detect_handle_collision_and_accept_exact_duplicates() {
        let handle = [0x35; 16];
        for namespace in [Namespace::Syntax, Namespace::Occurrence] {
            let mut log = IdentityRegistry::logged();
            let id = log.admit(minted(namespace, handle, 1)).unwrap();
            assert_eq!(id, format!("{}{}", namespace.prefix(), "35".repeat(16)));
            assert_eq!(log.admit(minted(namespace, handle, 1)).unwrap(), id);
            assert_eq!(log.order.as_ref().unwrap().len(), 1);
            assert!(
                log.admit(minted(namespace, handle, 2))
                    .unwrap_err()
                    .to_string()
                    .contains("identity handle collision")
            );
            let mut global = IdentityRegistry::default();
            global.absorb(log).unwrap();
            assert_eq!(global.admit(minted(namespace, handle, 1)).unwrap(), id);
            assert!(
                global
                    .admit(minted(namespace, handle, 2))
                    .unwrap_err()
                    .to_string()
                    .contains("identity handle collision")
            );
            assert_eq!(global.handles.len(), 1);
        }
    }

    #[test]
    fn ordered_absorb_reports_first_inserted_collision_not_handle_sort_order() {
        let first = [0xf0; 16];
        let second = [0x10; 16];
        let mut logged = IdentityRegistry::logged();
        logged.admit(minted(Namespace::Syntax, first, 1)).unwrap();
        logged.admit(minted(Namespace::Syntax, second, 2)).unwrap();
        logged.admit(minted(Namespace::Syntax, first, 1)).unwrap();
        assert_eq!(logged.order.as_ref().unwrap().len(), 2);

        let mut first_only = IdentityRegistry::logged();
        first_only
            .admit(minted(Namespace::Syntax, first, 1))
            .unwrap();
        let mut second_only = IdentityRegistry::logged();
        second_only
            .admit(minted(Namespace::Syntax, second, 2))
            .unwrap();
        let mut global = IdentityRegistry::default();
        global.inject_collision_for_tests(&first_only);
        global.inject_collision_for_tests(&second_only);
        let error = global.absorb(logged).unwrap_err().to_string();
        assert!(error.contains(&"f0".repeat(16)), "{error}");
        assert!(!error.contains(&"10".repeat(16)), "{error}");
    }

    #[test]
    fn matching_truncated_bytes_are_separate_across_namespaces_and_logs_keep_order() {
        let mut log = IdentityRegistry::logged();
        let first = [1; 16];
        let second = [2; 16];
        log.admit(minted(Namespace::Occurrence, first, 3)).unwrap();
        log.admit(minted(Namespace::Syntax, second, 4)).unwrap();
        log.admit(minted(Namespace::Syntax, first, 5)).unwrap();
        log.admit(minted(Namespace::Occurrence, first, 3)).unwrap();
        assert_eq!(
            log.order.as_ref().unwrap(),
            &vec![
                (Namespace::Occurrence, first),
                (Namespace::Syntax, second),
                (Namespace::Syntax, first),
            ]
        );
        let mut global = IdentityRegistry::default();
        global.absorb(log).unwrap();
        assert_eq!(global.handles.len(), 3);
        assert!(global.admit(minted(Namespace::Syntax, first, 5)).is_ok());
    }
}
