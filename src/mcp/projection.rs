//! Closed W1 wire projection; no Store admission occurs in this layer.
use serde_json::{Value, json};

/// Build refs from precisely the items that survive the wire byte budget.
type DocumentKey = (u8, String, String, String);
fn document_key(version: &crate::store::mcp_read::McpDocumentVersion) -> DocumentKey {
    let language = match version.document.language.as_str() {
        "java" => 0,
        "rust" => 1,
        "python" => 2,
        _ => 3,
    };
    (
        language,
        version.document.source_set_id.clone(),
        version.document.path.clone(),
        version.revision_id.clone(),
    )
}
fn document_table<'a>(
    versions: impl IntoIterator<Item = &'a crate::store::mcp_read::McpDocumentVersion>,
) -> Result<(Vec<Value>, std::collections::BTreeMap<DocumentKey, usize>), &'static str> {
    let mut unique = std::collections::BTreeMap::new();
    for version in versions {
        let key = document_key(version);
        if let Some(existing) = unique.insert(key.clone(), version)
            && existing != version
        {
            return Err("store_unavailable: conflicting document versions");
        }
    }
    let mut refs = std::collections::BTreeMap::new();
    let mut documents = Vec::new();
    for (key, version) in unique {
        let index = documents.len();
        refs.insert(key, index);
        documents.push(json!({"ref":index,"document":version.document,
            "revisionId":version.revision_id,"contentHash":version.content_hash,
            "provenanceId":version.provenance_id}));
    }
    Ok((documents, refs))
}
fn document_ref(
    refs: &std::collections::BTreeMap<DocumentKey, usize>,
    version: &crate::store::mcp_read::McpDocumentVersion,
) -> Option<usize> {
    refs.get(&document_key(version)).copied()
}

/// The four `LanguageStatus` rows, in #22 language order, from per-language
/// tuple counts [selected, complete, partial, failed, unselected].
fn language_status(rows: &[[u64; 5]; 4]) -> Vec<Value> {
    ["java", "rust", "python", "javascript"].into_iter().zip(rows).map(|(language, row)| {
        json!({"language":language,"evidenceTier":"syntax","coverage":{
            "selected":row[0],"complete":row[1],"partial":row[2],"failed":row[3],"unselected":row[4]}})
    }).collect()
}

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
    let languages: Vec<Value> = counts.map_or_else(Vec::new, |rows| language_status(&rows));
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
    result: crate::store::mcp_read::McpDeclarationWithLines,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Option<Value> {
    let source = &result.source;
    let result = &result.selected;
    let (documents, refs) = document_table([&source.document]).ok()?;
    let mut compact = serde_json::to_value(&result.declaration).ok()?;
    let fields = compact.as_object_mut()?;
    for field in ["document", "revisionId", "provenanceId"] {
        fields.remove(field);
    }
    let doc = document_ref(&refs, &source.document)?;
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
        "data":{"displayKey":display_key(&result.declaration),"declaration":compact,"doc":doc,"lines":source.lines.map(|lines| json!({"start":lines.start,"end":lines.end})),"documents":documents,
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
    result: crate::store::mcp_read::McpFindWithLines,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Result<Option<Value>, &'static str> {
    // #156: completeness is a property of the whole searched scope, carried by
    // `scopeCoverage`; per-document coverage covers only the emitted symbols.
    let incomplete = result.scope.iter().any(|row| row[2] != 0 || row[3] != 0);
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
    let scope = language_status(&result.scope);
    let basis = json!({"indexGeneration":result.pin.index_generation.to_string(),
        "indexRevision":result.pin.index_revision});
    let language_order = |language: &str| match language {
        "java" => 0,
        "rust" => 1,
        "python" => 2,
        _ => 3,
    };
    let mut last = None;
    // The empty prefix carries only the fixed envelope and the scope rows; if
    // even it cannot fit, the answer is range_too_large.
    for n in 0..=result.symbols.len() {
        let (documents, refs) = document_table(
            result.symbols[..n]
                .iter()
                .map(|symbol| &symbol.source.document),
        )?;
        let symbols: Vec<Value> = result.symbols[..n].iter().map(|symbol| {
            let d = &symbol.symbol.declaration;
            Ok(json!({"symbolId":d.syntax_id,"name":d.name,"displayKey":display_key(d),
                "kind":d.kind,"doc":document_ref(&refs, &symbol.source.document).ok_or("store_unavailable: missing document ref")?,
                "range":d.range,"nameRange":d.name_range,
                "lines":symbol.source.lines.map(|lines| json!({"start":lines.start,"end":lines.end})),
                "evidenceTier":"syntax","semanticBasis":null,"freshness":"fresh","staleBecause":[]}))
        }).collect::<Result<Vec<_>, &'static str>>()?;
        let emitted = &result.symbols[..n];
        let mut closure: Vec<&crate::native_evidence::Provenance> = Vec::new();
        for symbol in emitted {
            let id = &symbol.symbol.declaration.provenance_id;
            if !closure.iter().any(|p| &p.id == id) {
                closure.push(
                    result
                        .provenance
                        .iter()
                        .find(|p| &p.id == id)
                        .ok_or("store_unavailable: missing proof")?,
                );
            }
        }
        closure.sort_by(|a, b| a.id.cmp(&b.id));
        let mut coverage: Vec<&crate::native_evidence::Coverage> = Vec::new();
        for symbol in emitted {
            let c = &symbol.symbol.coverage;
            if !coverage.iter().any(|seen| {
                (
                    &seen.producer_id,
                    &seen.language,
                    &seen.source_set_id,
                    &seen.document_path,
                    &seen.revision_id,
                ) == (
                    &c.producer_id,
                    &c.language,
                    &c.source_set_id,
                    &c.document_path,
                    &c.revision_id,
                )
            }) {
                coverage.push(c);
            }
        }
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
        let truncated = n < result.symbols.len() || result.has_more;
        let envelope = json!({"schemaVersion":1,"requestId":id,"workspace":root,
            "catchingUp":catching_up,"evidenceBasis":basis,
            "data":{"symbols":symbols,"documents":documents,"coverage":coverage,"scopeCoverage":scope,"provenance":closure},
            "warnings":warnings,"partial":incomplete || truncated,"truncated":truncated,
            "truncationReason": if n < result.symbols.len() {Some("response_bytes")}
                else if result.has_more {Some("limit")} else {None}});
        let response = super::tools::result(envelope.clone(), modern);
        let frame = json!({"jsonrpc":"2.0","id":id,"result":response});
        if serde_json::to_vec(&frame)
            .map_err(|_| "store_unavailable: serialization")?
            .len()
            < super::wire::RESPONSE_BYTES
        {
            last = Some(envelope);
        } else {
            break;
        }
    }
    Ok(last)
}

#[cfg(test)]
mod find_tests {
    use super::*;
    use crate::mcp::wire::{RESPONSE_BYTES, write_response};
    use crate::native_evidence::{Coverage, Declaration, DocumentKey, Provenance};
    use crate::store::mcp_read::{
        McpDocumentVersion, McpFindSymbol, McpFindSymbolWithLines, McpFindWithLines, McpItemSource,
        McpLines,
    };

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
    fn fixture(
        hits: &[(String, &str, String)],
        scope: [[u64; 5]; 4],
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
                "revisionId":"revision:v1:test", "kind":"function", "name":name,
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
    fn hit(name: impl Into<String>, path: &str) -> (String, &'static str, String) {
        (name.into(), "source-set:v1:test", path.into())
    }
    /// The #156 smoke checkout: 270 documents, 111 partial Rust files.
    const LARGE_SCOPE: [[u64; 5]; 4] = [
        [4, 4, 0, 0, 0],
        [117, 6, 111, 0, 0],
        [9, 9, 0, 0, 0],
        [140, 140, 0, 0, 0],
    ];
    const COMPLETE_SCOPE: [[u64; 5]; 4] = [[1, 1, 0, 0, 0], [0; 5], [0; 5], [3, 3, 0, 0, 0]];

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
    fn zero_hits_report_scope_counts_without_document_rows() {
        let root = std::path::Path::new("/workspace");
        let id = json!(7);
        for modern in [false, true] {
            // The scope that overflowed the cap with per-document rows now fits.
            let large = find(&id, fixture(&[], LARGE_SCOPE, false), root, false, modern)
                .unwrap()
                .unwrap();
            let data = &large["data"];
            assert_eq!(data["symbols"], json!([]));
            assert_eq!(data["documents"], json!([]));
            assert_eq!(data["coverage"], json!([]));
            assert_eq!(data["provenance"], json!([]));
            assert_eq!(
                data["scopeCoverage"],
                json!([
                    {"language":"java","evidenceTier":"syntax","coverage":{"selected":4,"complete":4,"partial":0,"failed":0,"unselected":0}},
                    {"language":"rust","evidenceTier":"syntax","coverage":{"selected":117,"complete":6,"partial":111,"failed":0,"unselected":0}},
                    {"language":"python","evidenceTier":"syntax","coverage":{"selected":9,"complete":9,"partial":0,"failed":0,"unselected":0}},
                    {"language":"javascript","evidenceTier":"syntax","coverage":{"selected":140,"complete":140,"partial":0,"failed":0,"unselected":0}}
                ])
            );
            // Incomplete selected scope: partial, one aggregate warning first.
            assert_eq!(large["partial"], true);
            assert_eq!(large["truncated"], false);
            assert_eq!(large["warnings"][0]["code"], "coverageIncomplete");
            assert_eq!(large["warnings"][1]["code"], "syntaxOnly");
            assert_wire(large, &id, modern);

            // A failed selected tuple alone also flags the scope.
            let failed = [[0; 5], [0; 5], [1, 0, 0, 1, 0], [0; 5]];
            let envelope = find(&id, fixture(&[], failed, false), root, false, modern)
                .unwrap()
                .unwrap();
            assert_eq!(envelope["partial"], true);
            assert_eq!(envelope["warnings"][0]["code"], "coverageIncomplete");

            // Unselected-only tuples (and zero-language rows) flag nothing.
            let unselected = [[0; 5], [0, 0, 0, 0, 3], [0; 5], [2, 2, 0, 0, 0]];
            let envelope = find(&id, fixture(&[], unselected, false), root, false, modern)
                .unwrap()
                .unwrap();
            assert_eq!(envelope["partial"], false);
            assert_eq!(envelope["warnings"].as_array().unwrap().len(), 1);
            assert_eq!(envelope["warnings"][0]["code"], "syntaxOnly");
            assert_eq!(
                envelope["data"]["scopeCoverage"].as_array().unwrap().len(),
                4
            );
        }
    }

    #[test]
    fn coverage_is_the_closure_of_emitted_symbols_by_full_document_key() {
        let root = std::path::Path::new("/workspace");
        let id = json!(1);
        let hits = vec![
            hit("one", "src/a.js"),
            hit("two", "src/a.js"),
            ("three".into(), "source-set:v1:other", "src/a.js".into()),
            hit("four", "src/b.js"),
        ];
        for modern in [false, true] {
            let envelope = find(
                &id,
                fixture(&hits, COMPLETE_SCOPE, false),
                root,
                false,
                modern,
            )
            .unwrap()
            .unwrap();
            let rows = envelope["data"]["coverage"].as_array().unwrap();
            // Same path in two source sets is two documents; a repeated document is one row.
            let keys: Vec<_> = rows
                .iter()
                .map(|c| {
                    (
                        c["sourceSetId"].as_str().unwrap(),
                        c["documentPath"].as_str().unwrap(),
                    )
                })
                .collect();
            assert_eq!(
                keys,
                vec![
                    ("source-set:v1:other", "src/a.js"),
                    ("source-set:v1:test", "src/a.js"),
                    ("source-set:v1:test", "src/b.js"),
                ]
            );
            assert_eq!(envelope["data"]["provenance"].as_array().unwrap().len(), 3);
            let repeated = find(
                &id,
                fixture(&hits, COMPLETE_SCOPE, false),
                root,
                false,
                modern,
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                serde_json::to_vec(&envelope).unwrap(),
                serde_json::to_vec(&repeated).unwrap()
            );
            let documents = envelope["data"]["documents"].as_array().unwrap();
            assert_eq!(documents.len(), 3);
            let refs: Vec<_> = envelope["data"]["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .map(|symbol| symbol["doc"].as_u64().unwrap())
                .collect();
            assert_eq!(refs, vec![1, 1, 0, 2]);
            for (index, document) in documents.iter().enumerate() {
                assert_eq!(document["ref"], index);
                assert!(refs.contains(&(index as u64)));
            }
            assert_eq!(
                envelope["data"]["symbols"][0]["lines"],
                json!({"start":1,"end":1})
            );
            assert_eq!(envelope["partial"], false);
            assert_wire(envelope, &id, modern);
        }
    }

    #[test]
    fn document_refs_order_revisions_and_full_keys() {
        let version = |source_set: &str, revision: &str| McpDocumentVersion {
            document: DocumentKey {
                source_set_id: source_set.into(),
                language: "javascript".into(),
                path: "a.js".into(),
            },
            revision_id: revision.into(),
            content_hash: "a".repeat(64),
            provenance_id: "proof".into(),
        };
        let other = version("other", "rev:1");
        let later = version("same", "rev:2");
        let earlier = version("same", "rev:1");
        let (documents, refs) = document_table([&later, &other, &earlier, &other]).unwrap();
        assert_eq!(documents.len(), 3);
        assert_eq!(document_ref(&refs, &other), Some(0));
        assert_eq!(document_ref(&refs, &earlier), Some(1));
        assert_eq!(document_ref(&refs, &later), Some(2));
        assert!(document_table(std::iter::empty()).unwrap().0.is_empty());
    }

    #[test]
    fn conflicting_selected_document_version_is_store_unavailable() {
        for differing_proof in [false, true] {
            let mut conflicting = fixture(
                &[hit("one", "a.js"), hit("two", "a.js")],
                COMPLETE_SCOPE,
                false,
            );
            if differing_proof {
                conflicting.symbols[1].source.document.provenance_id = "other-proof".into();
            } else {
                conflicting.symbols[1].source.document.content_hash = "b".repeat(64);
            }
            let failure = find(
                &json!(1),
                conflicting,
                std::path::Path::new("/workspace"),
                false,
                true,
            )
            .unwrap_err();
            assert!(failure.starts_with("store_unavailable"));
        }
    }

    #[test]
    fn find_keeps_longest_fitting_hit_prefix_and_only_its_closure() {
        let root = std::path::Path::new("/workspace");
        let id = json!("id\n\"\\");
        // Each name appears in a symbol and its displayKey, then again in text.
        // Two fit; the third would overrun the complete JSON-RPC frame.
        let hits = vec![
            hit("A".repeat(6_000), "src/0.js"),
            hit("B".repeat(6_000), "src/1.js"),
            hit("C".repeat(6_000), "src/2.js"),
        ];
        for modern in [false, true] {
            let envelope = find(
                &id,
                fixture(&hits, COMPLETE_SCOPE, false),
                root,
                true,
                modern,
            )
            .unwrap()
            .unwrap();
            assert_wire(envelope.clone(), &id, modern);
            let data = &envelope["data"];
            assert_eq!(data["symbols"].as_array().unwrap().len(), 2);
            assert_eq!(data["symbols"][0]["symbolId"], "sid:0");
            assert_eq!(data["symbols"][1]["symbolId"], "sid:1");
            let paths: Vec<_> = data["coverage"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["documentPath"].clone())
                .collect();
            assert_eq!(paths, vec![json!("src/0.js"), json!("src/1.js")]);
            assert_eq!(data["provenance"].as_array().unwrap().len(), 2);
            let documents = data["documents"].as_array().unwrap();
            assert_eq!(documents.len(), 2);
            for (index, document) in documents.iter().enumerate() {
                assert_eq!(document["ref"], index);
                assert_eq!(document["document"]["path"], format!("src/{index}.js"));
                assert!(
                    data["symbols"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|symbol| symbol["doc"] == index)
                );
            }
            assert_eq!(data["scopeCoverage"].as_array().unwrap().len(), 4);
            assert_eq!(envelope["catchingUp"], true);
            assert_eq!(envelope["truncated"], true);
            assert_eq!(envelope["truncationReason"], "response_bytes");
            assert_eq!(envelope["partial"], true);

            let two = find(
                &id,
                fixture(&hits[..2], COMPLETE_SCOPE, false),
                root,
                true,
                modern,
            )
            .unwrap()
            .unwrap();
            assert_eq!(two["data"]["symbols"], data["symbols"]);
            assert_eq!(two["truncated"], false);
            assert_wire(two, &id, modern);
        }
    }

    #[test]
    fn a_first_hit_too_large_returns_an_empty_truncated_prefix() {
        let root = std::path::Path::new("/workspace");
        let id = json!(3);
        for modern in [false, true] {
            let hits = vec![hit("Z".repeat(40_000), "src/z.js")];
            let envelope = find(
                &id,
                fixture(&hits, COMPLETE_SCOPE, false),
                root,
                false,
                modern,
            )
            .unwrap()
            .unwrap();
            assert_eq!(envelope["data"]["symbols"], json!([]));
            assert_eq!(envelope["data"]["documents"], json!([]));
            assert_eq!(envelope["data"]["coverage"], json!([]));
            assert_eq!(envelope["data"]["provenance"], json!([]));
            assert_eq!(envelope["truncated"], true);
            assert_eq!(envelope["truncationReason"], "response_bytes");
            assert_eq!(envelope["partial"], true);
            assert_wire(envelope, &id, modern);
        }
    }

    #[test]
    fn all_hits_fit_with_limit_truncation_and_full_proof_closure() {
        let hits = vec![hit("first", "src/0.js"), hit("second", "src/1.js")];
        let id = json!(1);
        for modern in [false, true] {
            let envelope = find(
                &id,
                fixture(&hits, COMPLETE_SCOPE, true),
                std::path::Path::new("/workspace"),
                false,
                modern,
            )
            .unwrap()
            .unwrap();
            assert_eq!(envelope["data"]["symbols"].as_array().unwrap().len(), 2);
            assert_eq!(envelope["data"]["provenance"].as_array().unwrap().len(), 2);
            assert_eq!(envelope["data"]["coverage"].as_array().unwrap().len(), 2);
            assert_eq!(envelope["truncationReason"], "limit");
            assert_eq!(envelope["truncated"], true);
            assert_wire(envelope, &id, modern);
        }
    }
}

/// Calls have priority over optional hints in the complete, dual-content wire budget.
pub fn outgoing(
    id: &Value,
    result: crate::store::mcp_read::McpOutgoingWithLines,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Result<Option<Value>, &'static str> {
    use std::collections::BTreeMap;
    let owner_display = display_key(&result.owner.selected.declaration);
    let basis = json!({"indexGeneration":result.owner.selected.pin.index_generation.to_string(),
        "indexRevision":result.owner.selected.pin.index_revision});
    let fits = |envelope: &Value| -> bool {
        let frame = json!({"jsonrpc":"2.0","id":id,
            "result":super::tools::result(envelope.clone(), modern)});
        serde_json::to_vec(&frame).is_ok_and(|bytes| bytes.len() < super::wire::RESPONSE_BYTES)
    };
    let build = |count: usize, shown: &[usize]| -> Result<Value, &'static str> {
        let versions = result.calls[..count]
            .iter()
            .enumerate()
            .flat_map(|(index, item)| {
                std::iter::once(&item.source.document)
                    .chain(item.candidate_documents[..shown[index]].iter())
            });
        let (documents, refs) = document_table(versions)?;
        let mut coverage = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        for row in &result.owner.selected.coverage {
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
        for row in &result.owner.selected.provenance {
            provenance.insert(row.id.clone(), row.clone());
        }
        let mut calls = Vec::new();
        let mut byte_omission = false;
        let mut limited = result.has_more;
        let mut clipped = false;
        for (index, item) in result.calls[..count].iter().enumerate() {
            let text = item.selected.callee_text.as_deref();
            clipped |= item.selected.callee_text_truncated;
            let available = item.selected.candidates.len().min(8);
            let n = shown[index];
            byte_omission |= n < available;
            limited |= item.selected.candidates.len() > 8;
            for (candidate, version) in item.selected.candidates[..n]
                .iter()
                .zip(&item.candidate_documents[..n])
            {
                if candidate.declaration.document != version.document
                    || candidate.declaration.revision_id != version.revision_id
                    || candidate.declaration.provenance_id != version.provenance_id
                    || candidate.provenance.id != version.provenance_id
                    || candidate.provenance.content_hash != version.content_hash
                {
                    return Err("store_unavailable: candidate proof mismatch");
                }
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
            let suggestions: Vec<Value> = item.selected.candidates[..n].iter()
                .zip(&item.candidate_documents[..n]).map(|(candidate, version)| {
                    let d = &candidate.declaration;
                    Ok(json!({"syntaxId":d.syntax_id,"name":d.name,"doc":document_ref(&refs, version).ok_or("store_unavailable: missing candidate ref")?,
                        "range":d.range}))
                }).collect::<Result<Vec<_>, &'static str>>()?;
            let call = &item.selected.call;
            let label = match text {
                Some(text) => format!("{owner_display}@byte:{}:{text}", call.range.start),
                None => format!("{owner_display}@byte:{}", call.range.start),
            };
            calls.push(json!({"callId":call.id,"ownerSyntaxId":call.owner_syntax_id,
                "ordinal":call.ordinal,"doc":document_ref(&refs, &item.source.document).ok_or("store_unavailable: missing call ref")?,
                "lines":item.source.lines.map(|lines| json!({"start":lines.start,"end":lines.end})),
                "range":call.range,"calleeRange":call.callee_range,
                "calleeText":text,"calleeTextTruncated":item.selected.callee_text_truncated,
                "displayKey":label,"nativeCandidates":{"items":suggestions,
                    "omitted":item.selected.omitted.map(|_| item.selected.candidates.len().saturating_sub(n)),
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
        Ok(
            json!({"schemaVersion":1,"requestId":id,"workspace":root,"catchingUp":catching_up,
            "evidenceBasis":basis,"data":{"symbolId":result.owner.selected.declaration.syntax_id,
                "displayKey":owner_display,"calls":calls,"documents":documents,"coverage":coverage,
                "provenance":provenance.into_values().collect::<Vec<_>>()},
            "warnings":warnings,"partial":incomplete || truncated || count > 0,
            "truncated":truncated,"truncationReason":reason}),
        )
    };
    let mut count = 0;
    let mut shown = Vec::new();
    let empty = build(0, &[])?;
    if !fits(&empty) {
        return Ok(None);
    }
    while count < result.calls.len() {
        shown.push(0);
        if !fits(&build(count + 1, &shown)?) {
            shown.pop();
            break;
        }
        count += 1;
    }
    if count == 0 && !result.calls.is_empty() {
        return Ok(None);
    }
    let mut last = build(count, &shown)?;
    'candidate: for (index, item) in result.calls[..count].iter().enumerate() {
        for _ in 0..item.selected.candidates.len().min(8) {
            shown[index] += 1;
            let next = build(count, &shown)?;
            if !fits(&next) {
                shown[index] -= 1;
                break 'candidate;
            }
            last = next;
        }
    }
    Ok(Some(last))
}

#[cfg(test)]
mod outgoing_tests {
    use super::*;
    use crate::native_evidence::{Coverage, Declaration, DocumentKey, Provenance};
    use crate::store::mcp_read::{
        McpDeclaration, McpDeclarationWithLines, McpDocumentVersion, McpItemSource, McpLines,
        McpOutgoing, McpOutgoingCall, McpOutgoingCallWithLines, McpOutgoingCandidate,
        McpOutgoingWithLines,
    };

    fn with_lines(result: McpOutgoing) -> McpOutgoingWithLines {
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
                    document: owner_doc,
                    lines: Some(McpLines { start: 1, end: 1 }),
                },
            },
            calls,
            has_more: result.has_more,
        }
    }

    fn outgoing_fixture(
        id: &Value,
        result: McpOutgoing,
        root: &std::path::Path,
        catching_up: bool,
        modern: bool,
    ) -> Option<Value> {
        outgoing(id, with_lines(result), root, catching_up, modern).unwrap()
    }
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
    fn shown_candidate_uses_its_own_proof_not_another_valid_proof() {
        let make_result = || {
            let mut target = owner();
            target.declaration.syntax_id = "sid:v1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
            target.declaration.document.path = "target.js".into();
            target.declaration.provenance_id = "native-proof:v1:target".into();
            target.coverage[0].document_path = "target.js".into();
            target.provenance[0].document = target.declaration.document.clone();
            target.provenance[0].id = target.declaration.provenance_id.clone();
            let mut call = required_call(false);
            call.candidates.push(McpOutgoingCandidate {
                declaration: target.declaration,
                coverage: target.coverage.remove(0),
                provenance: target.provenance.remove(0),
            });
            with_lines(McpOutgoing {
                owner: owner(),
                calls: vec![call],
                has_more: false,
            })
        };
        let root = std::path::Path::new("/workspace");
        let valid = outgoing(&json!(1), make_result(), root, false, true)
            .unwrap()
            .unwrap();
        let entry = &valid["data"]["documents"]
            [valid["data"]["calls"][0]["nativeCandidates"]["items"][0]["doc"]
                .as_u64()
                .unwrap() as usize];
        assert_eq!(entry["provenanceId"], "native-proof:v1:target");
        assert_ne!(
            entry["provenanceId"],
            valid["data"]["documents"][0]["provenanceId"]
        );
        let mut wrong = make_result();
        // The owner proof is real and present in the response, but it is not
        // the independent target declaration's proof.
        wrong.calls[0].candidate_documents[0].provenance_id = owner().provenance[0].id.clone();
        let failure = outgoing(&json!(1), wrong, root, false, true).unwrap_err();
        assert!(failure.starts_with("store_unavailable"));
    }

    #[test]
    fn outgoing_empty_complete_vs_incomplete_coverage_and_unresolved_boundary() {
        let root = std::path::Path::new("/workspace");
        let incomplete = outgoing_fixture(
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
        assert_eq!(incomplete["data"]["documents"], json!([]));
        assert_eq!(incomplete["data"]["coverage"].as_array().unwrap().len(), 1);
        assert_eq!(
            incomplete["data"]["provenance"].as_array().unwrap().len(),
            1
        );
        assert_eq!(incomplete["warnings"][0]["code"], "coverageIncomplete");
        assert_eq!(incomplete["partial"], true);
        assert_eq!(incomplete["truncated"], false);
        let mut complete = owner();
        complete.coverage[0].state = "complete".into();
        complete.coverage[0].diagnostic = None;
        let complete = outgoing_fixture(
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
        let clipped = outgoing_fixture(
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
                outgoing_fixture(
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
                outgoing_fixture(
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
            let limited = outgoing_fixture(
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
            // A hint attached only to the excluded last call must never leak
            // a table entry, coverage row, or proof into the selected prefix.
            let mut candidate = owner();
            candidate.declaration.document.path = "excluded.js".into();
            candidate.declaration.revision_id = "revision:v2:excluded".into();
            candidate.declaration.provenance_id = "native-proof:v1:excluded".into();
            candidate.coverage[0].document_path = "excluded.js".into();
            candidate.coverage[0].revision_id = "revision:v2:excluded".into();
            candidate.provenance[0].document = candidate.declaration.document.clone();
            candidate.provenance[0].revision_id = "revision:v2:excluded".into();
            candidate.provenance[0].id = "native-proof:v1:excluded".into();
            calls[49].candidates.push(McpOutgoingCandidate {
                declaration: candidate.declaration,
                coverage: candidate.coverage.remove(0),
                provenance: candidate.provenance.remove(0),
            });
            let answer = || {
                outgoing_fixture(
                    &json!(1),
                    McpOutgoing {
                        owner: owner(),
                        calls: (0..50)
                            .map(|ordinal| {
                                let mut item = required_call(true);
                                item.call.id = format!("cid:v1:{ordinal}");
                                item.call.ordinal = ordinal;
                                if ordinal == 49 {
                                    item.candidates = calls[49].candidates.clone();
                                }
                                item
                            })
                            .collect(),
                        has_more: true,
                    },
                    root,
                    false,
                    modern,
                )
                .unwrap()
            };
            let bytes = answer();
            assert_eq!(
                serde_json::to_vec(&bytes).unwrap(),
                serde_json::to_vec(&answer()).unwrap()
            );
            let emitted = bytes["data"]["calls"].as_array().unwrap();
            assert!(!emitted.is_empty() && emitted.len() < 50);
            assert_eq!(
                emitted
                    .iter()
                    .map(|call| call["ordinal"].as_u64().unwrap())
                    .collect::<Vec<_>>(),
                (0..emitted.len() as u64).collect::<Vec<_>>()
            );
            assert_eq!(bytes["data"]["documents"].as_array().unwrap().len(), 1);
            assert_eq!(bytes["data"]["coverage"].as_array().unwrap().len(), 1);
            assert_eq!(bytes["data"]["provenance"].as_array().unwrap().len(), 1);
            assert!(emitted.iter().all(|call| call["doc"] == 0));
            assert_eq!(bytes["data"]["documents"][0]["document"]["path"], "a.js");
            assert_eq!(bytes["truncationReason"], "response_bytes");
            let required_prefix = |count: usize| {
                outgoing_fixture(
                    &json!(1),
                    McpOutgoing {
                        owner: owner(),
                        calls: (0..count)
                            .map(|ordinal| {
                                let mut call = required_call(true);
                                call.call.id = format!("cid:v1:{ordinal}");
                                call.call.ordinal = ordinal;
                                call
                            })
                            .collect(),
                        has_more: false,
                    },
                    root,
                    false,
                    modern,
                )
                .unwrap()
            };
            let exactly_fits = required_prefix(emitted.len());
            assert_eq!(exactly_fits["data"]["calls"], bytes["data"]["calls"]);
            assert_eq!(exactly_fits["truncationReason"], "callee_text");
            let first_excluded = required_prefix(emitted.len() + 1);
            assert_eq!(first_excluded["data"]["calls"], bytes["data"]["calls"]);
            assert_eq!(first_excluded["truncationReason"], "response_bytes");
            assert!(bytes["truncated"].as_bool().unwrap());
            assert!(bytes["partial"].as_bool().unwrap());
            let frame = json!({"jsonrpc":"2.0","id":1,
                "result":super::super::tools::result(bytes,modern)});
            let mut line = Vec::new();
            super::super::wire::write_response(&mut line, &frame).unwrap();
            assert!(line.len() <= super::super::wire::RESPONSE_BYTES);
            assert_eq!(line.last(), Some(&b'\n'));
        }
    }
}

/// Single complete selected source range; both serialized copies must fit.
pub fn source(
    id: &Value,
    result: crate::store::mcp_read::McpSource,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Option<Value> {
    let envelope = json!({
        "schemaVersion":1,"requestId":id,"workspace":root,"catchingUp":catching_up,
        "evidenceBasis":{"indexGeneration":result.pin.index_generation.to_string(),
            "indexRevision":result.pin.index_revision},
        "data":{"path":result.path,"contentHash":result.content_hash,
            "startLine":result.start_line,"endLine":result.end_line,
            "byteRange":{"start":result.byte_start,"end":result.byte_end},
            "text":result.text,
            "evidence":{"evidenceTier":"syntax","semanticBasis":null,
                "freshness":"fresh","staleBecause":[]}},
        "warnings":[{"code":"syntaxOnly","message":"Only syntax evidence is available",
            "provenanceId":null}],
        "partial":false,"truncated":false,"truncationReason":null,
    });
    let frame = json!({"jsonrpc":"2.0","id":id,
        "result":super::tools::result(envelope.clone(), modern)});
    (serde_json::to_vec(&frame).ok()?.len() < super::wire::RESPONSE_BYTES).then_some(envelope)
}

#[cfg(test)]
mod source_tests {
    use super::*;
    use crate::store::mcp_read::McpSource;
    #[test]
    fn whole_dual_wire_source_or_range_error_without_truncation() {
        let pin: crate::model::IndexPin = serde_json::from_value(json!({
            "indexGeneration":"123e4567-e89b-42d3-a456-426614174000",
            "indexRevision":1}))
        .unwrap();
        let item = |text: String| McpSource {
            path: "a.js".into(),
            content_hash: "a".repeat(64),
            start_line: 1,
            end_line: 1,
            byte_start: 0,
            byte_end: text.len(),
            text,
            pin,
        };
        for modern in [true, false] {
            let envelope = source(
                &json!(1),
                item("hello\r\n".into()),
                std::path::Path::new("/root"),
                false,
                modern,
            )
            .unwrap();
            assert_eq!(envelope["data"]["text"], "hello\r\n");
            assert_eq!(envelope["data"]["byteRange"], json!({"start":0,"end":7}));
            assert_eq!(envelope["partial"], false);
            assert_eq!(envelope["truncated"], false);
            assert_eq!(envelope["warnings"][0]["code"], "syntaxOnly");
            let frame = json!({"jsonrpc":"2.0","id":1,
                "result":super::super::tools::result(envelope,modern)});
            let mut bytes = Vec::new();
            super::super::wire::write_response(&mut bytes, &frame).unwrap();
            assert!(bytes.len() <= super::super::wire::RESPONSE_BYTES);
            assert!(
                source(
                    &json!(1),
                    item("\n".repeat(16384)),
                    std::path::Path::new("/root"),
                    false,
                    modern
                )
                .is_none()
            );
        }
    }
}
