//! Exact measured-call owners for Architecture and rules navigation.
use crate::references::DocumentInput;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Caller {
    stable_lookup_key: String,
    syntax_id: String,
    name: String,
    document: crate::native_evidence::DocumentKey,
}

pub(crate) struct Callers<'a> {
    calls: BTreeMap<&'a str, &'a crate::native_evidence::Call>,
    owners: BTreeMap<&'a str, Caller>,
}
impl<'a> Callers<'a> {
    pub(crate) fn new(doc: &'a DocumentInput) -> Self {
        let keys = if doc.calls.is_empty() {
            BTreeMap::new()
        } else {
            lookup_keys(doc)
        };
        Self::with_keys(doc, &keys)
    }
    pub(crate) fn with_keys(doc: &'a DocumentInput, keys: &BTreeMap<String, String>) -> Self {
        let owners = doc
            .declarations
            .iter()
            .filter(|d| crate::metrics::callable(&d.kind))
            .filter_map(|d| {
                Some((
                    d.syntax_id.as_str(),
                    Caller {
                        stable_lookup_key: keys.get(&d.syntax_id)?.clone(),
                        syntax_id: d.syntax_id.clone(),
                        name: d.name.clone()?,
                        document: d.document.clone(),
                    },
                ))
            })
            .collect();
        Self {
            calls: doc
                .calls
                .iter()
                .map(|call| (call.id.as_str(), call))
                .collect(),
            owners,
        }
    }
    /// The call exists and the range is its invocation or callee spelling, whether or not
    /// its owner has a stable lookup key.
    pub(crate) fn measured(
        &self,
        call_id: Option<&str>,
        range: &crate::native_evidence::Range,
    ) -> bool {
        call_id
            .and_then(|id| self.calls.get(id))
            .is_some_and(|call| matches_site(call, range))
    }
    pub(crate) fn get(
        &self,
        call_id: Option<&str>,
        range: &crate::native_evidence::Range,
    ) -> (Option<Caller>, Option<&'static str>) {
        let Some(call_id) = call_id else {
            return (None, Some("syntax_only"));
        };
        let Some(call) = self.calls.get(call_id) else {
            return (None, Some("enclosing_callable_unknown"));
        };
        if !matches_site(call, range) {
            return (None, Some("syntax_only"));
        }
        match self.owners.get(call.owner_syntax_id.as_str()) {
            Some(caller) => (Some(caller.clone()), None),
            None => (None, Some("stable_lookup_unavailable")),
        }
    }
    pub(crate) fn annotate(&self, evidence: &mut Value) {
        let (caller, reason) = if evidence["semanticBoundary"] == true {
            (None, Some("semantic_boundary"))
        } else {
            let range =
                serde_json::from_value(evidence["range"].clone()).expect("native evidence range");
            self.get(evidence["callId"].as_str(), &range)
        };
        evidence["caller"] = serde_json::to_value(caller).expect("caller serialization");
        evidence["callerReason"] = serde_json::to_value(reason).expect("reason serialization");
    }
}

/// Stable lookup keys of the document's declarations by syntax ID.
pub(crate) fn lookup_keys(doc: &DocumentInput) -> BTreeMap<String, String> {
    // A partial Rust impl parse cannot supply a stable written key.
    crate::symbol_lookup::keys_from_source(
        &doc.key.path,
        &doc.key.language,
        &doc.file.text,
        &doc.declarations,
    )
    .unwrap_or_default()
}

pub(super) fn matches_site(
    call: &crate::native_evidence::Call,
    range: &crate::native_evidence::Range,
) -> bool {
    // Native callee ranges can be only the final name of a qualified invocation.
    *range == call.range
        || call.callee_range.as_ref().is_some_and(|callee| {
            (callee.start <= range.start && range.end <= callee.end)
                || (range.start == call.range.start && range.end == callee.end)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_measured_invocation_and_callee_sites_have_a_caller() {
        use crate::native_evidence::{Call, DocumentKey, Range};
        let document = DocumentKey {
            source_set_id: "source".into(),
            language: "rust".into(),
            path: "src/lib.rs".into(),
        };
        let call = Call {
            id: "call".into(),
            owner_syntax_id: "owner".into(),
            ordinal: 0,
            document: document.clone(),
            revision_id: "revision".into(),
            range: Range { start: 10, end: 50 },
            callee_range: Some(Range { start: 20, end: 24 }),
            spelling: Some("call".into()),
            region_ids: vec![],
            provenance_id: "proof".into(),
        };
        let callers = Callers {
            calls: BTreeMap::from([("call", &call)]),
            owners: BTreeMap::from([(
                "owner",
                Caller {
                    stable_lookup_key: "src/lib.rs::owner".into(),
                    syntax_id: "owner".into(),
                    name: "owner".into(),
                    document,
                },
            )]),
        };
        for range in [
            Range { start: 10, end: 50 },
            Range { start: 20, end: 24 },
            Range { start: 10, end: 24 },
        ] {
            assert!(callers.get(Some("call"), &range).0.is_some());
        }
        assert_eq!(
            callers.get(Some("call"), &Range { start: 30, end: 40 }).1,
            Some("syntax_only")
        );
    }
}
