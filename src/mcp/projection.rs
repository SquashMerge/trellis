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

/// Semantic tiers per language in #22 order (#528); the default is syntax only everywhere.
pub type Tiers = [crate::store::LanguageTier; 4];

/// Native tuple counts per language with each language's semantic tier, as describe reads them.
pub type LanguageCounts = ([[u64; 5]; 4], Tiers);

fn counts(row: &[u64; 5]) -> Value {
    json!({"selected":row[0],"complete":row[1],"partial":row[2],"failed":row[3],"unselected":row[4]})
}

/// The four `LanguageStatus` rows, in #22 language order, from per-language native
/// tuple counts [selected, complete, partial, failed, unselected] and each language's
/// selected imported evidence (#528). `evidenceTier` and `coverage` keep describing the
/// native syntax tuples; `semantic` separately reports the selected producer, its
/// freshness and its tuple counts, with `available` true when it is selected, fresh and
/// covers at least one document. Partial coverage of either stays visible.
fn language_status(rows: &[[u64; 5]; 4], tiers: &Tiers) -> Vec<Value> {
    ["java", "rust", "python", "javascript"]
        .into_iter()
        .zip(rows)
        .zip(tiers)
        .map(|((language, row), tier)| {
            json!({"language":language,"evidenceTier":"syntax","coverage":counts(row),
            "semantic":{"available":tier.semantic(),"producerId":tier.producer_id,
                "freshness":if tier.freshness.is_empty() {"unavailable"} else {tier.freshness},
                "coverage":counts(&tier.counts),"note":tier.note}})
        })
        .collect()
}

const LANGUAGES: [&str; 4] = ["java", "rust", "python", "javascript"];

/// The `syntaxOnly` warning for `languages` (#528). It says the answer itself is
/// syntax-derived and names the languages without selected fresh semantic evidence; it is
/// dropped when every one has it, since `scopeCoverage` then reports that evidence. An
/// empty set (nothing to attribute) keeps the plain warning.
fn syntax_only<'a>(tiers: &Tiers, languages: impl IntoIterator<Item = &'a str>) -> Option<Value> {
    let languages: std::collections::BTreeSet<&str> = languages.into_iter().collect();
    if languages.is_empty() {
        return Some(
            json!({"code":"syntaxOnly","message":"This answer is syntax-derived","provenanceId":null}),
        );
    }
    let syntax: Vec<&str> = LANGUAGES
        .into_iter()
        .zip(tiers)
        .filter(|(language, tier)| languages.contains(language) && !tier.semantic())
        .map(|(language, _)| language)
        .collect();
    (!syntax.is_empty()).then(|| {
        json!({"code":"syntaxOnly","message":format!("This answer is syntax-derived; no fresh semantic evidence is selected for {}", syntax.join(", ")),"provenanceId":null})
    })
}

pub fn describe(
    id: &Value,
    label: &str,
    state: &str,
    progress: Option<Value>,
    counts: Option<LanguageCounts>,
    basis: Option<Value>,
    degraded: bool,
) -> Value {
    let partial = counts.as_ref().is_none_or(|(rows, tiers)| {
        rows.iter()
            .chain(tiers.iter().map(|tier| &tier.counts))
            .any(|row| row[2] != 0 || row[3] != 0)
    });
    let languages: Vec<Value> = counts
        .as_ref()
        .map_or_else(Vec::new, |(rows, tiers)| language_status(rows, tiers));
    let deadline_ms = u64::try_from(super::CALL_DEADLINE.as_millis()).unwrap_or(u64::MAX);
    json!({"schemaVersion":1,"requestId":id,"evidenceBasis":basis,
      "data":{"workspaceLabel":label,"indexState":state,"progress":progress,
      "watcherDegraded":degraded,"languages":languages,"toolVersion":3,"schemaVersion":1,
      "searchIndex":crate::search_model::SearchAvailability::default(),
      "limits":{"requestBytes":16384,"responseBytes":65536,"sourceBytes":16384,
        "sourceLines":200,"defaultLimit":20,"callDefaultLimit":super::CALL_DEFAULT_LIMIT,
        "maxLimit":50,"pageBytes":super::architecture::PAGE_BYTES,"calleeTextBytes":1024,
        "hintWorkMs":1000,"deadlineMs":deadline_ms,"concurrency":super::MAX_CONNECTION_CALLS}},
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
            Some((rows, Tiers::default())),
            Some(basis.clone()),
            true,
        );
        assert_eq!(ready["evidenceBasis"], basis);
        assert_eq!(ready["data"]["watcherDegraded"], true);
        assert_eq!(ready["data"]["toolVersion"], 3);
        assert_eq!(
            ready["data"]["languages"][1]["coverage"],
            json!({
            "selected":2,"complete":1,"partial":1,"failed":0,"unselected":3})
        );
        assert_eq!(ready["partial"], true);
        assert_eq!(ready["data"]["languages"].as_array().unwrap().len(), 4);
        // Without imported evidence every language is syntax, with no producer.
        for language in ready["data"]["languages"].as_array().unwrap() {
            assert_eq!(language["evidenceTier"], "syntax");
            assert_eq!(language["semantic"]["available"], false);
            assert_eq!(language["semantic"]["producerId"], Value::Null);
            assert_eq!(language["semantic"]["freshness"], "unavailable");
        }
    }

    /// Rust selects a `rust-analyzer` import; the other languages have no producer.
    pub(super) fn rust_semantic(freshness: &'static str, counts: [u64; 5]) -> Tiers {
        let mut tiers = Tiers::default();
        for (tier, language) in tiers.iter_mut().zip(LANGUAGES) {
            tier.freshness = "unavailable";
            tier.note =
                Some("semantic producer does not cover this language; showing syntax".into());
            if language == "rust" {
                tier.producer_id = Some("rust-analyzer".into());
                tier.note = None;
                tier.freshness = freshness;
                tier.counts = counts;
            }
        }
        tiers
    }

    /// #528: `syntaxOnly` names, in #22 order, only the answer's languages without
    /// available semantic evidence, and is dropped when all of them have it.
    #[test]
    fn syntax_only_names_only_languages_without_available_semantic_evidence() {
        let tiers = rust_semantic("fresh", [3, 3, 0, 0, 0]);
        assert_eq!(syntax_only(&tiers, ["rust"]), None);
        assert_eq!(
            syntax_only(&tiers, ["rust", "javascript", "java", "rust"]).unwrap()["message"],
            "This answer is syntax-derived; no fresh semantic evidence is selected for java, javascript"
        );
        assert_eq!(
            syntax_only(&tiers, Vec::<&str>::new()).unwrap()["message"],
            "This answer is syntax-derived"
        );
        for (freshness, counts) in [
            ("possiblyStale", [3, 3, 0, 0, 0]),
            ("stale", [3, 3, 0, 0, 0]),
            ("unavailable", [2, 0, 0, 2, 0]),
        ] {
            let warning = syntax_only(&rust_semantic(freshness, counts), ["rust"]).unwrap();
            assert_eq!(warning["code"], "syntaxOnly");
            assert_eq!(warning["provenanceId"], Value::Null);
        }
    }

    /// #528: a fresh selected import reports the semantic tier beside native coverage;
    /// partial semantic coverage stays partial, and stale evidence stays syntax.
    #[test]
    fn describe_reports_the_imported_semantic_tier_per_language() {
        let rows = [[1, 1, 0, 0, 0], [355, 22, 333, 0, 0], [0; 5], [0; 5]];
        let basis =
            json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":1});
        let fresh = describe(
            &json!(1),
            "project",
            "ready",
            None,
            Some((rows, rust_semantic("fresh", [355, 300, 55, 0, 0]))),
            Some(basis.clone()),
            false,
        );
        let rust = &fresh["data"]["languages"][1];
        // The native coverage row keeps its tier; the import is reported beside it.
        assert_eq!(rust["evidenceTier"], "syntax");
        assert_eq!(
            rust["coverage"],
            json!({"selected":355,"complete":22,"partial":333,"failed":0,"unselected":0})
        );
        assert_eq!(
            rust["semantic"],
            json!({"available":true,"producerId":"rust-analyzer","freshness":"fresh","note":null,
                "coverage":{"selected":355,"complete":300,"partial":55,"failed":0,"unselected":0}})
        );
        assert_eq!(
            fresh["data"]["languages"][0]["semantic"]["available"],
            false
        );
        assert_eq!(fresh["partial"], true);
        let stale = describe(
            &json!(2),
            "project",
            "ready",
            None,
            Some((rows, rust_semantic("stale", [355, 355, 0, 0, 0]))),
            Some(basis),
            false,
        );
        assert_eq!(
            stale["data"]["languages"][1]["semantic"]["available"],
            false
        );
        assert_eq!(
            stale["data"]["languages"][1]["semantic"]["freshness"],
            "stale"
        );
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
    tiers: &Tiers,
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
    let mut warnings = Vec::new();
    if incomplete {
        warnings.push(json!({"code":"coverageIncomplete","message":"Selected syntax coverage is incomplete","provenanceId":null}));
    }
    warnings.extend(syntax_only(
        tiers,
        [result.declaration.document.language.as_str()],
    ));
    let basis = json!({"indexGeneration":result.pin.index_generation.to_string(),
        "indexRevision":result.pin.index_revision});
    let envelope = json!({"schemaVersion":1,"requestId":id,"workspace":root,"catchingUp":catching_up,
        "evidenceBasis":basis,
        "data":{"displayKey":display_key(&result.declaration),"declaration":compact,"doc":doc,"lines":source.lines.map(|lines| json!({"start":lines.start,"end":lines.end})),"documents":documents,
            "evidence":{"evidenceTier":"syntax","semanticBasis":null,"freshness":"fresh","staleBecause":[]},
            "coverage":result.coverage,"provenance":result.provenance},
        "warnings":warnings,"partial":incomplete,"truncated":false,"truncationReason":null});
    let frame = json!({"jsonrpc":"2.0","id":id,
        "result":super::tools::result("trellis_declaration", envelope.clone(), modern)});
    (serde_json::to_vec(&frame).ok()?.len() < super::wire::RESPONSE_BYTES).then_some(envelope)
}

/// Budget the actual structured-plus-summary JSON-RPC frame, including LF.
pub fn find(
    id: &Value,
    result: crate::store::mcp_read::McpFindWithLines,
    tiers: &Tiers,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Result<Option<Value>, &'static str> {
    symbols(id, result, tiers, root, catching_up, modern, None)
}

/// Search relevance decorates syntax evidence; it never changes native completeness.
pub fn search(
    id: &Value,
    result: super::search::SearchResult,
    tiers: &Tiers,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Result<Option<Value>, &'static str> {
    let _phase = crate::search_model::SearchPhaseTimer::start("search_projection");
    if result.matched_by.len() != result.symbols.symbols.len()
        || result.matched_by.iter().any(|arms| {
            !matches!(
                arms.iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .as_slice(),
                ["lexical"] | ["vector"] | ["lexical", "vector"]
            )
        })
    {
        return Err("store_unavailable: invalid search arm membership");
    }
    let status = serde_json::to_value(result.search_index)
        .map_err(|_| "store_unavailable: search status serialization")?;
    symbols(
        id,
        result.symbols,
        tiers,
        root,
        catching_up,
        modern,
        Some((&result.matched_by, &status, result.role)),
    )
}

/// Budget the actual structured-plus-summary JSON-RPC frame, not the structured envelope alone.
fn symbols(
    id: &Value,
    result: crate::store::mcp_read::McpFindWithLines,
    tiers: &Tiers,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
    search: Option<(&[Vec<String>], &Value, crate::search_rank::SearchRole)>,
) -> Result<Option<Value>, &'static str> {
    // #156: completeness is a property of the whole searched scope, carried by
    // `scopeCoverage`; per-document coverage covers only the emitted symbols.
    let incomplete = result.scope.iter().any(|row| row[2] != 0 || row[3] != 0);
    // #528: `syntaxOnly` names the emitted symbols' languages without fresh selected
    // semantic evidence; with no symbols, the searched languages that have selected tuples.
    let scope_languages: Vec<&str> = LANGUAGES
        .into_iter()
        .zip(&result.scope)
        .filter(|(_, row)| row[0] != 0)
        .map(|(language, _)| language)
        .collect();
    let warnings_for = |n: usize| -> Vec<Value> {
        let mut warnings = Vec::new();
        if incomplete {
            warnings.push(json!({"code":"coverageIncomplete","message":"Selected syntax coverage is incomplete","provenanceId":null}));
        }
        let languages: Vec<&str> = if n == 0 {
            scope_languages.clone()
        } else {
            result.symbols[..n]
                .iter()
                .map(|symbol| symbol.symbol.declaration.document.language.as_str())
                .collect()
        };
        warnings.extend(syntax_only(tiers, languages));
        warnings
    };
    let scope = language_status(&result.scope, tiers);
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
        let mut symbols: Vec<Value> = result.symbols[..n].iter().map(|symbol| {
            let d = &symbol.symbol.declaration;
            Ok(json!({"symbolId":d.syntax_id,"name":d.name,"displayKey":display_key(d),
                "kind":d.kind,"role":d.role,"doc":document_ref(&refs, &symbol.source.document).ok_or("store_unavailable: missing document ref")?,
                "range":d.range,"nameRange":d.name_range,
                "lines":symbol.source.lines.map(|lines| json!({"start":lines.start,"end":lines.end})),
                "evidenceTier":"syntax","semanticBasis":null,"freshness":"fresh","staleBecause":[]}))
        }).collect::<Result<Vec<_>, &'static str>>()?;
        if let Some((matched_by, _, _)) = search {
            for (index, symbol) in symbols.iter_mut().enumerate() {
                symbol["rank"] = json!(index + 1);
                symbol["matchedBy"] = json!(matched_by[index]);
            }
        }
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
        let mut envelope = json!({"schemaVersion":1,"requestId":id,"workspace":root,
            "catchingUp":catching_up,"evidenceBasis":basis,
            "data":{"symbols":symbols,"documents":documents,"coverage":coverage,"scopeCoverage":scope,"provenance":closure},
            "warnings":warnings_for(n),"partial":incomplete || truncated,"truncated":truncated,
            "truncationReason": if n < result.symbols.len() {Some("response_bytes")}
                else if result.has_more {Some("limit")} else {None}});
        let name = if let Some((_, status, role)) = search {
            let data = envelope["data"].as_object_mut().expect("symbol data");
            let hits = data.remove("symbols").expect("symbol list");
            data.insert("hits".into(), hits);
            data.insert("searchIndex".into(), status.clone());
            data.insert("role".into(), json!(role));
            "trellis_search_symbols"
        } else {
            "trellis_find_symbols"
        };
        let response = super::tools::result(name, envelope.clone(), modern);
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
    pub(super) fn fixture(
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
        let result = super::super::tools::result("trellis_find_symbols", envelope.clone(), modern);
        assert_eq!(result["structuredContent"], envelope);
        let count = envelope["data"]["symbols"].as_array().unwrap().len();
        let truncated = if envelope["truncated"] == true {
            format!(
                ", truncated: {}",
                envelope["truncationReason"].as_str().unwrap()
            )
        } else {
            String::new()
        };
        let items = if count == 1 { "symbol" } else { "symbols" };
        assert_eq!(
            result["content"][0]["text"],
            format!(
                "trellis_find_symbols: {count} {items}{truncated}; full result in structuredContent."
            )
        );
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
            let large = find(
                &id,
                fixture(&[], LARGE_SCOPE, false),
                &Default::default(),
                root,
                false,
                modern,
            )
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
                    {"language":"java","evidenceTier":"syntax","coverage":{"selected":4,"complete":4,"partial":0,"failed":0,"unselected":0},"semantic":{"available":false,"producerId":null,"freshness":"unavailable","coverage":{"selected":0,"complete":0,"partial":0,"failed":0,"unselected":0},"note":null}},
                    {"language":"rust","evidenceTier":"syntax","coverage":{"selected":117,"complete":6,"partial":111,"failed":0,"unselected":0},"semantic":{"available":false,"producerId":null,"freshness":"unavailable","coverage":{"selected":0,"complete":0,"partial":0,"failed":0,"unselected":0},"note":null}},
                    {"language":"python","evidenceTier":"syntax","coverage":{"selected":9,"complete":9,"partial":0,"failed":0,"unselected":0},"semantic":{"available":false,"producerId":null,"freshness":"unavailable","coverage":{"selected":0,"complete":0,"partial":0,"failed":0,"unselected":0},"note":null}},
                    {"language":"javascript","evidenceTier":"syntax","coverage":{"selected":140,"complete":140,"partial":0,"failed":0,"unselected":0},"semantic":{"available":false,"producerId":null,"freshness":"unavailable","coverage":{"selected":0,"complete":0,"partial":0,"failed":0,"unselected":0},"note":null}}
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
            let envelope = find(
                &id,
                fixture(&[], failed, false),
                &Default::default(),
                root,
                false,
                modern,
            )
            .unwrap()
            .unwrap();
            assert_eq!(envelope["partial"], true);
            assert_eq!(envelope["warnings"][0]["code"], "coverageIncomplete");

            // Unselected-only tuples (and zero-language rows) flag nothing.
            let unselected = [[0; 5], [0, 0, 0, 0, 3], [0; 5], [2, 2, 0, 0, 0]];
            let envelope = find(
                &id,
                fixture(&[], unselected, false),
                &Default::default(),
                root,
                false,
                modern,
            )
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
                &Default::default(),
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
                &Default::default(),
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
                &Default::default(),
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
        // Each name appears in a symbol and its displayKey, not in the summary.
        // Two fit; the third would overrun the complete JSON-RPC frame.
        let hits = vec![
            hit("A".repeat(12_000), "src/0.js"),
            hit("B".repeat(12_000), "src/1.js"),
            hit("C".repeat(12_000), "src/2.js"),
        ];
        for modern in [false, true] {
            let envelope = find(
                &id,
                fixture(&hits, COMPLETE_SCOPE, false),
                &Default::default(),
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
                &Default::default(),
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
                &Default::default(),
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
                &Default::default(),
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

#[cfg(test)]
mod search_tests {
    use super::*;
    use crate::mcp::search::SearchResult;
    use crate::search_embed::Backend;
    use crate::search_model::{LexicalState, SearchIndexState, SearchIndexStatus};

    const COMPLETE: [[u64; 5]; 4] = [[0; 5], [0; 5], [0; 5], [3, 3, 0, 0, 0]];

    fn fixture(names: &[(&str, &str)], scope: [[u64; 5]; 4], has_more: bool) -> SearchResult {
        let hits: Vec<_> = names
            .iter()
            .map(|(name, path)| ((*name).to_owned(), "source-set:v1:test", (*path).to_owned()))
            .collect();
        SearchResult {
            role: crate::search_rank::SearchRole::default(),
            symbols: super::find_tests::fixture(&hits, scope, has_more),
            matched_by: names
                .iter()
                .map(|_| vec!["lexical".into(), "vector".into()])
                .collect(),
            search_index: SearchIndexStatus {
                state: SearchIndexState::Partial,
                model_id: Some("a".repeat(64)),
                model_name: Some("voyage-4-nano".into()),
                backend: Some(Backend::Cpu),
                declarations: 3,
                embedded: 1,
                lexical: LexicalState::Complete,
                unavailable_reason: None,
            },
        }
    }

    fn project(result: SearchResult, modern: bool) -> Result<Option<Value>, &'static str> {
        search(
            &json!("search"),
            result,
            &Default::default(),
            std::path::Path::new("/workspace"),
            true,
            modern,
        )
    }

    fn assert_wire(envelope: &Value, modern: bool) {
        let frame = json!({"jsonrpc":"2.0","id":"search","result":
            crate::mcp::tools::result("trellis_search_symbols", envelope.clone(), modern)});
        let mut line = Vec::new();
        crate::mcp::wire::write_response(&mut line, &frame).unwrap();
        assert!(line.len() <= crate::mcp::wire::RESPONSE_BYTES);
        assert_eq!(line.last(), Some(&b'\n'));
        assert_eq!(frame["result"].get("resultType").is_some(), modern);
    }

    #[test]
    fn search_relevance_is_not_native_completeness() {
        for modern in [false, true] {
            let envelope = project(
                fixture(&[("one", "a.js"), ("two", "a.js")], COMPLETE, false),
                modern,
            )
            .unwrap()
            .unwrap();
            let data = &envelope["data"];
            assert_eq!(data.as_object().unwrap().len(), 7);
            assert_eq!(data["role"], "any");
            assert_eq!(data["searchIndex"]["state"], "partial");
            assert_eq!(data["searchIndex"]["embedded"], 1);
            assert_eq!(envelope["partial"], false);
            assert_eq!(envelope["truncated"], false);
            assert_eq!(
                envelope["warnings"],
                json!([{"code":"syntaxOnly","message":"This answer is syntax-derived; no fresh semantic evidence is selected for javascript","provenanceId":null}])
            );
            assert_eq!(data["hits"][0]["rank"], 1);
            assert_eq!(data["hits"][1]["rank"], 2);
            assert_eq!(data["hits"][0]["matchedBy"], json!(["lexical", "vector"]));
            assert_eq!(data["hits"][0]["lines"], json!({"start":1,"end":1}));
            assert_eq!(data["documents"].as_array().unwrap().len(), 1);
            assert_eq!(data["coverage"].as_array().unwrap().len(), 1);
            assert_eq!(data["provenance"].as_array().unwrap().len(), 1);
            assert_wire(&envelope, modern);

            let mut unavailable = fixture(&[], COMPLETE, false);
            unavailable.search_index.state = SearchIndexState::Unavailable;
            unavailable.search_index.model_id = None;
            unavailable.search_index.model_name = None;
            unavailable.search_index.backend = None;
            unavailable.search_index.embedded = 0;
            let empty = project(unavailable, modern).unwrap().unwrap();
            assert_eq!(empty["partial"], false);
            assert_eq!(empty["data"]["searchIndex"]["backend"], Value::Null);
            for field in ["hits", "documents", "coverage", "provenance"] {
                assert_eq!(empty["data"][field], json!([]));
            }
            let incomplete = [[0; 5], [2, 1, 1, 0, 0], [0; 5], [3, 3, 0, 0, 0]];
            let empty = project(fixture(&[], incomplete, false), modern)
                .unwrap()
                .unwrap();
            assert_eq!(empty["partial"], true);
            assert_eq!(empty["warnings"][0]["code"], "coverageIncomplete");
            assert_eq!(empty["warnings"][1]["code"], "syntaxOnly");
            assert_eq!(empty["data"]["scopeCoverage"][1]["coverage"]["partial"], 1);
            assert_wire(&empty, modern);
        }
    }

    #[test]
    fn search_byte_prefix_keeps_only_emitted_closure_and_precedes_limit() {
        let first = "a".repeat(12_000);
        let second = "b".repeat(12_000);
        let third = "c".repeat(12_000);
        for modern in [false, true] {
            let envelope = project(
                fixture(
                    &[(&first, "a.js"), (&second, "b.js"), (&third, "c.js")],
                    COMPLETE,
                    true,
                ),
                modern,
            )
            .unwrap()
            .unwrap();
            assert_eq!(envelope["data"]["hits"].as_array().unwrap().len(), 2);
            assert_eq!(envelope["truncationReason"], "response_bytes");
            assert_eq!(envelope["partial"], true);
            for field in ["documents", "coverage", "provenance"] {
                assert_eq!(envelope["data"][field].as_array().unwrap().len(), 2);
            }
            assert_eq!(envelope["data"]["documents"][1]["document"]["path"], "b.js");
            assert_wire(&envelope, modern);
            let limited = project(fixture(&[("one", "a.js")], COMPLETE, true), modern)
                .unwrap()
                .unwrap();
            assert_eq!(limited["truncationReason"], "limit");
            let oversized = "x".repeat(40_000);
            let empty = project(fixture(&[(&oversized, "big.js")], COMPLETE, false), modern)
                .unwrap()
                .unwrap();
            assert_eq!(empty["truncationReason"], "response_bytes");
            for field in ["hits", "documents", "coverage", "provenance"] {
                assert_eq!(empty["data"][field], json!([]));
            }
            assert_wire(&empty, modern);
        }
    }

    #[test]
    fn search_role_survives_empty_limit_and_byte_trimmed_prefixes() {
        use crate::search_rank::SearchRole;
        let oversized = "x".repeat(40_000);
        for modern in [false, true] {
            for (role, expected) in [
                (SearchRole::Any, "any"),
                (SearchRole::Production, "production"),
                (SearchRole::Test, "test"),
            ] {
                for (names, has_more, reason) in [
                    (vec![], false, Value::Null),
                    (vec![("one", "a.js")], true, json!("limit")),
                    (
                        vec![(oversized.as_str(), "big.js")],
                        false,
                        json!("response_bytes"),
                    ),
                ] {
                    let mut result = fixture(&names, COMPLETE, has_more);
                    result.role = role;
                    let envelope = project(result, modern).unwrap().unwrap();
                    assert_eq!(envelope["data"]["role"], expected);
                    assert_eq!(envelope["truncationReason"], reason);
                    if reason == "response_bytes" {
                        assert_eq!(envelope["data"]["hits"], json!([]));
                    }
                    assert_wire(&envelope, modern);
                }
            }
        }
    }

    #[test]
    fn invalid_arm_membership_and_missing_required_proof_are_typed_failures() {
        for arms in [
            vec![],
            vec!["vector", "lexical"],
            vec!["lexical", "lexical"],
            vec!["unknown"],
        ] {
            let mut result = fixture(&[("one", "a.js")], COMPLETE, false);
            result.matched_by = vec![arms.into_iter().map(str::to_owned).collect()];
            assert!(
                project(result, true)
                    .unwrap_err()
                    .starts_with("store_unavailable")
            );
        }
        let mut wrong_length = fixture(&[("one", "a.js")], COMPLETE, false);
        wrong_length.matched_by.clear();
        assert!(
            project(wrong_length, true)
                .unwrap_err()
                .starts_with("store_unavailable")
        );
        let mut missing = fixture(&[("one", "a.js")], COMPLETE, false);
        missing.symbols.provenance.clear();
        assert!(
            project(missing, true)
                .unwrap_err()
                .starts_with("store_unavailable")
        );
    }

    #[test]
    fn search_budgets_status_summary_and_final_lf_at_exact_boundary() {
        for modern in [false, true] {
            let make = |size| {
                let mut result = fixture(&[], COMPLETE, false);
                result.search_index.model_name = Some("x".repeat(size));
                result
            };
            let (mut low, mut high) = (0, crate::mcp::wire::RESPONSE_BYTES);
            while low + 1 < high {
                let mid = low + (high - low) / 2;
                if project(make(mid), modern).unwrap().is_some() {
                    low = mid;
                } else {
                    high = mid;
                }
            }
            assert!(project(make(high), modern).unwrap().is_none());
            let envelope = project(make(low), modern).unwrap().unwrap();
            let frame = json!({"jsonrpc":"2.0","id":"search","result":
                crate::mcp::tools::result("trellis_search_symbols", envelope, modern)});
            assert_eq!(
                serde_json::to_vec(&frame).unwrap().len() + 1,
                crate::mcp::wire::RESPONSE_BYTES
            );
        }
    }
}

/// One call item's binding and evidence fields (#467). Without a selected binding they are
/// the syntax-only constants: no binding, unresolved, missing evidence, fresh syntax.
struct CallEvidence {
    binding: Value,
    disposition: &'static str,
    target_id: Value,
    boundary_reason: Value,
    evidence_tier: &'static str,
    semantic_basis: Value,
    freshness: Value,
}

fn call_evidence(
    semantic: Option<&crate::store::CallSemantic>,
) -> Result<CallEvidence, &'static str> {
    use crate::graph_query::BoundaryReason;
    use crate::semantic_evidence::Resolution;
    let Some(semantic) = semantic else {
        return Ok(CallEvidence {
            binding: Value::Null,
            disposition: "unresolved",
            target_id: Value::Null,
            boundary_reason: json!("missingEvidence"),
            evidence_tier: "syntax",
            semantic_basis: Value::Null,
            freshness: json!("fresh"),
        });
    };
    let value = |v: serde_json::Result<Value>| {
        v.map_err(|_| "store_unavailable: semantic binding projection")
    };
    Ok(CallEvidence {
        binding: value(serde_json::to_value(&semantic.binding))?,
        disposition: match semantic.boundary_reason {
            BoundaryReason::Stale => "staleTarget",
            _ => match semantic.binding.resolution {
                Resolution::Resolved => "resolved",
                Resolution::External => "external",
                Resolution::Ambiguous => "ambiguous",
                Resolution::Unresolved => "unresolved",
            },
        },
        target_id: json!(semantic.target_id),
        boundary_reason: value(serde_json::to_value(semantic.boundary_reason))?,
        evidence_tier: "semantic",
        semantic_basis: value(serde_json::to_value(&semantic.provenance.basis))?,
        freshness: value(serde_json::to_value(semantic.provenance.freshness))?,
    })
}

/// A semantic coverage or provenance row in the wire's shared `Coverage`/`Provenance` shape.
fn wire_row<T: serde::Serialize, U: serde::de::DeserializeOwned>(
    row: &T,
) -> Result<U, &'static str> {
    serde_json::to_value(row)
        .and_then(serde_json::from_value)
        .map_err(|_| "store_unavailable: semantic evidence projection")
}

/// #528: emit each distinct semantic basis once per response. Items carry
/// `semanticBasisRef` and proofs `basisRef`, both indexes into `semanticBases`, instead of
/// repeating the whole basis. Equal bases share one entry; refs follow the entries'
/// canonical serialization, so equal answers serialize byte-identically.
fn intern_bases(
    items: &mut [Value],
    provenance: impl IntoIterator<Item = crate::native_evidence::Provenance>,
) -> Result<(Vec<Value>, Vec<Value>), &'static str> {
    use std::collections::BTreeMap;
    let provenance: Vec<Value> = provenance
        .into_iter()
        .map(|row| {
            serde_json::to_value(row).map_err(|_| "store_unavailable: provenance projection")
        })
        .collect::<Result<_, _>>()?;
    let key = |basis: &Value| {
        serde_json::to_string(basis).map_err(|_| "store_unavailable: basis projection")
    };
    let mut unique: BTreeMap<String, Value> = BTreeMap::new();
    for basis in items
        .iter()
        .map(|item| &item["semanticBasis"])
        .chain(provenance.iter().map(|row| &row["basis"]))
        .filter(|basis| !basis.is_null())
    {
        unique.insert(key(basis)?, basis.clone());
    }
    let refs: BTreeMap<&str, usize> = unique
        .keys()
        .enumerate()
        .map(|(index, key)| (key.as_str(), index))
        .collect();
    let reference = |basis: &Value| -> Result<Value, &'static str> {
        if basis.is_null() {
            return Ok(Value::Null);
        }
        Ok(json!(refs[key(basis)?.as_str()]))
    };
    for item in items.iter_mut() {
        let item = item
            .as_object_mut()
            .ok_or("store_unavailable: item projection")?;
        let basis = item.remove("semanticBasis").unwrap_or(Value::Null);
        item.insert("semanticBasisRef".into(), reference(&basis)?);
    }
    let provenance = provenance
        .into_iter()
        .map(|mut row| {
            let fields = row
                .as_object_mut()
                .ok_or("store_unavailable: provenance projection")?;
            let basis = fields.remove("basis").unwrap_or(Value::Null);
            fields.insert("basisRef".into(), reference(&basis)?);
            Ok(row)
        })
        .collect::<Result<Vec<_>, &'static str>>()?;
    let table = unique
        .values()
        .enumerate()
        .map(|(index, basis)| {
            let mut entry = basis.clone();
            entry["ref"] = json!(index);
            entry
        })
        .collect();
    Ok((table, provenance))
}

/// #528 compact mode: keep the answer (items, refs, dispositions, targets, warnings and
/// completeness) and drop the proof detail: `coverage`, `provenance`, `semanticBases`,
/// and each item's `binding` and `semanticBasisRef`.
fn compact_data(data: &mut Value, items: &str) {
    if let Some(fields) = data.as_object_mut() {
        for field in ["coverage", "provenance", "semanticBases"] {
            fields.remove(field);
        }
    }
    for item in data[items].as_array_mut().into_iter().flatten() {
        if let Some(item) = item.as_object_mut() {
            item.remove("binding");
            item.remove("semanticBasisRef");
        }
    }
}

/// Warnings-v1 for the emitted calls (#467): coverage, stale evidence and targets, ambiguous
/// bindings and, when no producer covers them, `syntaxOnly` carrying the producer note.
fn call_warnings<'a>(
    incomplete: bool,
    semantic: &crate::store::McpSemantic,
    emitted: impl IntoIterator<Item = &'a crate::store::CallSemantic>,
) -> Vec<Value> {
    use crate::semantic_evidence::{Freshness, Resolution};
    use std::collections::BTreeMap;
    let mut keys: BTreeMap<(u8, Option<String>), (&str, &str)> = BTreeMap::new();
    if incomplete {
        keys.insert(
            (0, None),
            (
                "coverageIncomplete",
                "Selected syntax coverage is incomplete",
            ),
        );
    }
    for call in emitted {
        if call.coverage.selected
            && matches!(
                call.coverage.state,
                crate::semantic_evidence::CoverageState::Failed
                    | crate::semantic_evidence::CoverageState::Partial
            )
        {
            keys.entry((0, None)).or_insert((
                "coverageIncomplete",
                "Selected semantic coverage is incomplete",
            ));
        }
        for proof in &call.proofs {
            match proof.freshness {
                Freshness::Stale => {
                    keys.insert(
                        (1, Some(proof.id.clone())),
                        ("staleEvidence", "Returned provenance is stale."),
                    );
                }
                Freshness::PossiblyStale => {
                    keys.insert(
                        (1, None),
                        (
                            "staleEvidence",
                            "At least one returned provenance is possibly stale.",
                        ),
                    );
                }
                Freshness::Fresh => {}
            }
        }
        if call.binding.stale_target == Some(true) {
            keys.insert(
                (2, Some(call.binding.provenance_id.clone())),
                ("staleTarget", "A returned binding has a stale target."),
            );
        }
        if call.binding.resolution == Resolution::Ambiguous {
            keys.insert(
                (3, Some(call.binding.provenance_id.clone())),
                ("bindingAmbiguous", "A returned binding is ambiguous."),
            );
        }
    }
    if let Some(note) = semantic.note.as_deref() {
        keys.insert((4, None), ("syntaxOnly", note));
    }
    keys.into_iter()
        .map(|((_, provenance), (code, message))| {
            json!({"code":code,"message":message,"provenanceId":provenance})
        })
        .collect()
}

/// Calls have priority over optional hints in the complete structured-plus-summary wire budget.
pub fn outgoing(
    id: &Value,
    result: crate::store::mcp_read::McpOutgoingWithLines,
    semantic: &crate::store::McpSemantic,
    compact: bool,
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
            "result":super::tools::result("trellis_outgoing_calls", envelope.clone(), modern)});
        serde_json::to_vec(&frame).is_ok_and(|bytes| bytes.len() < super::wire::RESPONSE_BYTES)
    };
    let build = |count: usize, shown: &[usize], compacted: bool| -> Result<Value, &'static str> {
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
        // Top-level truncation describes the required calls only (#167); optional
        // hint clipping is reported per call in `nativeCandidates.omitted`.
        let limited = result.has_more;
        let mut clipped = false;
        let mut boundary = false;
        for (index, item) in result.calls[..count].iter().enumerate() {
            let text = item.selected.callee_text.as_deref();
            clipped |= item.selected.callee_text_truncated;
            let n = shown[index];
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
            let bound = semantic.call(index);
            if let Some(bound) = bound {
                let c: crate::native_evidence::Coverage = wire_row(&bound.coverage)?;
                coverage.insert(
                    (
                        c.producer_id.clone(),
                        c.language.clone(),
                        c.source_set_id.clone(),
                        c.document_path.clone(),
                        c.revision_id.clone(),
                    ),
                    c,
                );
                for proof in &bound.proofs {
                    provenance.insert(proof.id.clone(), wire_row(proof)?);
                }
            }
            boundary |= bound.is_none_or(|bound| {
                bound.boundary_reason != crate::graph_query::BoundaryReason::None
            });
            let evidence = call_evidence(bound)?;
            calls.push(json!({"callId":call.id,"ownerSyntaxId":call.owner_syntax_id,
                "ordinal":call.ordinal,"doc":document_ref(&refs, &item.source.document).ok_or("store_unavailable: missing call ref")?,
                "lines":item.source.lines.map(|lines| json!({"start":lines.start,"end":lines.end})),
                "range":call.range,"calleeRange":call.callee_range,
                "calleeText":text,"calleeTextTruncated":item.selected.callee_text_truncated,
                "displayKey":label,"nativeCandidates":{"items":suggestions,
                    "omitted":item.selected.omitted.map(|unexamined| unexamined + item.selected.candidates.len().saturating_sub(n)),
                    "complete":false},"binding":evidence.binding,"disposition":evidence.disposition,
                "targetId":evidence.target_id,"boundaryReason":evidence.boundary_reason,
                "evidenceTier":evidence.evidence_tier,"semanticBasis":evidence.semantic_basis,
                "freshness":evidence.freshness,"staleBecause":[]}));
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
        let truncated = count < result.calls.len() || limited || clipped;
        let reason = if count < result.calls.len() {
            Some("response_bytes")
        } else if limited {
            Some("limit")
        } else if clipped {
            Some("callee_text")
        } else {
            None
        };
        let warnings = call_warnings(
            incomplete,
            semantic,
            (0..count).filter_map(|index| semantic.call(index)),
        );
        let (bases, provenance) = intern_bases(&mut calls, provenance.into_values())?;
        let mut data = json!({"symbolId":result.owner.selected.declaration.syntax_id,
            "displayKey":owner_display,"calls":calls,"documents":documents,
            "semanticBases":bases,"coverage":coverage,"provenance":provenance});
        if compacted {
            compact_data(&mut data, "calls");
        }
        Ok(
            json!({"schemaVersion":1,"requestId":id,"workspace":root,"catchingUp":catching_up,
            "evidenceBasis":basis,"data":data,
            "warnings":warnings,"partial":incomplete || truncated || boundary,
            "truncated":truncated,"truncationReason":reason}),
        )
    };
    let mut count = 0;
    let mut shown = Vec::new();
    let empty = build(0, &[], compact)?;
    if !fits(&empty) {
        return Ok(None);
    }
    // Compact may fit more required calls than the full answer.
    while count < result.calls.len() {
        shown.push(0);
        if !fits(&build(count + 1, &shown, compact)?) {
            shown.pop();
            break;
        }
        count += 1;
    }
    if count == 0 && !result.calls.is_empty() {
        return Ok(None);
    }
    // #528: optional hints fill only to the page target, not the whole cap, so a default
    // page of calls stays agent-sized; hints left out are counted in `omitted`. They are
    // chosen on the full answer whatever the mode, so compact shows the same optional
    // evidence as the full answer for the same call prefix.
    let fits_page = |envelope: &Value| -> bool {
        let frame = json!({"jsonrpc":"2.0","id":id,
            "result":super::tools::result("trellis_outgoing_calls", envelope.clone(), modern)});
        serde_json::to_vec(&frame).is_ok_and(|bytes| bytes.len() < super::architecture::PAGE_BYTES)
    };
    'candidate: for (index, item) in result.calls[..count].iter().enumerate() {
        for _ in 0..item.selected.candidates.len().min(8) {
            shown[index] += 1;
            if !fits_page(&build(count, &shown, false)?) {
                shown[index] -= 1;
                break 'candidate;
            }
        }
    }
    Ok(Some(build(count, &shown, compact)?))
}

/// Project one rooted call-graph answer inside the response cap (D1, #40).
///
/// A complete answer that does not fit is replaced by the exact contract answer for the
/// largest `maxCalls = k` whose frame fits (`fits(k) && !fits(k + 1)`), re-evaluated over the
/// facts the query already read. The reduction is disclosed by `data.requested` (the caller's
/// values) and `truncationReason: "response_bytes"`. `None` means even `k = 0` is too large.
pub fn call_graph(
    id: &Value,
    answer: &mut crate::graph_query::GraphAnswer,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Result<Option<Value>, &'static str> {
    let Ok(full) = &answer.outcome else {
        return Err("store_unavailable: call graph answer is a contract failure");
    };
    let edges = full.edges.len();
    let requested = answer.requested.clone();
    let basis = json!({"indexGeneration":answer.basis.index_generation.to_string(),
        "indexRevision":answer.basis.index_revision});
    let build =
        |result: &crate::graph_query::GraphResult, reduced: bool| -> Result<Value, &'static str> {
            let reason = if reduced {
                Some("response_bytes")
            } else if result.truncated {
                Some("limit")
            } else {
                None
            };
            Ok(
                json!({"schemaVersion":1,"requestId":id,"workspace":root,"catchingUp":catching_up,
            "evidenceBasis":basis,
            "data":{"requested":requested,"result":result},
            "warnings":result.warnings,"partial":result.partial,"truncated":result.truncated,
            "truncationReason":reason}),
            )
        };
    let fits = |envelope: &Value| -> bool {
        let frame = json!({"jsonrpc":"2.0","id":id,
            "result":super::tools::result("trellis_call_graph", envelope.clone(), modern)});
        serde_json::to_vec(&frame).is_ok_and(|bytes| bytes.len() < super::wire::RESPONSE_BYTES)
    };
    fit_call_graph(
        build(full, false)?,
        edges,
        |edge_count| {
            let result = answer
                .prefix(edge_count)
                .map_err(|_| "store_unavailable: call graph prefix")?;
            build(&result, true)
        },
        fits,
    )
}

fn fit_call_graph(
    complete: Value,
    edges: usize,
    mut prefix: impl FnMut(usize) -> Result<Value, &'static str>,
    mut fits: impl FnMut(&Value) -> bool,
) -> Result<Option<Value>, &'static str> {
    if fits(&complete) {
        return Ok(Some(complete));
    }
    let mut retained = None;
    for edge_count in 0..edges {
        let envelope = prefix(edge_count)?;
        if !fits(&envelope) {
            break;
        }
        retained = Some(envelope);
    }
    Ok(retained)
}

/// Project a name-matched reverse lookup. Each caller carries its own call's selected
/// binding, if any; the name match itself establishes no binding.
#[allow(clippy::too_many_arguments)]
pub fn incoming(
    id: &Value,
    result: crate::store::mcp_read::McpIncomingWithLines,
    semantic: &crate::store::McpSemantic,
    tiers: &Tiers,
    compact: bool,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Result<Option<Value>, &'static str> {
    use std::collections::BTreeMap;
    let owner = &result.selected.selected;
    let basis = json!({"indexGeneration":owner.pin.index_generation.to_string(),
        "indexRevision":owner.pin.index_revision});
    let incomplete = result.scope.iter().any(|row| row[2] != 0 || row[3] != 0);
    let build = |count: usize| -> Result<Value, &'static str> {
        let (documents, refs) = document_table(
            result.calls[..count]
                .iter()
                .map(|item| &item.source.document),
        )?;
        let mut coverage = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        for row in &owner.coverage {
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
        for row in &owner.provenance {
            provenance.insert(row.id.clone(), row.clone());
        }
        let mut callers = Vec::new();
        let mut boundary = false;
        for (index, item) in result.calls[..count].iter().enumerate() {
            let call = &item.call;
            let version = &item.source.document;
            if call.document != version.document
                || call.revision_id != version.revision_id
                || call.provenance_id != version.provenance_id
                || item.caller.document != version.document
                || item.caller.revision_id != version.revision_id
                || item.caller.provenance_id != version.provenance_id
                || item.provenance.id != version.provenance_id
                || item.provenance.content_hash != version.content_hash
                || item.content_hash != version.content_hash
                || item.coverage.document_path != version.document.path
                || item.coverage.source_set_id != version.document.source_set_id
                || item.coverage.revision_id != version.revision_id
            {
                return Err("store_unavailable: incoming proof mismatch");
            }
            let callee_range = call
                .callee_range
                .as_ref()
                .ok_or("store_unavailable: missing callee range")?;
            coverage.insert(
                (
                    item.coverage.producer_id.clone(),
                    item.coverage.language.clone(),
                    item.coverage.source_set_id.clone(),
                    item.coverage.document_path.clone(),
                    item.coverage.revision_id.clone(),
                ),
                item.coverage.clone(),
            );
            provenance.insert(item.provenance.id.clone(), item.provenance.clone());
            let bound = semantic.call(index);
            if let Some(bound) = bound {
                let c: crate::native_evidence::Coverage = wire_row(&bound.coverage)?;
                coverage.insert(
                    (
                        c.producer_id.clone(),
                        c.language.clone(),
                        c.source_set_id.clone(),
                        c.document_path.clone(),
                        c.revision_id.clone(),
                    ),
                    c,
                );
                for proof in &bound.proofs {
                    provenance.insert(proof.id.clone(), wire_row(proof)?);
                }
            }
            boundary |= bound.is_none_or(|bound| {
                bound.boundary_reason != crate::graph_query::BoundaryReason::None
            });
            let evidence = call_evidence(bound)?;
            callers.push(
                json!({"callId":call.id,"ownerSyntaxId":call.owner_syntax_id,
                "callerDisplayKey":display_key(&item.caller),"callerRole":item.caller.role,
                "doc":document_ref(&refs, version).ok_or("store_unavailable: missing caller ref")?,
                "lines":item.source.lines.map(|lines| json!({"start":lines.start,"end":lines.end})),
                "range":call.range,"calleeRange":callee_range,"calleeText":item.callee_text,
                "matchedBy":"name","binding":evidence.binding,"disposition":evidence.disposition,
                "targetId":evidence.target_id,"boundaryReason":evidence.boundary_reason,
                "evidenceTier":evidence.evidence_tier,"semanticBasis":evidence.semantic_basis,
                "freshness":evidence.freshness,"staleBecause":[]}),
            );
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
        let truncated = count < result.calls.len() || result.has_more;
        let reason = if count < result.calls.len() {
            Some("response_bytes")
        } else if result.has_more {
            Some("limit")
        } else {
            None
        };
        let warnings = call_warnings(
            incomplete,
            semantic,
            (0..count).filter_map(|index| semantic.call(index)),
        );
        let incomplete = warnings
            .first()
            .is_some_and(|warning| warning["code"] == "coverageIncomplete");
        let (bases, provenance) = intern_bases(&mut callers, provenance.into_values())?;
        let mut data = json!({
            "symbolId":owner.declaration.syntax_id,"displayKey":display_key(&owner.declaration),
            "match":{"basis":"name","sameNameDeclarations":result.same_name_declarations,
                "limitations":["aliases","functionValues","reExports","dynamicDispatch",
                    "opaqueMacros","sameNameDeclarations"]},
            "callers":callers,"documents":documents,"semanticBases":bases,"coverage":coverage,
            "scopeCoverage":language_status(&result.scope, tiers),
            "provenance":provenance});
        if compact {
            compact_data(&mut data, "callers");
        }
        Ok(
            json!({"schemaVersion":1,"requestId":id,"workspace":root,"catchingUp":catching_up,
            "evidenceBasis":basis,"data":data,
            "warnings":warnings,"partial":incomplete || truncated || boundary,
            "truncated":truncated,"truncationReason":reason}),
        )
    };
    let fits = |envelope: &Value| {
        let frame = json!({"jsonrpc":"2.0","id":id,
            "result":super::tools::result("trellis_incoming_calls", envelope.clone(), modern)});
        serde_json::to_vec(&frame).is_ok_and(|bytes| bytes.len() < super::wire::RESPONSE_BYTES)
    };
    if !fits(&build(0)?) {
        return Ok(None);
    }
    let mut count = 0;
    while count < result.calls.len() && fits(&build(count + 1)?) {
        count += 1;
    }
    if count == 0 && !result.calls.is_empty() {
        return Ok(None);
    }
    Ok(Some(build(count)?))
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
        outgoing(
            id,
            with_lines(result),
            &syntax_only(),
            false,
            root,
            catching_up,
            modern,
        )
        .unwrap()
    }
    pub(super) fn syntax_only() -> crate::store::McpSemantic {
        crate::store::McpSemantic::syntax_only(crate::store::NO_PRODUCER_NOTE, 0)
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
        let valid = outgoing(
            &json!(1),
            make_result(),
            &syntax_only(),
            false,
            root,
            false,
            true,
        )
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
        let failure =
            outgoing(&json!(1), wrong, &syntax_only(), false, root, false, true).unwrap_err();
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
                "result":super::super::tools::result("trellis_outgoing_calls", bytes,modern)});
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
        "warnings":[{"code":"syntaxOnly","message":"This answer is syntax-derived",
            "provenanceId":null}],
        "partial":false,"truncated":false,"truncationReason":null,
    });
    let frame = json!({"jsonrpc":"2.0","id":id,
        "result":super::tools::result("trellis_read_source", envelope.clone(), modern)});
    (serde_json::to_vec(&frame).ok()?.len() < super::wire::RESPONSE_BYTES).then_some(envelope)
}

#[cfg(test)]
mod incoming_tests {
    use super::outgoing_tests::{owner, required_call, with_lines};
    use super::*;
    use crate::store::mcp_read::{McpIncomingCallWithLines, McpIncomingWithLines};

    fn fixture(calls: usize, has_more: bool, diagnostic: usize) -> McpIncomingWithLines {
        let outgoing = with_lines(crate::store::mcp_read::McpOutgoing {
            owner: owner(),
            calls: (0..calls).map(|_| required_call(false)).collect(),
            has_more,
        });
        let mut selected = outgoing.owner;
        selected.selected.coverage[0].state = "complete".into();
        selected.selected.coverage[0].diagnostic = Some("x".repeat(diagnostic));
        let calls = outgoing
            .calls
            .into_iter()
            .enumerate()
            .map(|(i, item)| {
                let mut call = item.selected.call;
                call.id = format!("cid:v1:{i}");
                McpIncomingCallWithLines {
                    call,
                    caller: selected.selected.declaration.clone(),
                    content_hash: selected.selected.content_hash.clone(),
                    source: item.source,
                    callee_text: "Target".into(),
                    coverage: selected.selected.coverage[0].clone(),
                    provenance: selected.selected.provenance[0].clone(),
                }
            })
            .collect();
        McpIncomingWithLines {
            selected,
            calls,
            same_name_declarations: 3,
            scope: [[0; 5], [0; 5], [0; 5], [2, 2, 0, 0, 4]],
            has_more,
        }
    }
    fn project(result: McpIncomingWithLines, modern: bool) -> Option<Value> {
        incoming(
            &json!(1),
            result,
            &crate::store::McpSemantic::syntax_only(crate::store::NO_PRODUCER_NOTE, 0),
            &Default::default(),
            false,
            std::path::Path::new("/workspace"),
            false,
            modern,
        )
        .unwrap()
    }
    #[test]
    fn incoming_empty_and_required_item_closed_shape() {
        for modern in [false, true] {
            let empty = project(fixture(0, false, 0), modern).unwrap();
            let data = &empty["data"];
            assert_eq!(data.as_object().unwrap().len(), 9);
            assert_eq!(data["semanticBases"], json!([]));
            assert_eq!(data["documents"], json!([]));
            assert_eq!(data["coverage"].as_array().unwrap().len(), 1);
            assert_eq!(data["provenance"].as_array().unwrap().len(), 1);
            assert_eq!(
                data["match"]["limitations"],
                json!([
                    "aliases",
                    "functionValues",
                    "reExports",
                    "dynamicDispatch",
                    "opaqueMacros",
                    "sameNameDeclarations"
                ])
            );
            assert_eq!(data["match"]["sameNameDeclarations"], 3);
            assert_eq!(
                data["scopeCoverage"][3]["coverage"],
                json!({"selected":2,
                "complete":2,"partial":0,"failed":0,"unselected":4})
            );
            assert_eq!(empty["partial"], false);
            assert_eq!(empty["warnings"].as_array().unwrap().len(), 1);
            assert_eq!(empty["warnings"][0]["code"], "syntaxOnly");
            let full = project(fixture(1, false, 0), modern).unwrap();
            let caller = &full["data"]["callers"][0];
            assert_eq!(caller.as_object().unwrap().len(), 18);
            assert_eq!(caller["callerRole"], "production");
            assert_eq!(caller["callerDisplayKey"], "a.js#Owner");
            assert_eq!(caller["calleeText"], "Target");
            assert_eq!(caller["binding"], Value::Null);
            assert_eq!(caller["matchedBy"], "name");
            assert_eq!(full["partial"], true);
            assert_eq!(full["data"]["documents"].as_array().unwrap().len(), 1);
            assert_eq!(full["data"]["callers"][0]["doc"], 0);
            assert_eq!(full["warnings"].as_array().unwrap().len(), 1);
            let frame = json!({"jsonrpc":"2.0","id":1,
                "result":crate::mcp::tools::result("trellis_incoming_calls",full,modern)});
            assert!(serde_json::to_vec(&frame).unwrap().len() <= crate::mcp::wire::RESPONSE_BYTES);
        }
    }
    #[test]
    fn incoming_scope_and_limit_are_independent_of_emitted_calls() {
        let mut result = fixture(1, true, 0);
        result.scope[3] = [2, 1, 1, 0, 4];
        result.calls[0].caller.role = crate::native_evidence::DeclarationRole::Test;
        let envelope = project(result, true).unwrap();
        assert_eq!(envelope["warnings"][0]["code"], "coverageIncomplete");
        assert_eq!(envelope["warnings"][1]["code"], "syntaxOnly");
        assert_eq!(envelope["data"]["callers"][0]["callerRole"], "test");
        assert_eq!(envelope["truncationReason"], "limit");
        assert_eq!(
            envelope["data"]["scopeCoverage"][3]["coverage"]["partial"],
            1
        );
    }
    #[test]
    fn incoming_byte_clipping_keeps_order_and_scope() {
        for modern in [false, true] {
            let mut result = fixture(50, false, 0);
            for (index, item) in result.calls.iter_mut().enumerate() {
                item.callee_text = format!("{index:02}{}", "t".repeat(1000));
            }
            let envelope = project(result, modern).unwrap();
            let items = envelope["data"]["callers"].as_array().unwrap();
            assert!(!items.is_empty());
            assert!(items.len() < 50);
            assert_eq!(envelope["truncationReason"], "response_bytes");
            assert_eq!(
                envelope["data"]["scopeCoverage"][3]["coverage"]["complete"],
                2
            );
            assert_eq!(envelope["data"]["documents"].as_array().unwrap().len(), 1);
            assert_eq!(envelope["data"]["coverage"].as_array().unwrap().len(), 1);
            for (index, item) in items.iter().enumerate() {
                assert_eq!(item["callId"], format!("cid:v1:{index}"));
                assert_eq!(
                    item["calleeText"],
                    format!("{index:02}{}", "t".repeat(1000))
                );
            }
            let frame = json!({"jsonrpc":"2.0","id":1,
                "result":crate::mcp::tools::result("trellis_incoming_calls",envelope,modern)});
            assert!(serde_json::to_vec(&frame).unwrap().len() < crate::mcp::wire::RESPONSE_BYTES);
        }
    }
    #[test]
    fn incoming_exact_wire_budget_and_first_item_refusal() {
        for modern in [false, true] {
            let fits = |n| project(fixture(0, false, n), modern).is_some();
            let (mut low, mut high) = (0, crate::mcp::wire::RESPONSE_BYTES);
            while low + 1 < high {
                let mid = low + (high - low) / 2;
                if fits(mid) { low = mid } else { high = mid }
            }
            assert!(fits(low));
            assert!(!fits(high));
            let exact = project(fixture(0, false, low), modern).unwrap();
            let frame = json!({"jsonrpc":"2.0","id":1,
                "result":crate::mcp::tools::result("trellis_incoming_calls",exact,modern)});
            assert_eq!(
                serde_json::to_vec(&frame).unwrap().len() + 1,
                crate::mcp::wire::RESPONSE_BYTES
            );
            assert!(project(fixture(1, false, low), modern).is_none());
            let mut result = fixture(1, false, 0);
            result.calls[0].callee_text = "x".repeat(crate::mcp::wire::RESPONSE_BYTES);
            assert!(project(result, modern).is_none());
        }
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;
    use crate::store::mcp_read::McpSource;
    #[test]
    fn source_complete_frame_or_range_error_without_truncation() {
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
                "result":super::super::tools::result("trellis_read_source", envelope,modern)});
            let mut bytes = Vec::new();
            super::super::wire::write_response(&mut bytes, &frame).unwrap();
            assert!(bytes.len() <= super::super::wire::RESPONSE_BYTES);
            assert!(
                source(
                    &json!(1),
                    item("\0".repeat(16384)),
                    std::path::Path::new("/root"),
                    false,
                    modern
                )
                .is_none()
            );
        }
    }
}

#[cfg(test)]
mod call_graph_tests {
    use super::*;
    use crate::{
        graph_query::{
            DocumentFacts, FrontierReason, GraphAnswer, GraphFacts, GraphRequest, GraphSnapshot,
            evaluate,
        },
        model::IndexPin,
        native_evidence::{self as native, DocumentKey},
        semantic_evidence as sem,
    };
    use std::{collections::BTreeMap, sync::Arc};

    const SS: &str = "source-set:v1:s";
    const REV: &str = "revision:v2:r";

    fn sid(n: u64) -> String {
        format!("sid:v1:{n:032x}")
    }

    /// A syntax-only root whose name is `padding` bytes long and which makes `calls` calls.
    fn answer(calls: u64, padding: usize) -> GraphAnswer {
        let key = DocumentKey {
            source_set_id: SS.into(),
            language: "rust".into(),
            path: "src/lib.rs".into(),
        };
        let name = "r".repeat(padding.max(1));
        let declaration = native::Declaration {
            role: native::DeclarationRole::Production,
            syntax_id: sid(1),
            document: key.clone(),
            revision_id: REV.into(),
            kind: "function".into(),
            name: Some(name.clone()),
            lookup_key: Some(name.clone()),
            ancestors: vec![],
            key: native::Key {
                kind: "function".into(),
                name: Some(name.clone()),
                signature: None,
                ordinal: 0,
            },
            range: native::Range { start: 0, end: 1 },
            name_range: None,
            header: native::Header {
                kind: "function".into(),
                name: Some(name),
                modifiers: vec![],
                type_parameters: vec![],
                parameters: vec![],
                result_type: None,
                bases: vec![],
            },
            provenance_id: "native:doc".into(),
        };
        let owned = (0..calls)
            .map(|i| native::Call {
                id: format!("occ:v2:{:032x}", 100 + i),
                owner_syntax_id: sid(1),
                ordinal: i as usize,
                document: key.clone(),
                revision_id: REV.into(),
                range: native::Range {
                    start: 10 + i as usize,
                    end: 11 + i as usize,
                },
                callee_range: None,
                spelling: Some(format!("f{i}")),
                region_ids: vec![],
                provenance_id: "native:doc".into(),
            })
            .collect::<Vec<_>>();
        let document = DocumentFacts {
            key: key.clone(),
            declarations: BTreeMap::from([(sid(1), declaration)]),
            calls: BTreeMap::from([(sid(1), owned)]),
            native_coverage: sem::Coverage {
                producer_id: "native".into(),
                language: sem::Language::Rust,
                source_set_id: SS.into(),
                document_path: key.path.clone(),
                revision_id: REV.into(),
                requested: true,
                selected: true,
                state: sem::CoverageState::Complete,
                supported_roles: vec![],
                observed_roles: vec![],
                diagnostic: None,
            },
            native_provenance: sem::Provenance {
                id: "native:doc".into(),
                producer_id: "native".into(),
                document: key.clone(),
                revision_id: REV.into(),
                content_hash: "h".into(),
                evidence_kind: sem::EvidenceKind::MeasuredSyntax,
                basis: None,
                freshness: sem::Freshness::Fresh,
                derived_from: None,
            },
            semantic: None,
        };
        let mut facts = GraphFacts::complete(GraphSnapshot {
            source_set_id: SS.into(),
            revision_id: REV.into(),
        });
        facts.locations.insert(sid(1), Some(key.clone()));
        facts.documents.insert(key, Arc::new(document));
        let requested = GraphRequest {
            source_set_id: SS.into(),
            revision_id: REV.into(),
            root_syntax_id: sid(1),
            semantic_producer_id: None,
            depth: 2,
            max_nodes: 150,
            max_calls: 500,
            include_external: false,
            include_tests: false,
        };
        let outcome = evaluate(&requested, &mut facts).unwrap();
        GraphAnswer {
            basis: IndexPin {
                index_generation: uuid::Uuid::from_u128(1),
                index_revision: 1,
            },
            requested,
            outcome,
            facts,
        }
    }

    fn frame_len(envelope: &Value, modern: bool) -> usize {
        let frame = json!({"jsonrpc":"2.0","id":"graph",
            "result":crate::mcp::tools::result("trellis_call_graph", envelope.clone(), modern)});
        serde_json::to_vec(&frame).unwrap().len()
    }

    fn project(answer: &mut GraphAnswer, modern: bool) -> Option<Value> {
        call_graph(
            &json!("graph"),
            answer,
            std::path::Path::new("/w"),
            false,
            modern,
        )
        .unwrap()
    }

    #[test]
    fn fitting_answer_is_unreduced() {
        for modern in [false, true] {
            let mut small = answer(3, 1);
            let envelope = project(&mut small, modern).unwrap();
            let result = &envelope["data"]["result"];
            assert_eq!(result["edges"].as_array().unwrap().len(), 3);
            assert_eq!(result["request"]["maxCalls"], 500);
            assert_eq!(envelope["data"]["requested"], result["request"]);
            assert_eq!(envelope["truncated"], false);
            assert_eq!(envelope["truncationReason"], Value::Null);
            assert_eq!(envelope["warnings"], result["warnings"]);
        }
    }

    #[test]
    fn complete_frame_is_checked_once_without_building_prefixes() {
        for edges in [0, 1, 300, 500] {
            let complete = json!({"complete": edges});
            let mut checks = 0;
            let result = fit_call_graph(
                complete.clone(),
                edges,
                |_| panic!("a fitting complete answer must not build prefixes"),
                |frame| {
                    checks += 1;
                    frame == &complete
                },
            )
            .unwrap();
            assert_eq!(result, Some(complete));
            assert_eq!(checks, 1);
        }
    }

    #[test]
    fn a_nonfitting_intermediate_prefix_cannot_hide_a_fitting_complete_frame() {
        let complete = json!({"jsonrpc":"2.0","id":"graph","result":"fits"});
        let intermediate = json!({"jsonrpc":"2.0","id":"graph",
            "result":{"disclosure":"x".repeat(crate::mcp::wire::RESPONSE_BYTES)}});
        let fits = |frame: &Value| {
            serde_json::to_vec(frame).unwrap().len() < crate::mcp::wire::RESPONSE_BYTES
        };
        assert!(fits(&complete));
        assert!(!fits(&intermediate));
        assert_eq!(
            fit_call_graph(complete.clone(), 3, |_| Ok(intermediate.clone()), fits).unwrap(),
            Some(complete)
        );
    }

    #[test]
    fn overflow_checks_complete_then_visits_each_prefix_once_until_first_refusal() {
        let mut prefixes = Vec::new();
        let mut checks = 0;
        let result = fit_call_graph(
            json!(500),
            500,
            |count| {
                prefixes.push(count);
                Ok(json!(count))
            },
            |frame| {
                checks += 1;
                frame.as_u64().unwrap() < 3
            },
        )
        .unwrap();
        assert_eq!(result, Some(json!(2)));
        assert_eq!(prefixes, [0, 1, 2, 3]);
        assert_eq!(checks, 5);
    }

    #[test]
    fn oversized_answer_fits_a_frozen_prefix_without_changing_limits() {
        for modern in [false, true] {
            let mut big = answer(600, 1);
            let full = big.outcome.clone().unwrap();
            assert_eq!(full.edges.len(), 500);
            let envelope = project(&mut big, modern).unwrap();
            let result = &envelope["data"]["result"];
            let k = result["edges"].as_array().unwrap().len();
            assert!(k < 500);
            assert_eq!(result["request"]["maxCalls"], 500);
            assert_eq!(envelope["data"]["requested"], result["request"]);
            assert_eq!(envelope["truncated"], true);
            assert_eq!(envelope["truncationReason"], "response_bytes");
            assert_eq!(envelope["partial"], true);
            let frontier = result["frontier"].as_array().unwrap();
            assert!(frontier.iter().all(|row| row["nextOrdinal"].is_null()));
            let omitted = |reason| {
                frontier
                    .iter()
                    .filter(|row| row["reason"] == reason)
                    .map(|row| row["omittedCalls"].as_u64().unwrap())
                    .sum::<u64>()
            };
            assert_eq!(omitted(json!(FrontierReason::CallLimit)), 100);
            assert_eq!(
                omitted(json!(FrontierReason::ResponseBytes)),
                500 - k as u64
            );
            assert_eq!(result["omitted"]["callLimit"], 100);
            assert_eq!(result["omitted"]["responseBytes"], 500 - k as u64);
            assert!(frame_len(&envelope, modern) < crate::mcp::wire::RESPONSE_BYTES);
            assert_eq!(
                serde_json::to_value(big.prefix(k).unwrap()).unwrap(),
                *result
            );
            let mut probe = envelope.clone();
            probe["data"]["result"] = serde_json::to_value(big.prefix(k + 1).unwrap()).unwrap();
            assert!(frame_len(&probe, modern) >= crate::mcp::wire::RESPONSE_BYTES);
            assert_eq!(project(&mut big, modern).unwrap(), envelope);
        }
    }

    #[test]
    fn root_alone_over_cap_is_range_too_large() {
        let mut huge = answer(2, 70_000);
        assert!(project(&mut huge, true).is_none());
        let mut no_calls = answer(0, 70_000);
        assert!(project(&mut no_calls, false).is_none());
    }
}

/// #528 response-size regressions on this repository's real shapes: the owner's live
/// `incoming_calls` on `index_workspace` returned ~62 KB and stopped at 12 callers because
/// each semantic caller repeated an ~800-byte basis in the item and again in its proof.
#[cfg(test)]
mod size_tests {
    use super::*;
    use crate::store::mcp_read::{
        McpDeclaration, McpDeclarationWithLines, McpDocumentVersion, McpIncomingCallWithLines,
        McpIncomingWithLines, McpItemSource, McpLines,
    };

    const SOURCE_SET: &str = "source-set:v1:0d7bc85f-6ff5-4e8f-b71a-8a2a5a60a4ab";
    const REVISION: &str =
        "revision:v2:5b541324769de8538eabba403d11f5f8fcfbf8dd27ef2e07911857b328ccb4c5";

    fn hex(seed: usize, len: usize) -> String {
        format!("{seed:x}").repeat(len)[..len].to_owned()
    }

    fn document(path: &str) -> Value {
        json!({"language":"rust","path":path,"sourceSetId":SOURCE_SET})
    }

    /// The basis rust-analyzer imports carry, as the live install serialized it.
    fn basis() -> Value {
        json!({"artifactHash":hex(1, 64),"configHash":hex(2, 64),"dependencyHash":hex(3, 64),
            "language":"rust","lookupDependencies":[],"producerHash":hex(4, 64),
            "producerId":"rust-analyzer","producerVersion":"1.98.1 (48a229ce 2026-09-01)",
            "revisionId":REVISION,"sourceManifestHash":hex(5, 64),"sourceSetId":SOURCE_SET,
            "toolchainHash":hex(6, 64)})
    }

    fn native_coverage(path: &str) -> crate::native_evidence::Coverage {
        serde_json::from_value(json!({"producerId":"trellis.native.syntax","language":"rust",
            "sourceSetId":SOURCE_SET,"documentPath":path,"revisionId":REVISION,"requested":true,
            "selected":true,"state":"partial","supportedRoles":["definition","call"],
            "observedRoles":["definition","call"],
            "diagnostic":"opaque Rust macro/attribute token trees skipped; calls and definitions inside were not measured"}))
        .unwrap()
    }

    fn native_proof(path: &str, file: usize) -> crate::native_evidence::Provenance {
        serde_json::from_value(
            json!({"id":format!("native-proof:v1:{}", hex(file + 7, 64)),
            "producerId":"trellis.native.syntax","document":document(path),"revisionId":REVISION,
            "contentHash":hex(file + 7, 64),"evidenceKind":"measuredSyntax","basis":null,
            "freshness":"fresh","derivedFrom":null}),
        )
        .unwrap()
    }

    fn declaration(
        path: &str,
        file: usize,
        name: &str,
        start: usize,
    ) -> crate::native_evidence::Declaration {
        serde_json::from_value(json!({"syntaxId":format!("sid:v1:{}", hex(start + 9, 32)),
            "document":document(path),"revisionId":REVISION,"kind":"function","role":"test",
            "name":name,"lookupKey":name,"ancestors":[],
            "key":{"kind":"function","name":name,"signature":null,"ordinal":0},
            "range":{"start":start,"end":start + 900},"nameRange":null,
            "header":{"kind":"function","name":name,"modifiers":[],"typeParameters":[],
                "parameters":[],"resultType":null,"bases":[]},
            "provenanceId":format!("native-proof:v1:{}", hex(file + 7, 64))}))
        .unwrap()
    }

    fn version(path: &str, file: usize) -> McpDocumentVersion {
        McpDocumentVersion {
            document: serde_json::from_value(document(path)).unwrap(),
            revision_id: REVISION.into(),
            content_hash: hex(file + 7, 64),
            provenance_id: format!("native-proof:v1:{}", hex(file + 7, 64)),
        }
    }

    /// `callers` name-matched sites, two per test file, each with a resolved
    /// rust-analyzer binding and its own proof, as `index_workspace`'s callers have.
    fn incoming(callers: usize) -> (McpIncomingWithLines, crate::store::McpSemantic) {
        let owner_path = "src/indexer.rs";
        let owner = declaration(owner_path, 0, "index_workspace", 100);
        let selected = McpDeclarationWithLines {
            selected: McpDeclaration {
                declaration: owner.clone(),
                content_hash: hex(7, 64),
                coverage: vec![native_coverage(owner_path)],
                provenance: vec![native_proof(owner_path, 0)],
                pin: serde_json::from_value(json!({
                    "indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":6}))
                .unwrap(),
            },
            source: McpItemSource {
                document: version(owner_path, 0),
                lines: Some(McpLines { start: 10, end: 40 }),
            },
        };
        let mut calls = Vec::new();
        let mut semantic = Vec::new();
        for i in 0..callers {
            let file = 1 + i / 2; // two callers per file, as index_workspace has
            let path = format!("tests/index/index_workspace_{file}.rs");
            let start = 1000 * (i + 1);
            let caller = declaration(&path, file, &format!("indexes_case_{i}"), start);
            let call_id = format!("occ:v2:{}", hex(i + 3, 32));
            let call: crate::native_evidence::Call = serde_json::from_value(json!({
                "id":call_id,"ownerSyntaxId":caller.syntax_id,"ordinal":0,
                "document":document(&path),"revisionId":REVISION,
                "range":{"start":start + 40,"end":start + 80},
                "calleeRange":{"start":start + 40,"end":start + 55},"spelling":"index_workspace",
                "regionIds":[],"provenanceId":format!("native-proof:v1:{}", hex(file + 7, 64))}))
            .unwrap();
            let proof_id = hex(i + 11, 64);
            let proof = json!({"id":proof_id,"producerId":"rust-analyzer",
                "document":document(&path),"revisionId":REVISION,"contentHash":hex(file + 7, 64),
                "evidenceKind":"semanticReference","basis":basis(),"freshness":"fresh",
                "derivedFrom":{"factDigest":hex(i + 12, 64),"provenanceId":hex(i + 13, 64),
                    "recordKind":"occurrence"}});
            let binding = json!({"callId":call_id,"candidates":[],
                "declaredTarget":{"document":document(owner_path),"kind":"internal",
                    "revisionId":REVISION,"syntaxId":owner.syntax_id},
                "dispatch":"unknown",
                "join":{"anchor":{"contentHash":hex(file + 7, 64),"document":document(&path),
                    "kind":"callee","range":{"start":start + 40,"end":start + 55},
                    "revisionId":REVISION},"candidateIds":[call_id],"diagnostic":null,
                    "status":"exact"},
                "possibleDispatch":[],"possibleDispatchComplete":false,"provenanceId":proof_id,
                "resolution":"resolved","staleTarget":false});
            let coverage = json!({"producerId":"rust-analyzer","language":"rust",
                "sourceSetId":SOURCE_SET,"documentPath":path,"revisionId":REVISION,
                "requested":true,"selected":true,"state":"partial",
                "supportedRoles":["definition","read","write","call","type","import","alias"],
                "observedRoles":["definition","read","call","type"],
                "diagnostic":"unsupported_native_gap"});
            semantic.push(Some(crate::store::CallSemantic {
                binding: serde_json::from_value(binding).unwrap(),
                provenance: serde_json::from_value(proof.clone()).unwrap(),
                boundary_reason: crate::graph_query::BoundaryReason::Dispatch,
                target_id: Some(owner.syntax_id.clone()),
                coverage: serde_json::from_value(coverage).unwrap(),
                proofs: vec![serde_json::from_value(proof).unwrap()],
            }));
            calls.push(McpIncomingCallWithLines {
                call,
                caller,
                content_hash: hex(file + 7, 64),
                source: McpItemSource {
                    document: version(&path, file),
                    lines: Some(McpLines {
                        start: 40 + i as u64,
                        end: 41 + i as u64,
                    }),
                },
                callee_text: "index_workspace".into(),
                coverage: native_coverage(&path),
                provenance: native_proof(&path, file),
            });
        }
        (
            McpIncomingWithLines {
                selected,
                calls,
                same_name_declarations: 1,
                scope: [[0; 5], [355, 22, 333, 0, 0], [0; 5], [0; 5]],
                has_more: true,
            },
            crate::store::McpSemantic {
                producer_id: Some("rust-analyzer".into()),
                note: None,
                calls: semantic,
            },
        )
    }

    fn frame_bytes(envelope: &Value) -> usize {
        let frame = json!({"jsonrpc":"2.0","id":1,
            "result":crate::mcp::tools::result("trellis_incoming_calls", envelope.clone(), true)});
        serde_json::to_vec(&frame).unwrap().len() + 1
    }

    fn project(callers: usize, compact: bool) -> Value {
        let (result, semantic) = incoming(callers);
        super::incoming(
            &json!(1),
            result,
            &semantic,
            &super::tests::rust_semantic("fresh", [326, 14, 312, 0, 29]),
            compact,
            std::path::Path::new("/Users/someone/projects/trellis"),
            false,
            true,
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn default_semantic_caller_page_stays_under_25_kib() {
        let page = project(crate::mcp::CALL_DEFAULT_LIMIT, false);
        assert_eq!(
            page["data"]["callers"].as_array().unwrap().len(),
            crate::mcp::CALL_DEFAULT_LIMIT
        );
        let bytes = frame_bytes(&page);
        assert!(bytes < 25 * 1024, "default incoming page is {bytes} bytes");
        // The basis is emitted once and referenced from every caller and proof.
        let bases = page["data"]["semanticBases"].as_array().unwrap();
        assert_eq!(bases.len(), 1);
        assert_eq!(bases[0]["ref"], 0);
        assert_eq!(bases[0]["producerHash"], hex(4, 64));
        let wire = serde_json::to_string(&page).unwrap();
        assert_eq!(wire.matches(&hex(4, 64)).count(), 1);
        for caller in page["data"]["callers"].as_array().unwrap() {
            assert_eq!(caller["semanticBasisRef"], 0);
            assert_eq!(caller["evidenceTier"], "semantic");
            assert!(caller.get("semanticBasis").is_none());
        }
        for proof in page["data"]["provenance"].as_array().unwrap() {
            assert!(proof.get("basis").is_none());
            let expected = if proof["evidenceKind"] == "measuredSyntax" {
                json!(null)
            } else {
                json!(0)
            };
            assert_eq!(proof["basisRef"], expected);
        }
        // Rust has fresh imported evidence: no syntaxOnly, and scope reports it beside
        // native coverage, which stays partial.
        assert!(
            page["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .all(|w| w["code"] != "syntaxOnly")
        );
        let rust = &page["data"]["scopeCoverage"][1];
        assert_eq!(rust["evidenceTier"], "syntax");
        assert_eq!(rust["coverage"]["partial"], 333);
        assert_eq!(rust["semantic"]["available"], true);
        assert_eq!(rust["semantic"]["coverage"]["partial"], 312);
        assert_eq!(page["partial"], true);
    }

    #[test]
    fn the_basis_table_cuts_a_full_caller_page_by_over_30_percent() {
        // Before #528 each caller and proof repeated the whole basis: re-inline it to
        // compare against the same emitted prefix (the old shape would not even fit it).
        let page = project(20, false);
        let mut inlined = page.clone();
        let basis = basis();
        for caller in inlined["data"]["callers"].as_array_mut().unwrap() {
            let fields = caller.as_object_mut().unwrap();
            fields.remove("semanticBasisRef");
            fields.insert("semanticBasis".into(), basis.clone());
        }
        for proof in inlined["data"]["provenance"].as_array_mut().unwrap() {
            let fields = proof.as_object_mut().unwrap();
            let semantic = fields.remove("basisRef").unwrap() != Value::Null;
            fields.insert(
                "basis".into(),
                if semantic { basis.clone() } else { Value::Null },
            );
        }
        inlined["data"]
            .as_object_mut()
            .unwrap()
            .remove("semanticBases");
        let (now, before) = (frame_bytes(&page), frame_bytes(&inlined));
        assert!(before > 50 * 1024, "the old shape is {before} bytes");
        assert!(
            now * 10 < before * 7,
            "{now} bytes is not much below {before}"
        );
    }

    #[test]
    fn compact_fits_twenty_semantic_callers_under_25_kib() {
        let page = project(20, true);
        assert_eq!(page["data"]["callers"].as_array().unwrap().len(), 20);
        let bytes = frame_bytes(&page);
        assert!(bytes < 25 * 1024, "compact page is {bytes} bytes");
        let data = page["data"].as_object().unwrap();
        for field in ["coverage", "provenance", "semanticBases"] {
            assert!(!data.contains_key(field), "{field}");
        }
        for caller in page["data"]["callers"].as_array().unwrap() {
            assert!(caller.get("binding").is_none());
            assert!(caller.get("semanticBasisRef").is_none());
            assert_eq!(caller["disposition"], "resolved");
            assert_eq!(caller["evidenceTier"], "semantic");
            assert!(caller["targetId"].is_string());
        }
        // Completeness and warnings are those of the full answer.
        let full = project(20, false);
        assert_eq!(page["warnings"], full["warnings"]);
        assert_eq!(page["partial"], full["partial"]);
        // Twenty full callers overflow the cap; compact returns them all.
        assert_eq!(full["truncationReason"], "response_bytes");
        assert_eq!(page["truncationReason"], "limit");
        assert_eq!(page["data"]["scopeCoverage"], full["data"]["scopeCoverage"]);
    }
}
