//! Closed W1 wire projection; no Store admission occurs in this layer.
use serde_json::{Value, json};

pub fn describe(
    id: &Value,
    label: &str,
    state: &str,
    progress: Option<Value>,
    counts: Option<[[u64; 5]; 4]>,
    basis: Option<Value>,
    degraded: bool,
) -> Value {
    let partial = counts
        .as_ref()
        .is_none_or(|rows| rows.iter().any(|row| row[2] != 0 || row[3] != 0));
    let languages: Vec<Value> = counts.map_or_else(Vec::new, |rows| {
        ["java", "rust", "python", "javascript"].into_iter().zip(rows).map(|(language, row)| {
            json!({"language":language,"evidenceTier":"syntax","coverage":{
                "selected":row[0],"complete":row[1],"partial":row[2],"failed":row[3],"unselected":row[4]}})
        }).collect()
    });
    let deadline_ms = u64::try_from(super::CALL_DEADLINE.as_millis()).unwrap_or(u64::MAX);
    json!({"schemaVersion":1,"requestId":id,"evidenceBasis":basis,
      "data":{"workspaceLabel":label,"indexState":state,"progress":progress,
      "watcherDegraded":degraded,"languages":languages,"toolVersion":1,"schemaVersion":1,
      "limits":{"requestBytes":16384,"responseBytes":65536,"sourceBytes":16384,
        "sourceLines":200,"defaultLimit":20,"maxLimit":50,"calleeTextBytes":1024,
        "hintWorkMs":1000,"deadlineMs":deadline_ms,"concurrency":4}},
      "warnings":[],"partial":partial,"truncated":false,"truncationReason":null})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn describe_closed_ready_and_missing_head() {
        let empty = describe(&json!(1), "project", "reconciling", None, None, None, false);
        assert!(empty["evidenceBasis"].is_null());
        assert_eq!(empty["data"]["languages"], json!([]));
        assert_eq!(empty["partial"], true);
        let rows = [[0, 0, 0, 0, 0], [2, 1, 1, 0, 3], [0; 5], [0; 5]];
        let basis =
            json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":1});
        let ready = describe(
            &json!(2),
            "project",
            "ready",
            None,
            Some(rows),
            Some(basis.clone()),
            true,
        );
        assert_eq!(ready["evidenceBasis"], basis);
        assert_eq!(ready["data"]["watcherDegraded"], true);
        assert_eq!(
            ready["data"]["languages"][1]["coverage"],
            json!({
            "selected":2,"complete":1,"partial":1,"failed":0,"unselected":3})
        );
        assert_eq!(ready["partial"], true);
        assert_eq!(ready["data"]["languages"].as_array().unwrap().len(), 4);
    }
}

fn display_key(d: &crate::native_evidence::Declaration) -> String {
    let mut segments: Vec<String> = d
        .ancestors
        .iter()
        .map(|key| {
            key.name
                .clone()
                .unwrap_or_else(|| format!("{}@{}", key.kind, key.ordinal))
        })
        .collect();
    segments.push(
        d.name
            .clone()
            .unwrap_or_else(|| format!("{}@{}", d.kind, d.range.start)),
    );
    format!("{}#{}", d.document.path, segments.join("."))
}

/// A declaration cannot be shortened: its measured record and closure fit together or fail.
pub fn declaration(
    id: &Value,
    result: crate::store::mcp_read::McpDeclaration,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Option<Value> {
    let incomplete = result
        .coverage
        .iter()
        .any(|c| c.selected && c.state != "complete");
    let warnings = if incomplete {
        vec![
            json!({"code":"coverageIncomplete","message":"Selected syntax coverage is incomplete","provenanceId":null}),
            json!({"code":"syntaxOnly","message":"Only syntax evidence is available","provenanceId":null}),
        ]
    } else {
        vec![
            json!({"code":"syntaxOnly","message":"Only syntax evidence is available","provenanceId":null}),
        ]
    };
    let basis = json!({"indexGeneration":result.pin.index_generation.to_string(),
        "indexRevision":result.pin.index_revision});
    let envelope = json!({"schemaVersion":1,"requestId":id,"workspace":root,"catchingUp":catching_up,
        "evidenceBasis":basis,
        "data":{"displayKey":display_key(&result.declaration),"declaration":result.declaration,
            "contentHash":result.content_hash,
            "evidence":{"evidenceTier":"syntax","semanticBasis":null,"freshness":"fresh","staleBecause":[]},
            "coverage":result.coverage,"provenance":result.provenance},
        "warnings":warnings,"partial":incomplete,"truncated":false,"truncationReason":null});
    let frame =
        json!({"jsonrpc":"2.0","id":id,"result":super::tools::result(envelope.clone(), modern)});
    (serde_json::to_vec(&frame).ok()?.len() < super::wire::RESPONSE_BYTES).then_some(envelope)
}

/// Budget the actual dual-content JSON-RPC frame, not the structured envelope alone.
pub fn find(
    id: &Value,
    result: crate::store::mcp_read::McpFind,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Option<Value> {
    let incomplete = result
        .coverage
        .iter()
        .any(|c| c.selected && c.state != "complete");
    let warnings = if incomplete {
        vec![
            json!({"code":"coverageIncomplete","message":"Selected syntax coverage is incomplete","provenanceId":null}),
            json!({"code":"syntaxOnly","message":"Only syntax evidence is available","provenanceId":null}),
        ]
    } else {
        vec![
            json!({"code":"syntaxOnly","message":"Only syntax evidence is available","provenanceId":null}),
        ]
    };
    let basis = json!({"indexGeneration":result.pin.index_generation.to_string(),
        "indexRevision":result.pin.index_revision});
    let mut symbols = Vec::new();
    let mut proofs = Vec::new();
    let mut last = None;
    for symbol in &result.symbols {
        let d = &symbol.declaration;
        let display = display_key(d);
        symbols.push(
            json!({"symbolId":d.syntax_id,"name":d.name,"displayKey":display,
            "kind":d.kind,"document":d.document,"range":d.range,"nameRange":d.name_range,
            "contentHash":symbol.content_hash,"provenanceId":d.provenance_id,
            "evidenceTier":"syntax","semanticBasis":null,"freshness":"fresh","staleBecause":[]}),
        );
        if !proofs
            .iter()
            .any(|p: &crate::native_evidence::Provenance| p.id == d.provenance_id)
        {
            proofs.push(
                result
                    .provenance
                    .iter()
                    .find(|p| p.id == d.provenance_id)?
                    .clone(),
            );
            proofs.sort_by(|a, b| a.id.cmp(&b.id));
        }
    }
    // A first, empty prefix is mandatory; a failure here must never discard coverage.
    for n in 0..=symbols.len() {
        let required: std::collections::BTreeSet<&str> = result.symbols[..n]
            .iter()
            .map(|s| s.declaration.provenance_id.as_str())
            .collect();
        let closure: Vec<_> = proofs
            .iter()
            .filter(|p| required.contains(p.id.as_str()))
            .collect();
        let truncated = n < symbols.len() || result.has_more;
        let envelope = json!({"schemaVersion":1,"requestId":id,"workspace":root,
            "catchingUp":catching_up,"evidenceBasis":basis,
            "data":{"symbols":&symbols[..n],"coverage":result.coverage,"provenance":closure},
            "warnings":warnings,"partial":incomplete || truncated,"truncated":truncated,
            "truncationReason": if n < symbols.len() {Some("response_bytes")}
                else if result.has_more {Some("limit")} else {None}});
        let response = super::tools::result(envelope.clone(), modern);
        let frame = json!({"jsonrpc":"2.0","id":id,"result":response});
        if serde_json::to_vec(&frame).ok()?.len() < super::wire::RESPONSE_BYTES {
            last = Some(envelope);
        } else {
            break;
        }
    }
    last
}

#[cfg(test)]
mod find_tests {
    use super::*;
    use crate::mcp::wire::{RESPONSE_BYTES, write_response};
    use crate::native_evidence::{Coverage, Declaration, DocumentKey, Provenance};
    use crate::store::mcp_read::{McpFind, McpFindSymbol};

    fn pin() -> crate::model::IndexPin {
        serde_json::from_value(json!({
            "indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":1
        }))
        .unwrap()
    }

    fn coverage(diagnostic: usize) -> Coverage {
        Coverage {
            producer_id: "native:v1".into(),
            language: "javascript".into(),
            source_set_id: "source-set:v1:test".into(),
            document_path: "src/a.js".into(),
            revision_id: "revision:v1:test".into(),
            requested: true,
            selected: true,
            state: "failed".into(),
            supported_roles: vec![],
            observed_roles: vec![],
            diagnostic: Some("x".repeat(diagnostic)),
        }
    }

    fn fixture(names: &[String], has_more: bool) -> McpFind {
        let mut symbols = Vec::new();
        let mut provenance = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let document = DocumentKey {
                source_set_id: "source-set:v1:test".into(),
                language: "javascript".into(),
                path: format!("src/{i}.js"),
            };
            let proof_id = format!("proof:{i}");
            let declaration: Declaration = serde_json::from_value(json!({
                "syntaxId": format!("sid:{i}"), "document":document,
                "revisionId":"revision:v1:test", "kind":"function", "name":name,
                "lookupKey":null, "ancestors":[],
                "key":{"kind":"function","name":name,"signature":null,"ordinal":0},
                "range":{"start":0,"end":10}, "nameRange":null,
                "header":{"kind":"function","name":name,"modifiers":[],
                    "typeParameters":[],"parameters":[],"resultType":null,"bases":[]},
                "provenanceId":proof_id
            }))
            .unwrap();
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
            symbols.push(McpFindSymbol {
                declaration,
                content_hash: "a".repeat(64),
            });
        }
        McpFind {
            symbols,
            provenance,
            coverage: vec![coverage(0)],
            has_more,
            pin: pin(),
        }
    }

    fn assert_wire(envelope: Value, id: &Value, modern: bool) -> Value {
        let result = super::super::tools::result(envelope.clone(), modern);
        assert_eq!(result["structuredContent"], envelope);
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, envelope);
        assert_eq!(result.get("resultType").is_some(), modern);
        let frame = json!({"jsonrpc":"2.0","id":id,"result":result});
        let mut bytes = Vec::new();
        write_response(&mut bytes, &frame).unwrap();
        assert!(bytes.len() <= RESPONSE_BYTES);
        assert_eq!(bytes.last(), Some(&b'\n'));
        envelope
    }

    #[test]
    fn zero_hit_coverage_cannot_be_trimmed_to_fit_duplicate_wire() {
        let result = |diagnostic| McpFind {
            symbols: vec![],
            coverage: vec![coverage(diagnostic)],
            provenance: vec![],
            has_more: false,
            pin: pin(),
        };
        let root = std::path::Path::new("/workspace");
        let id = json!("\"\\\n".repeat(30));
        for modern in [false, true] {
            let envelope = find(&id, result(20_000), root, false, modern).unwrap();
            assert_eq!(envelope["data"]["coverage"], json!([coverage(20_000)]));
            assert_eq!(envelope["data"]["symbols"], json!([]));
            assert_eq!(envelope["partial"], true);
            assert_eq!(envelope["truncated"], false);
            assert_wire(envelope, &id, modern);
            assert!(find(&id, result(33_000), root, false, modern).is_none());

            // Find the exact wire threshold, including both content copies, RPC ID,
            // modern resultType, and the LF written by write_response.
            let (mut low, mut high) = (20_000, 33_000);
            while high - low > 1 {
                let mid = (low + high) / 2;
                if find(&id, result(mid), root, false, modern).is_some() {
                    low = mid;
                } else {
                    high = mid;
                }
            }
            let last = find(&id, result(low), root, false, modern).unwrap();
            assert_wire(last, &id, modern);
            assert!(find(&id, result(low + 1), root, false, modern).is_none());
        }
    }

    #[test]
    fn find_keeps_longest_fitting_hit_prefix_and_only_its_proofs() {
        let root = std::path::Path::new("/workspace");
        let id = json!("id\n\"\\");
        // Each name appears in a symbol and its displayKey, then again in text.
        // Two fit; the third would overrun the complete JSON-RPC frame.
        let names = vec!["A".repeat(6_000), "B".repeat(6_000), "C".repeat(6_000)];
        for modern in [false, true] {
            let envelope = find(&id, fixture(&names, false), root, true, modern).unwrap();
            assert_wire(envelope.clone(), &id, modern);
            let data = &envelope["data"];
            assert_eq!(data["coverage"], json!([coverage(0)]));
            assert_eq!(data["symbols"].as_array().unwrap().len(), 2);
            assert_eq!(data["symbols"][0]["symbolId"], "sid:0");
            assert_eq!(data["symbols"][1]["symbolId"], "sid:1");
            assert_eq!(data["provenance"].as_array().unwrap().len(), 2);
            assert_eq!(data["provenance"][0]["id"], "proof:0");
            assert_eq!(data["provenance"][1]["id"], "proof:1");
            assert_eq!(envelope["catchingUp"], true);
            assert_eq!(envelope["truncated"], true);
            assert_eq!(envelope["truncationReason"], "response_bytes");
            assert_eq!(envelope["partial"], true);

            // With only two hits the full response fits; no shorter prefix
            // can be the longest fitting prefix of the original result.
            let two = find(&id, fixture(&names[..2], false), root, true, modern).unwrap();
            assert_eq!(two["data"]["symbols"], data["symbols"]);
            assert_eq!(two["truncated"], false);
            assert_wire(two, &id, modern);
        }
    }

    #[test]
    fn all_hits_fit_with_limit_truncation_and_full_proof_closure() {
        let names = vec!["first".into(), "second".into()];
        let id = json!(1);
        let envelope = find(
            &id,
            fixture(&names, true),
            std::path::Path::new("/workspace"),
            false,
            true,
        )
        .unwrap();
        assert_eq!(envelope["data"]["symbols"].as_array().unwrap().len(), 2);
        assert_eq!(envelope["data"]["provenance"].as_array().unwrap().len(), 2);
        assert_eq!(envelope["truncationReason"], "limit");
        assert_eq!(envelope["truncated"], true);
        assert_wire(envelope, &id, true);
    }
}

/// Calls have priority over optional hints in the complete, dual-content wire budget.
pub fn outgoing(
    id: &Value,
    result: crate::store::mcp_read::McpOutgoing,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Option<Value> {
    use std::collections::BTreeMap;
    let owner_display = display_key(&result.owner.declaration);
    let basis = json!({"indexGeneration":result.owner.pin.index_generation.to_string(),
        "indexRevision":result.owner.pin.index_revision});
    let fits = |envelope: &Value| -> bool {
        let frame = json!({"jsonrpc":"2.0","id":id,
            "result":super::tools::result(envelope.clone(), modern)});
        serde_json::to_vec(&frame).is_ok_and(|bytes| bytes.len() < super::wire::RESPONSE_BYTES)
    };
    let build = |count: usize, shown: &[usize]| -> Value {
        let mut coverage = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        for row in &result.owner.coverage {
            coverage.insert(
                (
                    row.producer_id.clone(),
                    row.language.clone(),
                    row.source_set_id.clone(),
                    row.document_path.clone(),
                    row.revision_id.clone(),
                ),
                row.clone(),
            );
        }
        for row in &result.owner.provenance {
            provenance.insert(row.id.clone(), row.clone());
        }
        let mut calls = Vec::new();
        let mut byte_omission = false;
        let mut limited = result.has_more;
        let mut clipped = false;
        for (index, item) in result.calls[..count].iter().enumerate() {
            let text = item.callee_text.as_deref();
            clipped |= item.callee_text_truncated;
            let available = item.candidates.len().min(8);
            let n = shown[index];
            byte_omission |= n < available;
            limited |= item.candidates.len() > 8;
            for candidate in &item.candidates[..n] {
                let c = &candidate.coverage;
                coverage.insert(
                    (
                        c.producer_id.clone(),
                        c.language.clone(),
                        c.source_set_id.clone(),
                        c.document_path.clone(),
                        c.revision_id.clone(),
                    ),
                    c.clone(),
                );
                provenance.insert(
                    candidate.provenance.id.clone(),
                    candidate.provenance.clone(),
                );
            }
            let suggestions: Vec<Value> = item.candidates[..n]
                .iter()
                .map(|candidate| {
                    let d = &candidate.declaration;
                    json!({"syntaxId":d.syntax_id,"name":d.name,"document":d.document,
                    "revisionId":d.revision_id,"range":d.range,"provenanceId":d.provenance_id})
                })
                .collect();
            let call = &item.call;
            let label = match text {
                Some(text) => format!("{owner_display}@byte:{}:{text}", call.range.start),
                None => format!("{owner_display}@byte:{}", call.range.start),
            };
            calls.push(json!({"callId":call.id,"ownerSyntaxId":call.owner_syntax_id,
                "ordinal":call.ordinal,"document":call.document,"revisionId":call.revision_id,
                "range":call.range,"calleeRange":call.callee_range,
                "contentHash":item.content_hash,"provenanceId":call.provenance_id,
                "calleeText":text,"calleeTextTruncated":item.callee_text_truncated,
                "displayKey":label,"nativeCandidates":{"items":suggestions,
                    "omitted":item.omitted.map(|_| item.candidates.len().saturating_sub(n)),
                    "complete":false},"binding":null,"disposition":"unresolved",
                "targetId":null,"boundaryReason":"missingEvidence",
                "evidenceTier":"syntax","semanticBasis":null,"freshness":"fresh","staleBecause":[]}));
        }
        let mut coverage: Vec<_> = coverage.into_values().collect();
        let language_order = |language: &str| match language {
            "java" => 0,
            "rust" => 1,
            "python" => 2,
            _ => 3,
        };
        coverage.sort_by(|a, b| {
            (
                &a.producer_id,
                language_order(&a.language),
                &a.source_set_id,
                &a.document_path,
                &a.revision_id,
            )
                .cmp(&(
                    &b.producer_id,
                    language_order(&b.language),
                    &b.source_set_id,
                    &b.document_path,
                    &b.revision_id,
                ))
        });
        let incomplete = coverage.iter().any(|c| c.selected && c.state != "complete");
        let truncated = count < result.calls.len() || byte_omission || limited || clipped;
        let reason = if count < result.calls.len() || byte_omission {
            Some("response_bytes")
        } else if limited {
            Some("limit")
        } else if clipped {
            Some("callee_text")
        } else {
            None
        };
        let mut warnings = vec![];
        if incomplete {
            warnings.push(json!({"code":"coverageIncomplete",
                "message":"Selected syntax coverage is incomplete","provenanceId":null}));
        }
        warnings.push(json!({"code":"syntaxOnly",
            "message":"Only syntax evidence is available","provenanceId":null}));
        json!({"schemaVersion":1,"requestId":id,"workspace":root,"catchingUp":catching_up,
            "evidenceBasis":basis,"data":{"symbolId":result.owner.declaration.syntax_id,
                "displayKey":owner_display,"calls":calls,"coverage":coverage,
                "provenance":provenance.into_values().collect::<Vec<_>>()},
            "warnings":warnings,"partial":incomplete || truncated || count > 0,
            "truncated":truncated,"truncationReason":reason})
    };
    let mut count = 0;
    let mut shown = Vec::new();
    let empty = build(0, &[]);
    if !fits(&empty) {
        return None;
    }
    while count < result.calls.len() {
        shown.push(0);
        if !fits(&build(count + 1, &shown)) {
            shown.pop();
            break;
        }
        count += 1;
    }
    if count == 0 && !result.calls.is_empty() {
        return None;
    }
    let mut last = build(count, &shown);
    'candidate: for (index, item) in result.calls[..count].iter().enumerate() {
        for _ in 0..item.candidates.len().min(8) {
            shown[index] += 1;
            let next = build(count, &shown);
            if !fits(&next) {
                shown[index] -= 1;
                break 'candidate;
            }
            last = next;
        }
    }
    Some(last)
}

#[cfg(test)]
mod outgoing_tests {
    use super::*;
    use crate::native_evidence::{Coverage, Declaration, DocumentKey, Provenance};
    use crate::store::mcp_read::{McpDeclaration, McpOutgoing, McpOutgoingCall};

    fn owner() -> McpDeclaration {
        let document = DocumentKey {
            source_set_id: "source-set:v1:test".into(),
            language: "javascript".into(),
            path: "a.js".into(),
        };
        let declaration: Declaration = serde_json::from_value(json!({
            "syntaxId":format!("sid:v1:{}","a".repeat(32)),"document":document,
            "revisionId":"revision:v2:test","kind":"function","name":"Owner",
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

    #[test]
    fn outgoing_empty_complete_vs_incomplete_coverage_and_unresolved_boundary() {
        let root = std::path::Path::new("/workspace");
        let incomplete = outgoing(
            &json!(1),
            McpOutgoing {
                owner: owner(),
                calls: vec![],
                has_more: false,
            },
            root,
            false,
            true,
        )
        .unwrap();
        assert_eq!(incomplete["data"]["calls"], json!([]));
        assert_eq!(incomplete["warnings"][0]["code"], "coverageIncomplete");
        assert_eq!(incomplete["partial"], true);
        assert_eq!(incomplete["truncated"], false);
        let mut complete = owner();
        complete.coverage[0].state = "complete".into();
        complete.coverage[0].diagnostic = None;
        let complete = outgoing(
            &json!(1),
            McpOutgoing {
                owner: complete,
                calls: vec![],
                has_more: false,
            },
            root,
            false,
            true,
        )
        .unwrap();
        assert_eq!(complete["warnings"][0]["code"], "syntaxOnly");
        assert_eq!(complete["partial"], false);
        let call = serde_json::from_value(
            json!({"id":"cid:v1:1","ownerSyntaxId":owner().declaration.syntax_id,
            "ordinal":0,"document":owner().declaration.document,"revisionId":"revision:v2:test",
            "range":{"start":21,"end":30},"calleeRange":{"start":21,"end":25},
            "spelling":"Target","regionIds":[],"provenanceId":"native-proof:v1:test"}),
        )
        .unwrap();
        let data = McpOutgoingCall {
            call,
            content_hash: "a".repeat(64),
            callee_text: Some("é".repeat(512)),
            callee_text_truncated: true,
            candidates: vec![],
            omitted: Some(0),
        };
        let clipped = outgoing(
            &json!(1),
            McpOutgoing {
                owner: owner(),
                calls: vec![data],
                has_more: false,
            },
            root,
            false,
            true,
        )
        .unwrap();
        assert_eq!(clipped["truncationReason"], "callee_text");
        assert_eq!(clipped["partial"], true);
        assert_eq!(
            clipped["data"]["calls"][0]["calleeText"]
                .as_str()
                .unwrap()
                .len(),
            1024
        );
        assert_eq!(clipped["data"]["calls"][0]["targetId"], Value::Null);
    }
    fn required_call(clipped: bool) -> McpOutgoingCall {
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

    #[test]
    fn fixed_closure_and_first_required_call_never_become_empty_success() {
        let root = std::path::Path::new("/workspace");
        for modern in [true, false] {
            let fits_empty = |diagnostic_size| {
                let mut selected = owner();
                selected.coverage[0].diagnostic = Some("x".repeat(diagnostic_size));
                outgoing(
                    &json!(1),
                    McpOutgoing {
                        owner: selected,
                        calls: vec![],
                        has_more: false,
                    },
                    root,
                    false,
                    modern,
                )
                .is_some()
            };
            let mut low = 0;
            let mut high = super::super::wire::RESPONSE_BYTES;
            while low + 1 < high {
                let middle = low + (high - low) / 2;
                if fits_empty(middle) {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            assert!(fits_empty(low));
            assert!(
                !fits_empty(high),
                "fixed selected coverage must not be clipped"
            );
            let mut selected = owner();
            selected.coverage[0].diagnostic = Some("x".repeat(low));
            assert!(
                outgoing(
                    &json!(1),
                    McpOutgoing {
                        owner: selected,
                        calls: vec![required_call(false)],
                        has_more: false,
                    },
                    root,
                    false,
                    modern
                )
                .is_none(),
                "a required call must not be replaced by an empty success"
            );
        }
    }

    #[test]
    fn outgoing_truncation_prioritizes_bytes_over_limit_over_callee_text() {
        let root = std::path::Path::new("/workspace");
        for modern in [true, false] {
            let limited = outgoing(
                &json!(1),
                McpOutgoing {
                    owner: owner(),
                    calls: vec![required_call(true)],
                    has_more: true,
                },
                root,
                false,
                modern,
            )
            .unwrap();
            assert_eq!(limited["truncationReason"], "limit");
            let mut calls = Vec::new();
            for ordinal in 0..50 {
                let mut item = required_call(true);
                item.call.id = format!("cid:v1:{ordinal}");
                item.call.ordinal = ordinal;
                calls.push(item);
            }
            let bytes = outgoing(
                &json!(1),
                McpOutgoing {
                    owner: owner(),
                    calls,
                    has_more: true,
                },
                root,
                false,
                modern,
            )
            .unwrap();
            let emitted = bytes["data"]["calls"].as_array().unwrap();
            assert!(!emitted.is_empty() && emitted.len() < 50);
            assert_eq!(bytes["truncationReason"], "response_bytes");
            assert!(bytes["truncated"].as_bool().unwrap());
            assert!(bytes["partial"].as_bool().unwrap());
            let frame = json!({"jsonrpc":"2.0","id":1,
                "result":super::super::tools::result(bytes,modern)});
            assert!(serde_json::to_vec(&frame).unwrap().len() < super::super::wire::RESPONSE_BYTES);
        }
    }
}
