//! Fixtures shared by more than one of this module's test modules.
use super::*;
use crate::native_evidence::{Coverage, Declaration, DocumentKey, Provenance};
use crate::store::mcp_read::{
    McpDeclaration, McpDeclarationWithLines, McpDocumentVersion, McpFindSymbol,
    McpFindSymbolWithLines, McpFindWithLines, McpItemSource, McpLines, McpOutgoing,
    McpOutgoingCall, McpOutgoingCallWithLines, McpOutgoingWithLines,
};

/// Tiers whose listed languages (by name) are pending with the given status and reason.
pub(super) fn pending_tiers(entries: &[(&str, crate::model::SemanticStatus, &str)]) -> Tiers {
    let mut tiers = Tiers::default();
    for (name, status, reason) in entries {
        let at = LANGUAGES
            .iter()
            .position(|language| language == name)
            .unwrap();
        tiers[at].pending = Some(crate::store::LanguagePending {
            status: *status,
            reason: (*reason).into(),
        });
    }
    tiers
}

/// Rust selects a `rust-analyzer` import; the other languages have no producer.
pub(super) fn rust_semantic(freshness: &'static str, counts: [u64; 5]) -> Tiers {
    let mut tiers = Tiers::default();
    for (tier, language) in tiers.iter_mut().zip(LANGUAGES) {
        tier.freshness = "unavailable";
        tier.note = Some("semantic producer does not cover this language; showing syntax".into());
        if language == "rust" {
            tier.producer_id = Some("rust-analyzer".into());
            tier.note = None;
            tier.freshness = freshness;
            tier.counts = counts;
        }
    }
    tiers
}

fn pin() -> crate::model::IndexPin {
    serde_json::from_value(json!({
        "indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":1
    }))
    .unwrap()
}

fn coverage(document: &DocumentKey, state: &str) -> Coverage {
    Coverage {
        producer_id: "native:v1".into(),
        language: document.language.clone(),
        source_set_id: document.source_set_id.clone(),
        document_path: document.path.clone(),
        revision_id: "revision:v1:test".into(),
        requested: true,
        selected: true,
        state: state.into(),
        supported_roles: vec![],
        observed_roles: vec![],
        diagnostic: (state != "complete").then(|| "fixture".into()),
    }
}

/// Hits `(name, source set, path)`, each in its own proof unless a path repeats.
pub(super) fn fixture(
    hits: &[(String, &str, String)],
    scope: [[u64; 5]; 5],
    has_more: bool,
) -> McpFindWithLines {
    let mut symbols = Vec::new();
    let mut provenance: Vec<Provenance> = Vec::new();
    for (i, (name, source_set, path)) in hits.iter().enumerate() {
        let document = DocumentKey {
            source_set_id: (*source_set).into(),
            language: "javascript".into(),
            path: path.clone(),
        };
        let proof_id = format!("proof:{source_set}:{path}");
        let declaration: Declaration = serde_json::from_value(json!({
            "syntaxId": format!("sid:{i}"), "document":document,
            "revisionId":"revision:v1:test", "kind":"function", "role":"production", "name":name,
            "lookupKey":null, "ancestors":[],
            "key":{"kind":"function","name":name,"signature":null,"ordinal":0},
            "range":{"start":0,"end":10}, "nameRange":null,
            "header":{"kind":"function","name":name,"modifiers":[],
                "typeParameters":[],"parameters":[],"resultType":null,"bases":[]},
            "provenanceId":proof_id
        }))
        .unwrap();
        if !provenance.iter().any(|p| p.id == proof_id) {
            provenance.push(Provenance {
                id: proof_id,
                producer_id: "native:v1".into(),
                document: document.clone(),
                revision_id: "revision:v1:test".into(),
                content_hash: "a".repeat(64),
                evidence_kind: "measuredSyntax".into(),
                basis: None,
                freshness: "fresh".into(),
                derived_from: None,
            });
        }
        symbols.push(McpFindSymbol {
            coverage: coverage(&document, "complete"),
            declaration,
            content_hash: "a".repeat(64),
        });
    }
    McpFindWithLines {
        symbols: symbols
            .into_iter()
            .map(|symbol| {
                let proof = provenance
                    .iter()
                    .find(|p| p.id == symbol.declaration.provenance_id)
                    .unwrap();
                McpFindSymbolWithLines {
                    source: McpItemSource {
                        stable_lookup_key: None,
                        document: McpDocumentVersion {
                            document: symbol.declaration.document.clone(),
                            revision_id: symbol.declaration.revision_id.clone(),
                            content_hash: symbol.content_hash.clone(),
                            provenance_id: proof.id.clone(),
                        },
                        lines: Some(McpLines { start: 1, end: 1 }),
                    },
                    symbol,
                }
            })
            .collect(),
        scope,
        provenance,
        has_more,
        pin: pin(),
    }
}

pub(super) fn with_lines(result: McpOutgoing) -> McpOutgoingWithLines {
    let owner_doc = McpDocumentVersion {
        document: result.owner.declaration.document.clone(),
        revision_id: result.owner.declaration.revision_id.clone(),
        content_hash: result.owner.content_hash.clone(),
        provenance_id: result.owner.declaration.provenance_id.clone(),
    };
    let calls = result
        .calls
        .into_iter()
        .map(|selected| {
            let candidate_documents = selected
                .candidates
                .iter()
                .map(|candidate| McpDocumentVersion {
                    document: candidate.declaration.document.clone(),
                    revision_id: candidate.declaration.revision_id.clone(),
                    content_hash: candidate.provenance.content_hash.clone(),
                    provenance_id: candidate.declaration.provenance_id.clone(),
                })
                .collect();
            McpOutgoingCallWithLines {
                selected,
                source: McpItemSource {
                    stable_lookup_key: None,
                    document: owner_doc.clone(),
                    lines: Some(McpLines { start: 1, end: 1 }),
                },
                candidate_documents,
            }
        })
        .collect();
    McpOutgoingWithLines {
        owner: McpDeclarationWithLines {
            selected: result.owner,
            source: McpItemSource {
                stable_lookup_key: None,
                document: owner_doc,
                lines: Some(McpLines { start: 1, end: 1 }),
            },
        },
        calls,
        has_more: result.has_more,
        external_omitted: None,
    }
}

pub(super) fn owner() -> McpDeclaration {
    let document = DocumentKey {
        source_set_id: "source-set:v1:test".into(),
        language: "javascript".into(),
        path: "a.js".into(),
    };
    let declaration: Declaration = serde_json::from_value(json!({
        "syntaxId":format!("sid:v1:{}","a".repeat(32)),"document":document,
        "revisionId":"revision:v2:test","kind":"function","role":"production","name":"Owner",
        "lookupKey":"Owner","ancestors":[],
        "key":{"kind":"function","name":"Owner","signature":null,"ordinal":0},
        "range":{"start":0,"end":20},"nameRange":null,
        "header":{"kind":"function","name":"Owner","modifiers":[],
            "typeParameters":[],"parameters":[],"resultType":null,"bases":[]},
        "provenanceId":"native-proof:v1:test"
    }))
    .unwrap();
    McpDeclaration {
        declaration,
        content_hash: "a".repeat(64),
        coverage: vec![Coverage {
            producer_id: "native:v1".into(),
            language: "javascript".into(),
            source_set_id: "source-set:v1:test".into(),
            document_path: "a.js".into(),
            revision_id: "revision:v2:test".into(),
            requested: true,
            selected: true,
            state: "partial".into(),
            supported_roles: vec!["definition".into(), "call".into()],
            observed_roles: vec!["definition".into()],
            diagnostic: Some("incomplete".into()),
        }],
        provenance: vec![Provenance {
            id: "native-proof:v1:test".into(),
            producer_id: "native:v1".into(),
            document: document.clone(),
            revision_id: "revision:v2:test".into(),
            content_hash: "a".repeat(64),
            evidence_kind: "measuredSyntax".into(),
            basis: None,
            freshness: "fresh".into(),
            derived_from: None,
        }],
        pin: serde_json::from_value(
            json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000",
            "indexRevision":1}),
        )
        .unwrap(),
    }
}

pub(super) fn required_call(clipped: bool) -> McpOutgoingCall {
    let owner = owner();
    McpOutgoingCall {
        call: serde_json::from_value(json!({
            "id":"cid:v1:1","ownerSyntaxId":owner.declaration.syntax_id,
            "ordinal":0,"document":owner.declaration.document,
            "revisionId":"revision:v2:test","range":{"start":21,"end":30},
            "calleeRange":{"start":21,"end":25},"spelling":"Target",
            "regionIds":[],"provenanceId":"native-proof:v1:test"
        }))
        .unwrap(),
        content_hash: "a".repeat(64),
        callee_text: Some("é".repeat(512)),
        callee_text_truncated: clipped,
        candidates: vec![],
        omitted: Some(0),
    }
}
