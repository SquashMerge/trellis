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
        let display = format!("{}#{}", d.document.path, segments.join("."));
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
