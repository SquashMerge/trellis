//! Closed MCP wire projection; no Store admission occurs in this layer.
use serde_json::{Value, json};

/// Build refs from precisely the items that survive the wire byte budget.
type DocumentKey = (usize, String, String, String);
fn document_key(version: &crate::store::mcp_read::McpDocumentVersion) -> DocumentKey {
    (
        language_rank(&version.document.language),
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
pub type Tiers = [crate::store::LanguageTier; 5];

/// Native tuple counts per language with each language's semantic tier, as describe reads them.
pub type LanguageCounts = ([[u64; 5]; 5], Tiers);

fn counts(row: &[u64; 5]) -> Value {
    json!({"selected":row[0],"complete":row[1],"partial":row[2],"failed":row[3],"unselected":row[4]})
}

/// How many of a language's projected documents (#620) are in each read status (#930):
/// counts only, so a row stays small however many documents are catching up.
fn document_status(tier: &crate::store::LanguageTier) -> Value {
    use crate::model::SemanticStatus;
    let mut counts = [0u64; 3];
    for document in &tier.documents {
        counts[match document.projection.semantic_status {
            SemanticStatus::OlderBasis => 0,
            SemanticStatus::CatchingUp => 1,
            SemanticStatus::NeedsReimport => 2,
        }] += 1;
    }
    json!({"olderBasis":counts[0],"catchingUp":counts[1],"needsReimport":counts[2]})
}

/// The five `LanguageStatus` rows, in #22 language order, from per-language native
/// tuple counts [selected, complete, partial, failed, unselected] and each language's
/// selected imported evidence (#528). `nativeCoverageTier` and `coverage` describe the
/// native syntax tuples; `semantic` separately reports the selected producer, its
/// freshness and its tuple counts, with `available` true when it is selected, fresh and
/// covers at least one document. Partial coverage of either stays visible.
fn language_status(rows: &[[u64; 5]; 5], tiers: &Tiers) -> Vec<Value> {
    crate::semantic_evidence::LANGUAGE_ORDER
        .into_iter()
        .zip(rows)
        .zip(tiers)
        .map(|((language, row), tier)| {
            json!({"language":language,"nativeCoverageTier":"syntax","coverage":counts(row),
            "semantic":{"available":tier.semantic(),"producerId":tier.producer_id,
                "freshness":if tier.freshness.is_empty() {"unavailable"} else {tier.freshness},
                "coverage":counts(&tier.counts),"note":tier.note,"reimport":tier.reimport,
                "documentStatus":document_status(tier)}})
        })
        .collect()
}

/// Describe's `LanguageStatus` rows: each semantic block also says why its partial tuples
/// are partial (#520), or `null` when none is.
fn describe_language_status(rows: &[[u64; 5]; 5], tiers: &Tiers) -> Vec<Value> {
    let mut languages = language_status(rows, tiers);
    for (status, tier) in languages.iter_mut().zip(tiers) {
        status["semantic"]["partialReasons"] =
            tier.partial_reasons
                .as_ref()
                .map_or(Value::Null, |summary| {
                    let mut reasons = json!(summary.partial_reasons);
                    reasons["summary"] = json!(summary.summary);
                    reasons
                });
    }
    languages
}

const LANGUAGES: [&str; 5] = crate::semantic_evidence::LANGUAGE_ORDER;

/// Sort rank of a document language in #22 order; an unknown language sorts last.
fn language_rank(language: &str) -> usize {
    crate::semantic_evidence::language_order(language).unwrap_or(LANGUAGES.len())
}

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

/// `semanticStatus` (#660): each of the answer's languages whose evidence is still catching
/// up or needs re-import, in #22 order, from the same committed per-language state describe
/// reports. `[]` when none is pending.
fn semantic_status<'a>(tiers: &Tiers, languages: impl IntoIterator<Item = &'a str>) -> Value {
    use crate::model::SemanticStatus;
    let languages: std::collections::BTreeSet<&str> = languages.into_iter().collect();
    Value::Array(
        LANGUAGES
            .into_iter()
            .zip(tiers)
            .filter(|(language, _)| languages.contains(language))
            .filter_map(|(language, tier)| {
                let pending = tier.pending.as_ref()?;
                let state = match pending.status {
                    SemanticStatus::NeedsReimport => "needsReimport",
                    SemanticStatus::CatchingUp => "catchingUp",
                    SemanticStatus::OlderBasis => return None,
                };
                Some(json!({"language":language,"state":state,"reason":pending.reason}))
            })
            .collect(),
    )
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
            || tiers.iter().any(|tier| {
                tier.documents
                    .iter()
                    .any(|status| status.projection.semantic_status.is_pending())
            })
    });
    let languages: Vec<Value> = counts.as_ref().map_or_else(Vec::new, |(rows, tiers)| {
        describe_language_status(rows, tiers)
    });
    let deadline_ms = u64::try_from(super::CALL_DEADLINE.as_millis()).unwrap_or(u64::MAX);
    json!({"schemaVersion":1,"requestId":id,"evidenceBasis":basis,
      "data":{"workspaceLabel":label,"indexState":state,"progress":progress,
      "watcherDegraded":degraded,"languages":languages,"toolVersion":30,"schemaVersion":1,
      "searchIndex":crate::search_model::SearchAvailability::default(),
      // #851: the head's apps, when its architecture is read; the call fills them in.
      "apps":{"state":"unavailable","items":[],"noApp":null},
      "limits":{"requestBytes":16384,"responseBytes":65536,"sourceBytes":16384,
        "sourceLines":200,"defaultLimit":20,"callDefaultLimit":super::CALL_DEFAULT_LIMIT,
        "maxLimit":50,"pageBytes":super::architecture::PAGE_BYTES,"calleeTextBytes":CALLEE_TEXT_BYTES,
        "hintWorkMs":1000,"deadlineMs":deadline_ms,"concurrency":super::MAX_CONNECTION_CALLS}},
      "warnings":[],"partial":partial,"truncated":false,"truncationReason":null})
}

/// Describe's page of failed and partial semantic documents (#780): from the sorted
/// `documents`, the items at `offset`, at most the request's `limit` (default 20), such
/// that the complete JSON-RPC response line (envelope, summary, `workspace`, `catchingUp`,
/// `searchIndex` and LF) stays inside the architecture page target; a page always holds
/// its first document. Run on the final envelope, once every other field is attached.
/// `page` continues it as architecture paging does; an offset past the end fails
/// `range_too_large` with the admitted basis.
pub fn coverage_documents(
    id: &Value,
    mut envelope: Value,
    documents: Vec<crate::store::CoverageDocument>,
    request: &Value,
    modern: bool,
) -> Value {
    let uint = |field: &str| request.get(field).and_then(super::wire::uint);
    let offset = uint("offset").unwrap_or(0);
    let limit = uint("limit").map_or(COVERAGE_DOCUMENTS_DEFAULT, |limit| limit as usize);
    let total = documents.len() as u64;
    if offset > total {
        let mut failure = super::tools::failure(id, "range_too_large");
        failure["error"]["currentBasis"] = envelope["evidenceBasis"].clone();
        for field in ["workspace", "catchingUp"] {
            if let Some(value) = envelope.get(field) {
                failure[field] = value.clone();
            }
        }
        return failure;
    }
    let items: Vec<Value> = documents
        .into_iter()
        .skip(offset as usize)
        .take(limit)
        .map(|document| {
            json!({"language":document.language,"path":document.path,
                "state":if document.failed { "failed" } else { "partial" },
                "reasons":document.reasons})
        })
        .collect();
    let page = |shown: usize| {
        let mut envelope = envelope.clone();
        let next = offset + shown as u64;
        envelope["data"]["coverageDocuments"] = json!({"items":items[..shown],
            "page":{"offset":offset,"nextOffset":(next < total).then_some(next),"total":total}});
        envelope
    };
    // Each added document grows the line by far more than a shorter `nextOffset` saves.
    let fits =
        |shown: usize| line_bytes(id, &page(shown), modern) <= super::architecture::PAGE_BYTES;
    let mut shown = items.len().min(1);
    while shown < items.len() && fits(shown + 1) {
        shown += 1;
    }
    envelope = page(shown);
    envelope
}

/// The bytes of describe's complete JSON-RPC response line for `envelope`, LF included.
pub fn line_bytes(id: &Value, envelope: &Value, modern: bool) -> usize {
    let result = super::tools::result("trellis_workspace_describe", envelope.clone(), modern);
    serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"result":result}))
        .map_or(usize::MAX, |line| line.len() + 1)
}

/// Documents on one describe coverage page without a `limit` (#780).
pub const COVERAGE_DOCUMENTS_DEFAULT: usize = 20;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

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
        "data":{"stableLookupKey":source.stable_lookup_key,"displayKey":crate::references::display_key(&result.declaration),"declaration":compact,"doc":doc,"lines":source.lines.map(|lines| json!({"start":lines.start,"end":lines.end})),"documents":documents,
            "evidence":{"evidenceTier":"syntax","semanticBasis":null,"freshness":"fresh","staleBecause":[]},
            "coverage":result.coverage,"provenance":result.provenance},
        "warnings":warnings,"partial":incomplete,"truncated":false,"truncationReason":null,
        "semanticStatus":semantic_status(tiers, [result.declaration.document.language.as_str()])});
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
    compact: bool,
) -> Result<Option<Value>, &'static str> {
    symbols(
        id,
        result,
        tiers,
        root,
        catching_up,
        modern,
        SymbolMode::Find,
        compact,
    )
}

/// Exact resolution never emits a candidate prefix or uses the ordinary find limit.
pub fn find_lookup(
    id: &Value,
    result: crate::store::mcp_read::McpFindWithLines,
    tiers: &Tiers,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
    compact: bool,
) -> Result<Option<Value>, &'static str> {
    symbols(
        id,
        result,
        tiers,
        root,
        catching_up,
        modern,
        SymbolMode::ExactFind,
        compact,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn lookup_candidates(
    id: &Value,
    name: &str,
    key: &str,
    result: crate::store::mcp_read::McpFindWithLines,
    tiers: &Tiers,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Result<Option<Value>, &'static str> {
    symbols(
        id,
        result,
        tiers,
        root,
        catching_up,
        modern,
        SymbolMode::Candidates(name, key),
        false,
    )
}

#[derive(Clone, Copy)]
enum SymbolMode<'a> {
    Find,
    ExactFind,
    Candidates(&'a str, &'a str),
    Search(&'a [Vec<String>], &'a Value, crate::search_rank::SearchRole),
}

/// Search relevance decorates syntax evidence; it never changes native completeness.
pub fn search(
    id: &Value,
    result: super::search::SearchResult,
    tiers: &Tiers,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
    compact: bool,
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
        SymbolMode::Search(&result.matched_by, &status, result.role),
        compact,
    )
}

/// #715: each distinct coverage diagnostic once, in first-row order, with each row's
/// `diagnostic` replaced by `diagnosticRef` (null without one). Re-inlining the table gives
/// back the native coverage rows.
fn coverage_diagnostics(
    rows: &[&crate::native_evidence::Coverage],
) -> Result<(Vec<Value>, Vec<String>), &'static str> {
    let mut diagnostics: Vec<String> = Vec::new();
    let rows = rows
        .iter()
        .map(|row| {
            let mut value =
                serde_json::to_value(row).map_err(|_| "store_unavailable: coverage projection")?;
            let fields = value
                .as_object_mut()
                .ok_or("store_unavailable: coverage projection")?;
            fields.remove("diagnostic");
            let reference = row.diagnostic.as_ref().map(|diagnostic| {
                diagnostics
                    .iter()
                    .position(|seen| seen == diagnostic)
                    .unwrap_or_else(|| {
                        diagnostics.push(diagnostic.clone());
                        diagnostics.len() - 1
                    })
            });
            fields.insert("diagnosticRef".into(), json!(reference));
            Ok(value)
        })
        .collect::<Result<_, &'static str>>()?;
    Ok((rows, diagnostics))
}

/// Budget the actual structured-plus-summary JSON-RPC frame, not the structured envelope alone.
#[allow(clippy::too_many_arguments)]
fn symbols(
    id: &Value,
    result: crate::store::mcp_read::McpFindWithLines,
    tiers: &Tiers,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
    mode: SymbolMode<'_>,
    compact: bool,
) -> Result<Option<Value>, &'static str> {
    let search = match mode {
        SymbolMode::Search(matched_by, status, role) => Some((matched_by, status, role)),
        _ => None,
    };
    let atomic = matches!(mode, SymbolMode::ExactFind | SymbolMode::Candidates(..));
    if atomic && result.has_more {
        return Err("store_unavailable: incomplete lookup resolution");
    }
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
    let languages_for = |n: usize| -> Vec<&str> {
        if n == 0 {
            scope_languages.clone()
        } else {
            result.symbols[..n]
                .iter()
                .map(|symbol| symbol.symbol.declaration.document.language.as_str())
                .collect()
        }
    };
    let warnings_for = |n: usize| -> Vec<Value> {
        let mut warnings = Vec::new();
        if incomplete {
            warnings.push(json!({"code":"coverageIncomplete","message":"Selected syntax coverage is incomplete","provenanceId":null}));
        }
        warnings.extend(syntax_only(tiers, languages_for(n)));
        warnings
    };
    // #715: lookup alternatives for the other tools keep the contract shape; find and
    // search trim the scope rows, share coverage diagnostics and take `compact`.
    let alternatives = matches!(mode, SymbolMode::Candidates(..));
    let compact = compact && !alternatives;
    let scope = language_status(&result.scope, tiers);
    let basis = json!({"indexGeneration":result.pin.index_generation.to_string(),
        "indexRevision":result.pin.index_revision});
    let mut last = None;
    // The empty prefix carries only the fixed envelope and the scope rows; if
    // neither it nor any hit prefix fits, the answer is range_too_large.
    // #715: the empty prefix lists every searched language while a hit prefix
    // lists only its own, so the empty frame is no lower bound on a hit frame:
    // it may overflow where one hit fits. From one hit on, each longer prefix
    // adds items and never drops a scope row, so its frame only grows.
    let start = if atomic { result.symbols.len() } else { 0 };
    for n in start..=result.symbols.len() {
        let (documents, refs) = document_table(
            result.symbols[..n]
                .iter()
                .map(|symbol| &symbol.source.document),
        )?;
        let mut symbols: Vec<Value> = result.symbols[..n].iter().map(|symbol| {
            let d = &symbol.symbol.declaration;
            let lines = symbol.source.lines.map(|lines| json!({"start":lines.start,"end":lines.end}));
            let doc = document_ref(&refs, &symbol.source.document).ok_or("store_unavailable: missing document ref")?;
            // #715 compact: the follow-up handles only; the path replaces the document ref.
            Ok(if compact {
                json!({"symbolId":d.syntax_id,"name":d.name,"stableLookupKey":symbol.source.stable_lookup_key,
                    "kind":d.kind,"role":d.role,"path":d.document.path,"lines":lines,"evidenceTier":"syntax"})
            } else {
                json!({"symbolId":d.syntax_id,"name":d.name,"displayKey":crate::references::display_key(d),"stableLookupKey":symbol.source.stable_lookup_key,
                    "kind":d.kind,"role":d.role,"doc":doc,"range":d.range,"nameRange":d.name_range,"lines":lines,
                    "evidenceTier":"syntax","semanticBasis":null,"freshness":"fresh","staleBecause":[]})
            })
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
                language_rank(&a.language),
                &a.source_set_id,
                &a.document_path,
                &a.revision_id,
            )
                .cmp(&(
                    &b.producer_id,
                    language_rank(&b.language),
                    &b.source_set_id,
                    &b.document_path,
                    &b.revision_id,
                ))
        });
        let truncated = n < result.symbols.len() || result.has_more;
        let data = if alternatives {
            json!({"symbols":symbols,"documents":documents,"coverage":coverage,"scopeCoverage":scope,"provenance":closure})
        } else {
            // #715: only the scope rows of the emitted items' languages (with none, the
            // searched languages), in #22 order. Completeness still covers every row.
            let languages = languages_for(n);
            let scope: Vec<&Value> = LANGUAGES
                .iter()
                .zip(&scope)
                .filter(|(language, _)| languages.contains(language))
                .map(|(_, row)| row)
                .collect();
            if compact {
                json!({"symbols":symbols,"scopeCoverage":scope})
            } else {
                let (coverage, diagnostics) = coverage_diagnostics(&coverage)?;
                json!({"symbols":symbols,"documents":documents,"coverage":coverage,
                    "coverageDiagnostics":diagnostics,"scopeCoverage":scope,"provenance":closure})
            }
        };
        let mut envelope = json!({"schemaVersion":1,"requestId":id,"workspace":root,
            "catchingUp":catching_up,"evidenceBasis":basis,"data":data,
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
        } else if let SymbolMode::Candidates(name, key) = mode {
            let data = envelope["data"].as_object_mut().expect("symbol data");
            let candidates = data.remove("symbols").expect("symbol list");
            data.insert("candidates".into(), candidates);
            data.insert("lookupKey".into(), json!(key));
            name
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
        } else if n > 0 {
            break;
        }
    }
    Ok(last)
}

#[cfg(test)]
mod find_tests;

#[cfg(test)]
mod search_tests;

#[cfg(test)]
mod compact_symbol_tests;

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
    historical_freshness: Value,
    projection: Value,
}

fn call_evidence(
    semantic: Option<&crate::store::CallSemantic>,
    projection: Option<&crate::model::SemanticProjection>,
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
            freshness: if projection.is_some() {
                json!("possiblyStale")
            } else {
                json!("fresh")
            },
            historical_freshness: Value::Null,
            projection: json!(projection),
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
        freshness: value(serde_json::to_value(semantic.effective_freshness))?,
        historical_freshness: value(serde_json::to_value(semantic.historical_freshness))?,
        projection: value(serde_json::to_value(&semantic.projection))?,
    })
}

fn call_projection<'a>(
    semantic: &'a crate::store::McpSemantic,
    document: &crate::native_evidence::DocumentKey,
) -> Option<&'a crate::model::SemanticProjection> {
    semantic
        .documents
        .iter()
        .find(|status| status.language == document.language && status.path == document.path)
        .map(|status| &status.projection)
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
fn intern_bases<P: serde::Serialize>(
    items: &mut [Value],
    provenance: impl IntoIterator<Item = P>,
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

/// Keep each historical proof labelled even when detached from its call item.
fn label_projected_proofs(
    provenance: &mut [Value],
    semantic: &crate::store::McpSemantic,
    count: usize,
) -> Result<(), &'static str> {
    let mut labels = std::collections::BTreeMap::new();
    for call in (0..count).filter_map(|index| semantic.call(index)) {
        let Some(projection) = &call.projection else {
            continue;
        };
        for proof in &call.proofs {
            let label = json!({"projection":projection,"historicalFreshness":proof.freshness,
                "effectiveFreshness":projection.effective_freshness(proof.freshness)});
            if let Some(previous) = labels.insert(proof.id.as_str(), label.clone())
                && previous != label
            {
                return Err("store_unavailable: conflicting projected proof");
            }
        }
    }
    for proof in provenance {
        if let Some(label) = proof["id"].as_str().and_then(|id| labels.get(id)) {
            let fields = label
                .as_object()
                .ok_or("store_unavailable: projected proof label")?;
            proof
                .as_object_mut()
                .ok_or("store_unavailable: projected proof")?
                .extend(fields.clone());
        }
    }
    Ok(())
}

/// #528 compact mode: keep the answer (items, refs, dispositions, targets, warnings and
/// completeness) and drop the proof detail: `coverage`, `provenance`, `semanticBases`,
/// and each item's `binding` and `semanticBasisRef`. Projection and historical freshness
/// stay on each item; `semanticDocuments` keeps pending selections without bindings.
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
            match call
                .projection
                .as_ref()
                .map_or(proof.freshness, |projection| {
                    projection.effective_freshness(proof.freshness)
                }) {
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
#[allow(clippy::too_many_arguments)]
pub fn outgoing(
    id: &Value,
    result: crate::store::mcp_read::McpOutgoingWithLines,
    semantic: &crate::store::McpSemantic,
    tiers: &Tiers,
    compact: bool,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Result<Option<Value>, &'static str> {
    use std::collections::BTreeMap;
    let owner_display = crate::references::display_key(&result.owner.selected.declaration);
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
        let languages: Vec<&str> =
            std::iter::once(result.owner.selected.declaration.document.language.as_str())
                .chain(
                    documents
                        .iter()
                        .filter_map(|document| document["document"]["language"].as_str()),
                )
                .collect();
        let semantic_status = semantic_status(tiers, languages);
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
            let evidence = call_evidence(bound, call_projection(semantic, &call.document))?;
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
                "freshness":evidence.freshness,"staleBecause":[],
                "historicalFreshness":evidence.historical_freshness,"projection":evidence.projection}));
        }
        let mut coverage: Vec<_> = coverage.into_values().collect();
        coverage.sort_by(|a, b| {
            (
                &a.producer_id,
                language_rank(&a.language),
                &a.source_set_id,
                &a.document_path,
                &a.revision_id,
            )
                .cmp(&(
                    &b.producer_id,
                    language_rank(&b.language),
                    &b.source_set_id,
                    &b.document_path,
                    &b.revision_id,
                ))
        });
        let incomplete = coverage.iter().any(|c| c.selected && c.state != "complete")
            || semantic
                .documents
                .iter()
                .any(|status| status.projection.semantic_status.is_pending());
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
        let (bases, mut provenance) = intern_bases(&mut calls, provenance.into_values())?;
        label_projected_proofs(&mut provenance, semantic, count)?;
        let mut data = json!({"symbolId":result.owner.selected.declaration.syntax_id,
            "stableLookupKey":result.owner.source.stable_lookup_key,"displayKey":owner_display,"calls":calls,"documents":documents,
            "semanticBases":bases,"coverage":coverage,"provenance":provenance,
            "semanticDocuments":semantic.documents});
        if let Some(omitted) = result.external_omitted {
            data["externalOmitted"] = json!(omitted);
        }
        if compacted {
            compact_data(&mut data, "calls");
        }
        Ok(
            json!({"schemaVersion":1,"requestId":id,"workspace":root,"catchingUp":catching_up,
            "evidenceBasis":basis,"data":data,
            "warnings":warnings,"partial":incomplete || truncated || boundary,
            "truncated":truncated,"truncationReason":reason,"semanticStatus":semantic_status}),
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
/// values), `truncationReason: "response_bytes"` and `data.responseOmitted`, the nodes and
/// edge groups of the bounded answer left out. `None` means even `k = 0` is too large.
///
/// #665: each distinct semantic basis is emitted once in `data.semanticBases` and proofs
/// carry `basisRef`, as outgoing and incoming calls do. `compact` drops the proof detail
/// (see [`compact_graph`]) and is fitted on its own, smaller frame.
#[allow(clippy::too_many_arguments)]
pub fn call_graph(
    id: &Value,
    answer: &mut crate::graph_query::GraphAnswer,
    stable_lookup_key: Option<&str>,
    node_keys: &std::collections::BTreeMap<String, String>,
    tiers: &Tiers,
    compact: bool,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Result<Option<Value>, &'static str> {
    let Ok(full) = &answer.outcome else {
        return Err("store_unavailable: call graph answer is a contract failure");
    };
    let edges = full.edges.len();
    let nodes = full.nodes.len();
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
            let mut projected = serde_json::to_value(result)
                .map_err(|_| "store_unavailable: call graph projection")?;
            let Value::Array(rows) = projected["provenance"].take() else {
                return Err("store_unavailable: call graph provenance projection");
            };
            let mut rows = rows;
            let ordinary_count = rows.len();
            for proof in projected["projectedProvenance"]
                .as_array()
                .into_iter()
                .flatten()
            {
                rows.push(proof["evidence"].clone());
            }
            let (bases, mut provenance) = intern_bases(&mut [], rows)?;
            let historical = provenance.split_off(ordinary_count);
            projected["provenance"] = Value::Array(provenance);
            for (proof, evidence) in projected
                .get_mut("projectedProvenance")
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
                .zip(historical)
            {
                proof["evidence"] = evidence;
            }
            clip_edge_spellings(&mut projected);
            let omitted = json!({"nodes":nodes - result.nodes.len(),
                "edges":edges - result.edges.len()});
            let mut data = json!({"stableLookupKey":stable_lookup_key,"requested":requested,
                "result":projected,"semanticBases":bases,"responseOmitted":omitted});
            if compact {
                compact_graph(&mut data, node_keys);
            }
            let semantic_status = semantic_status(
                tiers,
                result
                    .nodes
                    .iter()
                    .map(|node| node.declaration.document.language.as_str()),
            );
            Ok(
                json!({"schemaVersion":1,"requestId":id,"workspace":root,"catchingUp":catching_up,
            "evidenceBasis":basis,"data":data,
            "warnings":result.warnings,"partial":result.partial,"truncated":result.truncated,
            "truncationReason":reason,"semanticStatus":semantic_status}),
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

/// #724: clip each edge call's `spelling` to `calleeTextBytes` (1024 UTF-8 bytes) on a
/// scalar boundary, as outgoing calls clip `calleeText`, and mark a clipped call with
/// `spellingTruncated:true`. A spelling that fits is unchanged and carries no flag.
fn clip_edge_spellings(result: &mut Value) {
    for edge in result["edges"].as_array_mut().into_iter().flatten() {
        let Some(call) = edge["call"].as_object_mut() else {
            continue;
        };
        let Some(Value::String(spelling)) = call.get_mut("spelling") else {
            continue;
        };
        if spelling.len() <= CALLEE_TEXT_BYTES {
            continue;
        }
        let end = (0..=CALLEE_TEXT_BYTES)
            .rev()
            .find(|&end| spelling.is_char_boundary(end))
            .unwrap_or(0);
        spelling.truncate(end);
        call.insert("spellingTruncated".into(), Value::Bool(true));
    }
}

/// The advertised `calleeTextBytes` cap on exported callee text.
const CALLEE_TEXT_BYTES: usize = 1024;

/// #665 compact graph: keep the graph (node keys, names, kinds, roles, paths and ranges; edge
/// endpoints, call sites, visits, boundaries and binding resolutions), frontiers, omissions,
/// hints, warnings and completeness, and drop the proof detail: `semanticBases`, the
/// result's `coverage`, `provenance` and `projectedProvenance`, each edge's `binding`
/// (replaced by its `resolution`, null without one), each node declaration's header, ancestors, key, name
/// range, revision and proof, and each call's document, revision, regions and proof.
/// #715: each node declaration adds its `stableLookupKey` from `node_keys` (null without
/// one), the durable handle for follow-up calls.
fn compact_graph(data: &mut Value, node_keys: &std::collections::BTreeMap<String, String>) {
    if let Some(fields) = data.as_object_mut() {
        fields.remove("semanticBases");
    }
    let result = &mut data["result"];
    if let Some(fields) = result.as_object_mut() {
        fields.remove("coverage");
        fields.remove("provenance");
        fields.remove("projectedProvenance");
    }
    for node in result["nodes"].as_array_mut().into_iter().flatten() {
        if let Some(declaration) = node["declaration"].as_object_mut() {
            for field in [
                "ancestors",
                "key",
                "nameRange",
                "header",
                "revisionId",
                "provenanceId",
            ] {
                declaration.remove(field);
            }
            let key = declaration
                .get("syntaxId")
                .and_then(Value::as_str)
                .and_then(|sid| node_keys.get(sid));
            declaration.insert("stableLookupKey".into(), json!(key));
        }
    }
    for edge in result["edges"].as_array_mut().into_iter().flatten() {
        if let Some(call) = edge["call"].as_object_mut() {
            for field in ["document", "revisionId", "regionIds", "provenanceId"] {
                call.remove(field);
            }
        }
        if let Some(fields) = edge.as_object_mut() {
            let resolution = fields
                .remove("binding")
                .and_then(|binding| binding.get("resolution").cloned())
                .unwrap_or(Value::Null);
            fields.insert("resolution".into(), resolution);
        }
    }
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
            let evidence = call_evidence(bound, call_projection(semantic, &call.document))?;
            callers.push(
                json!({"callId":call.id,"ownerSyntaxId":call.owner_syntax_id,
                "callerDisplayKey":crate::references::display_key(&item.caller),"callerRole":item.caller.role,
                "doc":document_ref(&refs, version).ok_or("store_unavailable: missing caller ref")?,
                "lines":item.source.lines.map(|lines| json!({"start":lines.start,"end":lines.end})),
                "range":call.range,"calleeRange":callee_range,"calleeText":item.callee_text,
                "matchedBy":"name","binding":evidence.binding,"disposition":evidence.disposition,
                "targetId":evidence.target_id,"boundaryReason":evidence.boundary_reason,
                "evidenceTier":evidence.evidence_tier,"semanticBasis":evidence.semantic_basis,
                "freshness":evidence.freshness,"staleBecause":[],
                "historicalFreshness":evidence.historical_freshness,"projection":evidence.projection}),
            );
        }
        let mut coverage: Vec<_> = coverage.into_values().collect();
        coverage.sort_by(|a, b| {
            (
                &a.producer_id,
                language_rank(&a.language),
                &a.source_set_id,
                &a.document_path,
                &a.revision_id,
            )
                .cmp(&(
                    &b.producer_id,
                    language_rank(&b.language),
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
            .is_some_and(|warning| warning["code"] == "coverageIncomplete")
            || semantic
                .documents
                .iter()
                .any(|status| status.projection.semantic_status.is_pending());
        let (bases, mut provenance) = intern_bases(&mut callers, provenance.into_values())?;
        label_projected_proofs(&mut provenance, semantic, count)?;
        let semantic_status = semantic_status(
            tiers,
            std::iter::once(owner.declaration.document.language.as_str()).chain(
                documents
                    .iter()
                    .filter_map(|document| document["document"]["language"].as_str()),
            ),
        );
        let mut data = json!({
            "symbolId":owner.declaration.syntax_id,"displayKey":crate::references::display_key(&owner.declaration),
            "stableLookupKey":result.selected.source.stable_lookup_key,
            "match":{"basis":"name","sameNameDeclarations":result.same_name_declarations,
                "limitations":["aliases","functionValues","reExports","dynamicDispatch",
                    "opaqueMacros","sameNameDeclarations"]},
            "callers":callers,"documents":documents,"semanticBases":bases,"coverage":coverage,
            "scopeCoverage":language_status(&result.scope, tiers),
            "provenance":provenance,"semanticDocuments":semantic.documents});
        // #724: callers excluded because their fresh, direct binding targets another
        // declaration. Present only when some were excluded.
        if result.bound_elsewhere_omitted > 0 {
            data["boundElsewhereOmitted"] = json!(result.bound_elsewhere_omitted);
        }
        if compact {
            compact_data(&mut data, "callers");
        }
        Ok(
            json!({"schemaVersion":1,"requestId":id,"workspace":root,"catchingUp":catching_up,
            "evidenceBasis":basis,"data":data,
            "warnings":warnings,"partial":incomplete || truncated || boundary,
            "truncated":truncated,"truncationReason":reason,"semanticStatus":semantic_status}),
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
mod outgoing_tests;

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
mod incoming_tests;

#[cfg(test)]
mod source_tests;

#[cfg(test)]
mod call_graph_tests;

/// #528 response-size regressions on this repository's real shapes: the owner's live
/// `incoming_calls` on `index_workspace` returned ~62 KB and stopped at 12 callers because
/// each semantic caller repeated an ~800-byte basis in the item and again in its proof.
#[cfg(test)]
mod size_tests;
