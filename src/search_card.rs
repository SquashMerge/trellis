//! Exact, search-only form-c card preparation and serialization.
//!
//! Inputs are measured during the existing native parse. This module neither reads
//! source files nor parses source, and has no dependency on display keys.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::native_evidence::Header;
use crate::native_ids::{canonical, digest};

pub const DOC_BYTES: usize = 600;
pub const LITERAL_BYTES: usize = 100;
pub const LITERAL_COUNT: usize = 20;
pub const TYPE_COUNT: usize = 30;
pub const CALL_COUNT: usize = 40;
pub const HEAD_SOURCE_BYTES: usize = 1_200;

/// A closed format identifier, also used by the supported model descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CardFormat {
    V1,
}

impl CardFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "trellis.search-card.v1",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct Ancestor {
    pub kind: String,
    pub name: Option<String>,
    pub self_type: Option<String>,
    pub trait_name: Option<String>,
}

/// Payloads are already syntactically selected and attached to this declaration.
/// Lists arrive in (startByte, endByte, nativeOrdinal) order; head is the captured
/// declaration range, not a first-line fallback or a nested-body-filtered view.
#[derive(Clone, Copy, Debug, Default)]
pub struct MeasuredFacts<'a> {
    pub doc: &'a str,
    pub calls: &'a [String],
    pub body_types: &'a [String],
    pub strings: &'a [String],
    pub members: &'a [String],
    pub head: &'a str,
}

/// Prepared once for storage and card production, without rereading source.
#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
pub struct PreparedFacts {
    pub doc: String,
    pub calls: Vec<String>,
    pub types: Vec<String>,
    pub strings: Vec<String>,
    pub error_codes: Vec<String>,
    pub members: Vec<String>,
    pub head: String,
}

pub struct CardInput<'a> {
    pub kind: &'a str,
    pub name: &'a str,
    pub ancestors: &'a [Ancestor],
    pub path: &'a str,
    pub header: &'a Header,
    pub facts: &'a PreparedFacts,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct SearchCard {
    pub text: String,
    pub hash: String,
}

pub fn is_container(kind: &str) -> bool {
    matches!(kind, "type" | "implementation" | "namespace" | "module")
}

pub fn is_eligible(kind: &str, name: Option<&str>) -> bool {
    (is_container(kind) || matches!(kind, "function" | "method"))
        && name.is_some_and(|name| !name.is_empty())
}

/// Longest scalar-safe UTF-8 prefix, with no marker or replacement character.
pub fn prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Only ASCII space, tab, CR and LF participate in metadata whitespace folding.
pub fn space(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut pending = false;
    for ch in text.chars() {
        if matches!(ch, ' ' | '\t' | '\r' | '\n') {
            pending = !result.is_empty();
        } else {
            if pending {
                result.push(' ');
                pending = false;
            }
            result.push(ch);
        }
    }
    result
}

/// The cap applies to source bytes BEFORE folding horizontal whitespace.
pub fn prepare_head(source: &str) -> String {
    let mut result = String::new();
    let mut horizontal = false;
    for ch in prefix(source, HEAD_SOURCE_BYTES).chars() {
        if matches!(ch, ' ' | '\t') {
            if !horizontal {
                result.push(' ');
            }
            horizontal = true;
        } else {
            result.push(ch);
            horizontal = false;
        }
    }
    result
}

fn unique_nonempty(values: impl IntoIterator<Item = String>, limit: usize) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for value in values {
        if !value.is_empty() && seen.insert(value.clone()) {
            result.push(value);
            if result.len() == limit {
                break;
            }
        }
    }
    result
}

pub fn prepare_facts(kind: &str, header: &Header, measured: MeasuredFacts<'_>) -> PreparedFacts {
    let strings = unique_nonempty(
        measured
            .strings
            .iter()
            .map(|value| prefix(value, LITERAL_BYTES).to_owned()),
        LITERAL_COUNT,
    );
    let types = header
        .parameters
        .iter()
        .filter_map(|parameter| parameter.type_name.as_deref())
        .chain(header.result_type.as_deref())
        .chain(measured.body_types.iter().map(String::as_str));
    PreparedFacts {
        doc: prefix(&space(measured.doc), DOC_BYTES).to_owned(),
        calls: unique_nonempty(measured.calls.iter().map(|value| space(value)), CALL_COUNT),
        types: unique_nonempty(types.map(space), TYPE_COUNT),
        error_codes: error_codes(&strings),
        strings,
        members: if is_container(kind) {
            unique_nonempty(measured.members.iter().cloned(), usize::MAX)
        } else {
            Vec::new()
        },
        head: prepare_head(measured.head),
    }
}

/// Maximal ASCII identifier runs, extracted only from retained literal payloads.
pub fn error_codes(strings: &[String]) -> Vec<String> {
    unique_nonempty(
        strings.iter().flat_map(|literal| {
            literal
                .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .filter(|run| {
                    let mut parts = run.split('_');
                    let Some(first) = parts.next() else {
                        return false;
                    };
                    let mut first_bytes = first.bytes();
                    if !first_bytes
                        .next()
                        .is_some_and(|byte| byte.is_ascii_lowercase())
                        || !first_bytes
                            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                    {
                        return false;
                    }
                    let mut has_separator = false;
                    for part in parts {
                        has_separator = true;
                        if part.is_empty()
                            || !part
                                .bytes()
                                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                        {
                            return false;
                        }
                    }
                    has_separator
                })
                .map(str::to_owned)
        }),
        usize::MAX,
    )
}

/// Header arrays preserve order, repetitions and empty entries. Only the initial
/// modifier/kind/name components omit empty normalized values.
pub fn render_signature(header: &Header) -> String {
    let mut signature = header
        .modifiers
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(header.kind.as_str()))
        .chain(header.name.as_deref())
        .map(space)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !header.type_parameters.is_empty() {
        signature.push('<');
        signature.push_str(
            &header
                .type_parameters
                .iter()
                .map(|value| space(value))
                .collect::<Vec<_>>()
                .join(", "),
        );
        signature.push('>');
    }
    if !is_container(&header.kind) || !header.parameters.is_empty() {
        signature.push('(');
        for (index, parameter) in header.parameters.iter().enumerate() {
            if index != 0 {
                signature.push_str(", ");
            }
            if parameter.variadic {
                signature.push_str("...");
            }
            signature.push_str(&space(parameter.name.as_deref().unwrap_or("_")));
            if let Some(type_name) = &parameter.type_name {
                signature.push_str(": ");
                signature.push_str(&space(type_name));
            }
        }
        signature.push(')');
    }
    if let Some(result) = &header.result_type {
        signature.push_str(" -> ");
        signature.push_str(&space(result));
    }
    if !header.bases.is_empty() {
        signature.push_str(" : ");
        signature.push_str(
            &header
                .bases
                .iter()
                .map(|value| space(value))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    signature
}

fn line(out: &mut Vec<u8>, label: &str, value: Value) {
    if !out.is_empty() {
        out.push(b'\n');
    }
    out.extend_from_slice(label.as_bytes());
    out.extend_from_slice(b": ");
    out.extend_from_slice(&canonical(&value));
}

/// Serialize prepared measured values using #22 canonical JSON, including its
/// all-control-characters-as-\\u00xx escaping (not serde_json's short escapes).
pub fn serialize(input: &CardInput<'_>) -> SearchCard {
    let mut bytes = Vec::new();
    line(&mut bytes, "kind", json!(input.kind));
    line(&mut bytes, "name", json!(input.name));
    let ancestors: Vec<_> = input
        .ancestors
        .iter()
        .map(|ancestor| {
            json!({
                "kind": ancestor.kind,
                "name": ancestor.name,
                "selfType": ancestor.self_type,
                "trait": ancestor.trait_name,
            })
        })
        .collect();
    line(&mut bytes, "ancestors", json!(ancestors));
    line(&mut bytes, "path", json!(input.path));
    line(
        &mut bytes,
        "signature",
        json!(render_signature(input.header)),
    );
    let facts = input.facts;
    if !facts.doc.is_empty() {
        line(&mut bytes, "doc", json!(facts.doc));
    }
    for (label, values) in [
        ("calls", &facts.calls),
        ("types", &facts.types),
        ("strings", &facts.strings),
        ("error codes", &facts.error_codes),
        ("members", &facts.members),
    ] {
        if !values.is_empty() {
            line(&mut bytes, label, json!(values));
        }
    }
    bytes.extend_from_slice(b"\nbody:\n");
    bytes.extend_from_slice(facts.head.as_bytes());
    let hash = card_hash(&bytes);
    SearchCard {
        // Every component was UTF-8; canonical JSON and the separators preserve it.
        text: String::from_utf8(bytes).expect("card is assembled from UTF-8"),
        hash,
    }
}

pub fn card_hash(exact_card_bytes: &[u8]) -> String {
    digest(b"trellis.search-card.v1\0", exact_card_bytes)
}
