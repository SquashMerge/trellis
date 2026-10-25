//! Exact measured joins for authenticated, document-local SCIP claims.
use crate::{
    native_evidence as native, native_ids,
    scip_raw::{CapturedClaim, CapturedScipInventory},
    semantic_evidence as semantic,
};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

type AnchorKey = (native::DocumentKey, usize, usize, semantic::JoinKind);
type ScopedKey = (String, Option<native::DocumentKey>);
type LocalDeclarations = BTreeMap<ScopedKey, BTreeSet<(native::DocumentKey, String)>>;

/// Indexed native candidates are identities, not counts of raw producer claims.
pub struct MeasuredFacts<'a> {
    native: &'a native::Artifact,
    sources: &'a BTreeMap<native::DocumentKey, Vec<u8>>,
    candidates: BTreeMap<AnchorKey, BTreeSet<String>>,
}

impl<'a> MeasuredFacts<'a> {
    pub fn new(
        native: &'a native::Artifact,
        sources: &'a BTreeMap<native::DocumentKey, Vec<u8>>,
    ) -> Result<Self> {
        let mut candidates: BTreeMap<AnchorKey, BTreeSet<String>> = BTreeMap::new();
        for doc in &native.revision.documents {
            let bytes = sources
                .get(&doc.key)
                .context("missing authenticated native source")?;
            ensure!(
                bytes.len() == doc.byte_length
                    && native_ids::digest(b"", bytes) == doc.content_hash,
                "native source hash mismatch"
            );
            std::str::from_utf8(bytes).context("non-UTF8 native source")?;
        }
        let mut insert = |doc: &native::DocumentKey, range: &native::Range, kind, id: &str| {
            candidates
                .entry((doc.clone(), range.start, range.end, kind))
                .or_default()
                .insert(id.to_owned());
        };
        for d in &native.declarations {
            if let Some(r) = &d.name_range {
                insert(
                    &d.document,
                    r,
                    semantic::JoinKind::DeclarationName,
                    &d.syntax_id,
                );
            }
        }
        for c in &native.calls {
            if let Some(r) = &c.callee_range {
                insert(&c.document, r, semantic::JoinKind::Callee, &c.id);
            }
            insert(&c.document, &c.range, semantic::JoinKind::Invocation, &c.id);
        }
        for r in &native.references {
            insert(&r.document, &r.range, semantic::JoinKind::Reference, &r.id);
        }
        Ok(Self {
            native,
            sources,
            candidates,
        })
    }

    pub fn join(
        &self,
        document: &native::DocumentKey,
        range: native::Range,
        kind: semantic::JoinKind,
    ) -> Result<semantic::Join> {
        let doc = self
            .native
            .revision
            .documents
            .iter()
            .find(|d| &d.key == document)
            .context("foreign anchor document")?;
        let bytes = self.sources.get(document).context("missing anchor bytes")?;
        let source = std::str::from_utf8(bytes)?;
        let spelling = source
            .get(range.start..range.end)
            .filter(|s| !s.is_empty())
            .context("invalid UTF-8 scalar anchor")?;
        let ids = self
            .candidates
            .get(&(document.clone(), range.start, range.end, kind))
            .into_iter()
            .flat_map(|ids| ids.iter())
            .filter(|id| match kind {
                semantic::JoinKind::DeclarationName => self.native.declarations.iter().any(|d| {
                    &d.document == document
                        && &d.syntax_id == *id
                        && d.name.as_deref() == Some(spelling)
                }),
                semantic::JoinKind::Callee => self.native.calls.iter().any(|c| {
                    &c.document == document
                        && &c.id == *id
                        && c.spelling.as_deref() == Some(spelling)
                }),
                semantic::JoinKind::Invocation => self.native.calls.iter().any(|c| {
                    &c.document == document
                        && &c.id == *id
                        && c.range.start == range.start
                        && c.range.end == range.end
                }),
                semantic::JoinKind::Reference => self.native.references.iter().any(|r| {
                    &r.document == document
                        && &r.id == *id
                        && r.spelling == spelling
                        && native::lookup(&document.language, spelling).ok().as_deref()
                            == Some(&r.lookup_key)
                }),
            })
            .cloned()
            .collect::<Vec<_>>();
        let support = self
            .native
            .capability_reports
            .iter()
            .find(|r| &r.document == document && r.revision_id == self.native.revision.id)
            .context("missing native capability report")?
            .families
            .iter()
            .find(|r| {
                r.kind
                    == match kind {
                        semantic::JoinKind::DeclarationName => {
                            native::MeasurementKind::DeclarationName
                        }
                        semantic::JoinKind::Callee => native::MeasurementKind::Callee,
                        semantic::JoinKind::Invocation => native::MeasurementKind::Invocation,
                        semantic::JoinKind::Reference => native::MeasurementKind::Reference,
                    }
            })
            .context("missing native measurement family")?;
        ensure!(
            support.available || ids.is_empty() && support.gaps.is_empty(),
            "unavailable native family has candidates or gaps"
        );
        for gap in &support.gaps {
            ensure!(
                gap.range.start < gap.range.end
                    && source.get(gap.range.start..gap.range.end).is_some(),
                "invalid native capability gap"
            );
            ensure!(
                !self
                    .candidates
                    .iter()
                    .any(|((key, start, end, family), ids)| key == document
                        && *family == kind
                        && !ids.is_empty()
                        && gap.range.start <= *start
                        && *end <= gap.range.end),
                "native capability gap contradicts measured candidates"
            );
        }
        let unavailable = if !support.available {
            Some(
                support
                    .diagnostic
                    .as_deref()
                    .context("missing capability diagnostic")?,
            )
        } else {
            support
                .gaps
                .iter()
                .find(|g| g.range.start <= range.start && range.end <= g.range.end)
                .map(|g| g.diagnostic.as_str())
        };
        let (status, diagnostic) = if ids.len() == 1 {
            (semantic::JoinStatus::Exact, None)
        } else if ids.len() > 1 {
            (
                semantic::JoinStatus::Ambiguous,
                Some("ambiguous_measured_anchor".to_owned()),
            )
        } else if let Some(reason) = unavailable {
            (semantic::JoinStatus::Unsupported, Some(reason.to_owned()))
        } else {
            (
                semantic::JoinStatus::Unmatched,
                Some("unmatched_measured_anchor".to_owned()),
            )
        };
        Ok(semantic::Join {
            anchor: semantic::MeasuredAnchor {
                document: document.clone(),
                revision_id: self.native.revision.id.clone(),
                content_hash: doc.content_hash.clone(),
                range,
                kind,
            },
            status,
            candidate_ids: ids,
            diagnostic,
        })
    }
}

fn point(source: &str, line: u64, column: u64, encoding: u8) -> Result<usize> {
    let mut start = 0;
    for _ in 0..line {
        let next = source[start..]
            .find('\n')
            .context("SCIP line outside source")?;
        start += next + 1;
    }
    let rest = &source[start..];
    let end = rest.find('\n').unwrap_or(rest.len());
    let line = rest[..end].strip_suffix('\r').unwrap_or(&rest[..end]);
    let mut width = 0u64;
    for (offset, c) in line.char_indices() {
        if width == column {
            return Ok(start + offset);
        }
        width += match encoding {
            1 => c.len_utf8() as u64,
            2 => c.len_utf16() as u64,
            3 => 1,
            _ => anyhow::bail!("invalid SCIP position encoding"),
        };
        ensure!(width <= column, "SCIP column splits scalar");
    }
    ensure!(width == column, "SCIP column outside source");
    Ok(start + line.len())
}

/// Convert the admitted raw numeric range using authenticated source bytes only.
pub fn occurrence_range(
    claim: &CapturedClaim,
    source: &[u8],
    admitted_encoding: u8,
) -> Result<native::Range> {
    ensure!(
        claim.envelope.record_kind == "occurrence",
        "not an occurrence"
    );
    let raw = &claim.envelope.raw["range"];
    let values: Vec<u64> = match raw["encoding"]
        .as_str()
        .context("missing raw range encoding")?
    {
        "legacy" => raw["values"]
            .as_array()
            .context("missing legacy values")?
            .iter()
            .map(|v| v.as_u64().context("invalid legacy value"))
            .collect::<Result<_>>()?,
        "singleLine" => vec![
            raw["line"].as_u64().context("invalid line")?,
            raw["startCharacter"].as_u64().context("invalid start")?,
            raw["endCharacter"].as_u64().context("invalid end")?,
        ],
        "multiLine" => vec![
            raw["startLine"].as_u64().context("invalid start line")?,
            raw["startCharacter"].as_u64().context("invalid start")?,
            raw["endLine"].as_u64().context("invalid end line")?,
            raw["endCharacter"].as_u64().context("invalid end")?,
        ],
        _ => anyhow::bail!("invalid raw range shape"),
    };
    ensure!(matches!(values.len(), 3 | 4), "invalid raw range length");
    let encoding = if claim.envelope.position_encoding == 0 {
        admitted_encoding
    } else {
        claim.envelope.position_encoding
    };
    ensure!(
        matches!(encoding, 1..=3) && encoding == admitted_encoding,
        "SCIP encoding differs from admission"
    );
    let text = std::str::from_utf8(source).context("non-UTF8 authenticated source")?;
    let start = point(text, values[0], values[1], encoding)?;
    let end = if values.len() == 3 {
        point(text, values[0], values[2], encoding)?
    } else {
        point(text, values[2], values[3], encoding)?
    };
    ensure!(start < end, "empty or reversed SCIP range");
    Ok(native::Range { start, end })
}

fn scoped_key(
    symbol: &str,
    document: &native::DocumentKey,
) -> (String, Option<native::DocumentKey>) {
    (
        symbol.to_owned(),
        symbol.starts_with("local ").then(|| document.clone()),
    )
}

fn symbol_key(symbol: &str, document: &native::DocumentKey) -> semantic::SymbolKey {
    let local = symbol.starts_with("local ");
    semantic::SymbolKey {
        scheme: semantic::SymbolScheme::Scip,
        symbol: symbol.into(),
        scope: if local {
            semantic::SymbolScope::Document
        } else {
            semantic::SymbolScope::Global
        },
        document: local.then(|| document.clone()),
    }
}

#[derive(Debug, Default)]
pub struct JoinedFacts {
    pub symbols: Vec<semantic::Symbol>,
    pub declaration_bindings: Vec<semantic::DeclarationBinding>,
    pub references: Vec<semantic::Reference>,
    pub call_bindings: Vec<semantic::CallBinding>,
    pub type_relationships: Vec<semantic::TypeRelationship>,
    pub joins: Vec<(String, semantic::Join)>,
    pub diagnostics: BTreeMap<native::DocumentKey, BTreeSet<String>>,
}

/// Classify admitted occurrence claims against a measured native snapshot. The caller owns
/// captured proof persistence and publication; this function never consults mutable files.
pub fn join_inventory(
    inventory: &CapturedScipInventory,
    native: &native::Artifact,
    sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    admitted_encoding: u8,
) -> Result<JoinedFacts> {
    let measured = MeasuredFacts::new(native, sources)?;
    let mut out = JoinedFacts::default();
    let mut declarations: LocalDeclarations = BTreeMap::new();
    let mut type_uses: BTreeMap<(native::DocumentKey, usize, usize), BTreeSet<String>> =
        BTreeMap::new();
    let mut occurrences = Vec::new();
    for claim in &inventory.claims {
        inventory.lookup(
            &claim.captured_proof_id,
            &claim.envelope.record_kind,
            &claim.fact_digest,
        )?;
        let doc = native::DocumentKey {
            source_set_id: native.revision.source_set_id.clone(),
            language: claim.language.clone(),
            path: claim.envelope.relative_path.clone(),
        };
        let source = sources
            .get(&doc)
            .context("missing admitted claim document")?;
        if claim.envelope.record_kind == "occurrence" {
            let symbol = claim.envelope.raw["symbol"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("missing occurrence symbol")?;
            let range = occurrence_range(claim, source, admitted_encoding)?;
            let roles = claim.envelope.raw["symbolRoles"]
                .as_u64()
                .context("invalid occurrence roles")?;
            let _syntax_kind = claim.envelope.raw["syntaxKind"]
                .as_i64()
                .context("invalid syntax kind")?;
            let declaration = roles & 1 != 0;
            let binding =
                measured.join(&doc, range.clone(), semantic::JoinKind::DeclarationName)?;
            let reference = measured.join(&doc, range.clone(), semantic::JoinKind::Reference)?;
            let call = measured.join(&doc, range.clone(), semantic::JoinKind::Callee)?;
            if declaration && binding.status == semantic::JoinStatus::Exact {
                let candidate = native
                    .declarations
                    .iter()
                    .find(|d| d.syntax_id == binding.candidate_ids[0])
                    .context("missing exact native declaration")?;
                let symbol_info = inventory.claims.iter().find(|c| {
                    c.envelope.record_kind == "symbolInformation"
                        && c.envelope.relative_path == doc.path
                        && c.envelope.raw["symbol"].as_str() == Some(symbol)
                });
                let compatible = symbol_info
                    .and_then(|c| c.envelope.raw["kind"].as_i64())
                    .is_some_and(|kind| match candidate.kind.as_str() {
                        "type" => matches!(kind, 7 | 11 | 21),
                        "method" => kind == 26,
                        _ => true,
                    });
                if compatible {
                    declarations
                        .entry(scoped_key(symbol, &doc))
                        .or_default()
                        .insert((doc.clone(), binding.candidate_ids[0].clone()));
                }
            }
            type_uses
                .entry((doc.clone(), range.start, range.end))
                .or_default()
                .insert(symbol.into());
            occurrences.push((
                claim,
                doc,
                symbol.to_owned(),
                roles,
                binding,
                reference,
                call,
            ));
        }
    }
    let targets = |doc: &native::DocumentKey, symbol: &str| -> Vec<semantic::Target> {
        declarations
            .get(&scoped_key(symbol, doc))
            .into_iter()
            .flat_map(|ids| ids.iter())
            .map(|(owner, id)| semantic::Target::Internal {
                syntax_id: id.clone(),
                document: owner.clone(),
                revision_id: native.revision.id.clone(),
            })
            .collect()
    };
    let target = |doc: &native::DocumentKey, symbol: &str| -> Option<semantic::Target> {
        let items = targets(doc, symbol);
        (items.len() == 1)
            .then(|| items.into_iter().next())
            .flatten()
    };
    for claim in &inventory.claims {
        if claim.envelope.record_kind != "symbolInformation" {
            continue;
        }
        let doc = native::DocumentKey {
            source_set_id: native.revision.source_set_id.clone(),
            language: claim.language.clone(),
            path: claim.envelope.relative_path.clone(),
        };
        let key = claim.envelope.raw["symbol"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("missing symbol key")?;
        let declarations = targets(&doc, key);
        out.symbols.push(semantic::Symbol {
            key: symbol_key(key, &doc),
            display_name: claim.envelope.raw["displayName"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            declarations,
            provenance_id: claim.captured_proof_id.clone(),
        });
    }
    let mut reference_claims: BTreeMap<String, Vec<(String, semantic::Target)>> = BTreeMap::new();
    let role_sites: BTreeMap<_, _> = sources
        .iter()
        .map(|(doc, bytes)| {
            Ok((
                doc.clone(),
                source_role_sites(doc.language.as_str(), bytes)?,
            ))
        })
        .collect::<Result<_>>()?;
    let mut call_reference_ids: BTreeMap<String, String> = BTreeMap::new();
    for (claim, doc, symbol, roles, binding, reference, call) in occurrences {
        let key = symbol_key(&symbol, &doc);
        let mut asserted = targets(&doc, &symbol);
        if asserted.is_empty()
            && key.scope == semantic::SymbolScope::Global
            && !inventory.claims.iter().any(|c| {
                c.envelope.record_kind == "occurrence"
                    && c.envelope.raw["symbol"].as_str() == Some(&symbol)
                    && c.envelope.raw["symbolRoles"]
                        .as_u64()
                        .is_some_and(|roles| roles & 1 != 0)
            })
            && !inventory.claims.iter().any(|c| {
                c.envelope.record_kind == "symbolInformation"
                    && c.envelope.raw["symbol"].as_str() == Some(&symbol)
            })
        {
            asserted.push(semantic::Target::External {
                symbol: key.clone(),
            });
        }
        asserted.sort_by_key(|t| {
            native_ids::canonical(&serde_json::to_value(t).expect("serializable target"))
        });
        asserted.dedup();
        let (resolution, resolved, candidates) = match asserted.len() {
            0 => (semantic::Resolution::Unresolved, None, vec![]),
            1 => {
                let item = asserted.pop().unwrap();
                let status = if matches!(item, semantic::Target::Internal { .. }) {
                    semantic::Resolution::Resolved
                } else {
                    semantic::Resolution::External
                };
                (status, Some(item), vec![])
            }
            _ => (semantic::Resolution::Ambiguous, None, asserted),
        };
        if roles & 1 != 0 {
            out.joins
                .push((claim.captured_proof_id.clone(), binding.clone()));
            out.declaration_bindings.push(semantic::DeclarationBinding {
                syntax_id: (binding.status == semantic::JoinStatus::Exact)
                    .then(|| binding.candidate_ids[0].clone()),
                symbols: if binding.status == semantic::JoinStatus::Exact {
                    vec![key.clone()]
                } else {
                    vec![]
                },
                join: binding.clone(),
                provenance_id: claim.captured_proof_id.clone(),
            });
        }
        out.joins
            .push((claim.captured_proof_id.clone(), reference.clone()));
        let invocation = if roles & 1 == 0 && !claim.envelope.raw["enclosingRange"].is_null() {
            let source = sources.get(&doc).context("missing occurrence source")?;
            let mut enclosing_claim = claim.clone();
            enclosing_claim.envelope.raw["range"] = claim.envelope.raw["enclosingRange"].clone();
            let span = occurrence_range(&enclosing_claim, source, admitted_encoding)?;
            Some(measured.join(&doc, span, semantic::JoinKind::Invocation)?)
        } else {
            None
        };
        if reference.status == semantic::JoinStatus::Exact {
            let r = native
                .references
                .iter()
                .find(|r| r.id == reference.candidate_ids[0])
                .context("missing indexed native reference")?;
            let declaration_site = roles & 1 != 0
                && native.declarations.iter().any(|d| {
                    d.document == doc
                        && d.name_range.as_ref() == Some(&r.range)
                        && d.name.as_deref() == Some(&r.spelling)
                });
            let is_call = invocation.as_ref().is_some_and(|j| {
                j.status == semantic::JoinStatus::Exact
                    && native.calls.iter().any(|c| {
                        c.id == j.candidate_ids[0]
                            && c.document == doc
                            && c.owner_syntax_id == r.owner_syntax_id
                            && c.range.start <= r.range.start
                            && r.range.end <= c.range.end
                            && c.spelling.as_deref() == Some(&r.spelling)
                            && c.callee_range
                                .as_ref()
                                .is_none_or(|callee| callee == &r.range)
                            && (call.status != semantic::JoinStatus::Exact
                                || call.candidate_ids == j.candidate_ids)
                    })
            });
            let mut assigned = BTreeSet::new();
            if declaration_site {
                assigned.insert(semantic::Role::Definition);
            }
            let site = (r.range.start, r.range.end);
            if roles & 2 != 0
                && role_sites
                    .get(&doc)
                    .is_some_and(|sites: &RoleSites| sites.imports.contains(&site))
            {
                assigned.insert(semantic::Role::Import);
            }
            if !declaration_site
                && role_sites
                    .get(&doc)
                    .is_some_and(|sites: &RoleSites| sites.types.contains(&site))
                && type_uses
                    .get(&(doc.clone(), site.0, site.1))
                    .is_some_and(|keys| keys.len() == 1 && keys.contains(&symbol))
            {
                assigned.insert(semantic::Role::Type);
            }
            if roles & 4 != 0 {
                assigned.insert(semantic::Role::Write);
            }
            if is_call {
                assigned.insert(semantic::Role::Call);
            }
            if !declaration_site && roles & 4 == 0 && roles & 2 == 0 {
                assigned.insert(semantic::Role::Read);
            }
            if declaration_site
                && roles & 2 != 0
                && matches!(doc.language.as_str(), "javascript" | "rust")
                && binding.status == semantic::JoinStatus::Exact
                && native.declarations.iter().any(|d| {
                    d.syntax_id == binding.candidate_ids[0]
                        && d.document == doc
                        && d.kind == "alias"
                        && d.name_range.as_ref() == Some(&r.range)
                })
            {
                assigned.insert(semantic::Role::Alias);
            }
            if !assigned.is_empty() {
                for target in resolved.iter().chain(candidates.iter()) {
                    reference_claims
                        .entry(r.id.clone())
                        .or_default()
                        .push((claim.captured_proof_id.clone(), target.clone()));
                }
            }
            if !assigned.is_empty() && !out.references.iter().any(|prior| prior.id == r.id) {
                out.references.push(semantic::Reference {
                    id: r.id.clone(),
                    owner_syntax_id: r.owner_syntax_id.clone(),
                    ordinal: r.ordinal,
                    document: doc.clone(),
                    revision_id: native.revision.id.clone(),
                    range: r.range.clone(),
                    spelling: r.spelling.clone(),
                    lookup_key: r.lookup_key.clone(),
                    site: if declaration_site {
                        semantic::ReferenceSite::Declaration
                    } else {
                        semantic::ReferenceSite::Use
                    },
                    roles: assigned.into_iter().collect(),
                    resolution,
                    declared_target: resolved.clone(),
                    candidates: candidates.clone(),
                    provenance_id: claim.captured_proof_id.clone(),
                });
            }
        }
        if roles & 1 == 0 {
            let source = sources.get(&doc).context("missing occurrence source")?;
            if let Some(join) = &invocation {
                out.joins
                    .push((claim.captured_proof_id.clone(), join.clone()));
            }
            let candidate = if call.status == semantic::JoinStatus::Exact {
                call.clone()
            } else if invocation
                .as_ref()
                .is_some_and(|j| j.status == semantic::JoinStatus::Exact)
            {
                invocation.as_ref().expect("exact invocation").clone()
            } else {
                call.clone()
            };
            out.joins.push((claim.captured_proof_id.clone(), call));
            if candidate.status == semantic::JoinStatus::Exact
                && reference.status == semantic::JoinStatus::Exact
                && invocation.as_ref().is_some_and(|j| {
                    j.status == semantic::JoinStatus::Exact
                        && j.candidate_ids == candidate.candidate_ids
                })
            {
                let actual = native
                    .calls
                    .iter()
                    .find(|c| c.id == candidate.candidate_ids[0]);
                if actual.is_some_and(|c| {
                    let primary = &reference.anchor.range;
                    c.document == doc
                        && c.range.start <= primary.start
                        && primary.end <= c.range.end
                        && c.callee_range.as_ref().is_none_or(|r| r == primary)
                        && c.spelling.as_deref()
                            == source
                                .get(primary.start..primary.end)
                                .and_then(|b| std::str::from_utf8(b).ok())
                        && native.references.iter().any(|r| {
                            r.id == reference.candidate_ids[0]
                                && r.owner_syntax_id == c.owner_syntax_id
                                && r.range == *primary
                        })
                }) {
                    call_reference_ids.insert(
                        claim.captured_proof_id.clone(),
                        reference.candidate_ids[0].clone(),
                    );
                    out.call_bindings.push(semantic::CallBinding {
                        call_id: Some(candidate.candidate_ids[0].clone()),
                        join: candidate,
                        resolution,
                        stale_target: resolved.as_ref().and_then(|t| {
                            matches!(t, semantic::Target::Internal { .. }).then_some(false)
                        }),
                        declared_target: resolved,
                        candidates,
                        dispatch: semantic::Dispatch::Unknown,
                        possible_dispatch: vec![],
                        possible_dispatch_complete: false,
                        provenance_id: claim.captured_proof_id.clone(),
                    });
                }
            }
        }
    }
    for reference in &mut out.references {
        let mut claims = reference_claims.remove(&reference.id).unwrap_or_default();
        claims.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some((proof, _)) = claims.first() {
            reference.provenance_id = proof.clone();
        }
        let mut targets: Vec<_> = claims.into_iter().map(|(_, target)| target).collect();
        targets.sort_by_key(|target| {
            native_ids::canonical(&serde_json::to_value(target).expect("serializable target"))
        });
        targets.dedup();
        if targets.len() > 1 {
            reference.resolution = semantic::Resolution::Ambiguous;
            reference.declared_target = None;
            reference.candidates = targets;
        } else if let Some(target) = targets.pop() {
            reference.resolution = if matches!(target, semantic::Target::Internal { .. }) {
                semantic::Resolution::Resolved
            } else {
                semantic::Resolution::External
            };
            reference.declared_target = Some(target);
            reference.candidates.clear();
        }
    }
    for binding in &mut out.call_bindings {
        if let Some(reference) = call_reference_ids
            .get(&binding.provenance_id)
            .and_then(|id| out.references.iter().find(|r| &r.id == id))
        {
            binding.resolution = reference.resolution;
            binding.declared_target = reference.declared_target.clone();
            binding.candidates = reference.candidates.clone();
            binding.stale_target = reference
                .declared_target
                .as_ref()
                .and_then(|t| matches!(t, semantic::Target::Internal { .. }).then_some(false));
        }
    }
    for (doc, bytes) in sources {
        let bases = match doc.language.as_str() {
            "java" => direct_java_types(bytes)?,
            "javascript" => direct_script_types(
                bytes,
                tree_sitter_javascript::LANGUAGE.into(),
                "class_declaration",
                "class_heritage",
            )?,
            "python" => direct_script_types(
                bytes,
                tree_sitter_python::LANGUAGE.into(),
                "class_definition",
                "argument_list",
            )?,
            _ => continue,
        };
        for (owner_range, base_range, kind) in bases {
            let owner = type_uses.get(&(doc.clone(), owner_range.start, owner_range.end));
            let base = type_uses.get(&(doc.clone(), base_range.start, base_range.end));
            let (Some(owner), Some(base)) = (
                owner.filter(|s| s.len() == 1),
                base.filter(|s| s.len() == 1),
            ) else {
                continue;
            };
            let (source_symbol, target_symbol) =
                (owner.iter().next().unwrap(), base.iter().next().unwrap());
            let Some(source) = target(doc, source_symbol) else {
                continue;
            };
            let mut target_candidates = targets(doc, target_symbol);
            if target_candidates.is_empty()
                && symbol_key(target_symbol, doc).scope == semantic::SymbolScope::Global
                && !declarations.contains_key(&scoped_key(target_symbol, doc))
                && !inventory.claims.iter().any(|c| {
                    c.envelope.record_kind == "symbolInformation"
                        && c.envelope.raw["symbol"].as_str() == Some(target_symbol)
                })
            {
                target_candidates.push(semantic::Target::External {
                    symbol: symbol_key(target_symbol, doc),
                });
            }
            let Some(target) = (target_candidates.len() == 1)
                .then(|| target_candidates.pop())
                .flatten()
            else {
                if doc.language == "java" {
                    out.diagnostics
                        .entry(doc.clone())
                        .or_default()
                        .insert("unmeasurable_external_base".into());
                }
                continue;
            };
            for claim in inventory.claims.iter().filter(|c| {
                c.envelope.record_kind == "relationship"
                    && c.language == doc.language
                    && c.envelope.relative_path == doc.path
                    && c.envelope.raw["sourceSymbol"].as_str() == Some(source_symbol)
                    && c.envelope.raw["targetSymbol"].as_str() == Some(target_symbol)
            }) {
                out.type_relationships.push(semantic::TypeRelationship {
                    kind,
                    source: source.clone(),
                    target: target.clone(),
                    provenance_id: claim.captured_proof_id.clone(),
                });
            }
        }
        if doc.language == "java" {
            for claim in inventory.claims.iter().filter(|c| {
                c.envelope.record_kind == "relationship"
                    && c.language == "java"
                    && c.envelope.relative_path == doc.path
            }) {
                let raw = &claim.envelope.raw;
                let (Some(source_symbol), Some(target_symbol)) =
                    (raw["sourceSymbol"].as_str(), raw["targetSymbol"].as_str())
                else {
                    continue;
                };
                let source_methods: Vec<_> = native
                    .declarations
                    .iter()
                    .filter(|d| {
                        d.document == *doc
                            && d.kind == "method"
                            && d.name_range.is_some()
                            && type_uses
                                .get(&(
                                    doc.clone(),
                                    d.name_range.as_ref().unwrap().start,
                                    d.name_range.as_ref().unwrap().end,
                                ))
                                .is_some_and(|keys| keys.contains(source_symbol))
                    })
                    .collect();
                let [source_method] = source_methods.as_slice() else {
                    if source_methods.is_empty() && target(doc, source_symbol).is_none() {
                        out.diagnostics
                            .entry(doc.clone())
                            .or_default()
                            .insert("unmeasurable_override_endpoint".into());
                    }
                    continue;
                };
                let Some(source_target) = target(doc, source_symbol) else {
                    continue;
                };
                let target_methods: Vec<_> = native
                    .declarations
                    .iter()
                    .filter(|d| {
                        d.kind == "method"
                            && d.name_range.is_some()
                            && d.document.source_set_id == doc.source_set_id
                            && type_uses
                                .get(&(
                                    d.document.clone(),
                                    d.name_range.as_ref().unwrap().start,
                                    d.name_range.as_ref().unwrap().end,
                                ))
                                .is_some_and(|keys| keys.contains(target_symbol))
                    })
                    .collect();
                if target_methods.len() > 1 {
                    out.diagnostics
                        .entry(doc.clone())
                        .or_default()
                        .insert("unmeasurable_override_endpoint".into());
                    continue;
                }
                let target_method = target_methods.first().copied();
                let source_owner = source_method.ancestors.last();
                let owner_declarations: Vec<_> = native
                    .declarations
                    .iter()
                    .filter(|d| {
                        d.document == *doc
                            && d.kind == "type"
                            && source_owner == Some(&d.key)
                            && d.range.start <= source_method.range.start
                            && source_method.range.end <= d.range.end
                    })
                    .collect();
                let direct_base = (owner_declarations.len() == 1)
                    .then(|| owner_declarations[0])
                    .and_then(|owner| owner.name_range.as_ref())
                    .and_then(|r| {
                        direct_java_types(bytes)
                            .ok()
                            .and_then(|rows| rows.into_iter().find(|(range, _, _)| range == r))
                    })
                    .and_then(|(_, range, _)| type_uses.get(&(doc.clone(), range.start, range.end)))
                    .filter(|s| s.len() == 1)
                    .and_then(|s| s.iter().next());
                let target_owner_key = target_symbol
                    .rsplit_once('#')
                    .map(|(owner, _)| format!("{owner}#"));
                let method_descriptor_matches =
                    target_symbol.rsplit_once('#').is_some_and(|(_, suffix)| {
                        source_symbol
                            .rsplit_once('#')
                            .is_some_and(|(_, source_suffix)| {
                                target_method.is_none() || suffix == source_suffix
                            })
                    });
                let target_owner_matches = target_method.is_none_or(|method| {
                    target_owner_key.as_ref().is_some_and(|owner_key| {
                        native
                            .declarations
                            .iter()
                            .filter(|owner| {
                                owner.document == method.document
                                    && owner.kind == "type"
                                    && method.ancestors.last() == Some(&owner.key)
                                    && owner.range.start <= method.range.start
                                    && method.range.end <= owner.range.end
                                    && owner.name_range.as_ref().is_some_and(|r| {
                                        type_uses
                                            .get(&(owner.document.clone(), r.start, r.end))
                                            .is_some_and(|keys| {
                                                keys.len() == 1 && keys.contains(owner_key)
                                            })
                                    })
                            })
                            .count()
                            == 1
                    })
                });
                let owner_matches = direct_base
                    .is_some_and(|base| Some(base) == target_owner_key.as_ref())
                    && method_descriptor_matches
                    && target_owner_matches;
                if !owner_matches {
                    if target_method.is_none()
                        && direct_base.is_none_or(|base| target_owner_key.as_ref() != Some(base))
                    {
                        out.diagnostics
                            .entry(doc.clone())
                            .or_default()
                            .insert("unmeasurable_override_endpoint".into());
                    }
                    continue;
                }
                let destination = target(doc, target_symbol).or_else(|| {
                    (target_method.is_none()).then(|| semantic::Target::External {
                        symbol: symbol_key(target_symbol, doc),
                    })
                });
                if let Some(destination) = destination {
                    out.type_relationships.push(semantic::TypeRelationship {
                        kind: semantic::RelationshipKind::Overrides,
                        source: source_target,
                        target: destination,
                        provenance_id: claim.captured_proof_id.clone(),
                    });
                }
            }
            for declaration in native.declarations.iter().filter(|d| {
                d.document == *doc
                    && d.kind == "type"
                    && std::str::from_utf8(bytes).ok().is_some_and(|source| {
                        d.name_range
                            .as_ref()
                            .and_then(|r| source.get(..r.start))
                            .and_then(|prefix| prefix.rsplit(['{', '}', ';']).next())
                            .is_some_and(|prefix| prefix.split_whitespace().last() == Some("enum"))
                    })
            }) {
                let Some(name) = &declaration.name_range else {
                    continue;
                };
                let Some(keys) = type_uses
                    .get(&(doc.clone(), name.start, name.end))
                    .filter(|s| s.len() == 1)
                else {
                    continue;
                };
                let source_symbol = keys.iter().next().unwrap();
                if target(doc, source_symbol)
                    != Some(semantic::Target::Internal {
                        syntax_id: declaration.syntax_id.clone(),
                        document: doc.clone(),
                        revision_id: native.revision.id.clone(),
                    })
                {
                    continue;
                }
                for claim in inventory.claims.iter().filter(|c| {
                    c.envelope.record_kind == "relationship"
                        && c.language == "java"
                        && c.envelope.relative_path == doc.path
                        && c.envelope.raw["sourceSymbol"].as_str() == Some(source_symbol)
                        && c.envelope.raw["targetSymbol"]
                            .as_str()
                            .is_some_and(|s| s.ends_with("java/lang/Enum#"))
                }) {
                    let key = claim.envelope.raw["targetSymbol"].as_str().unwrap();
                    out.type_relationships.push(semantic::TypeRelationship {
                        kind: semantic::RelationshipKind::Extends,
                        source: semantic::Target::Internal {
                            syntax_id: declaration.syntax_id.clone(),
                            document: doc.clone(),
                            revision_id: native.revision.id.clone(),
                        },
                        target: semantic::Target::External {
                            symbol: symbol_key(key, doc),
                        },
                        provenance_id: claim.captured_proof_id.clone(),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// A clause node contributes only direct named type-use spans, never ancestors inferred
/// from producer flags. The exact raw occurrence at that span supplies endpoint identity.
fn direct_java_types(
    source: &[u8],
) -> Result<Vec<(native::Range, native::Range, semantic::RelationshipKind)>> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_java::LANGUAGE.into())?;
    let tree = parser.parse(source, None).context("Java parser failed")?;
    let mut result = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
        ) && let Some(name) = node.child_by_field_name("name")
        {
            let owner = native::Range {
                start: name.start_byte(),
                end: name.end_byte(),
            };
            let mut clauses = Vec::new();
            if let Some(base) = node.child_by_field_name("superclass") {
                clauses.push((base, semantic::RelationshipKind::Extends));
            }
            if let Some(interfaces) = node.child_by_field_name("interfaces") {
                clauses.push((interfaces, semantic::RelationshipKind::Implements));
            }
            if kind == "interface_declaration" {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "extends_interfaces" {
                        clauses.push((child, semantic::RelationshipKind::Extends));
                    }
                }
            }
            for (clause, relationship) in clauses {
                let mut todo = vec![clause];
                while let Some(item) = todo.pop() {
                    if matches!(
                        item.kind(),
                        "type_list" | "super_interfaces" | "interface_type_list"
                    ) {
                        let mut cursor = item.walk();
                        todo.extend(item.named_children(&mut cursor));
                    } else if item.kind() == "type_identifier" {
                        result.push((
                            owner.clone(),
                            native::Range {
                                start: item.start_byte(),
                                end: item.end_byte(),
                            },
                            relationship,
                        ));
                    } else if matches!(item.kind(), "generic_type" | "scoped_type_identifier") {
                        // Only the outermost base type, never a generic argument.
                        let mut outer = item;
                        let mut base = None;
                        while matches!(outer.kind(), "generic_type" | "scoped_type_identifier") {
                            let mut cursor = outer.walk();
                            let children: Vec<_> = outer.named_children(&mut cursor).collect();
                            let next = if outer.kind() == "generic_type" {
                                children.first()
                            } else {
                                children.last()
                            };
                            let Some(child) = next else { break };
                            outer = *child;
                            base = Some(outer);
                        }
                        if let Some(base) = base {
                            result.push((
                                owner.clone(),
                                native::Range {
                                    start: base.start_byte(),
                                    end: base.end_byte(),
                                },
                                relationship,
                            ));
                        }
                    } else {
                        let mut cursor = item.walk();
                        todo.extend(item.named_children(&mut cursor));
                    }
                }
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    Ok(result)
}

#[derive(Default)]
struct RoleSites {
    types: BTreeSet<(usize, usize)>,
    imports: BTreeSet<(usize, usize)>,
}

fn source_role_sites(language: &str, source: &[u8]) -> Result<RoleSites> {
    let grammar: tree_sitter::Language = match language {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        _ => return Ok(RoleSites::default()),
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar)?;
    let tree = parser
        .parse(source, None)
        .context("semantic role source parser failed")?;
    let mut sites = RoleSites::default();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        let type_field = match language {
            "java"
                if matches!(
                    kind,
                    "field_declaration"
                        | "local_variable_declaration"
                        | "formal_parameter"
                        | "spread_parameter"
                        | "catch_formal_parameter"
                        | "cast_expression"
                        | "instanceof_expression"
                        | "method_declaration"
                        | "constructor_declaration"
                        | "object_creation_expression"
                        | "array_creation_expression"
                        | "superclass"
                        | "super_interfaces"
                        | "extends_interfaces"
                        | "type_arguments"
                        | "type_parameters"
                        | "array_type"
                ) =>
            {
                Some("type")
            }
            "python"
                if matches!(
                    kind,
                    "typed_parameter"
                        | "typed_default_parameter"
                        | "assignment"
                        | "function_definition"
                ) =>
            {
                Some("type")
            }
            "rust"
                if matches!(
                    kind,
                    "trait_item" | "impl_item" | "struct_item" | "enum_item"
                ) =>
            {
                Some("bounds")
            }
            "rust"
                if matches!(
                    kind,
                    "field_declaration"
                        | "let_declaration"
                        | "parameter"
                        | "function_item"
                        | "type_item"
                        | "const_item"
                        | "static_item"
                ) =>
            {
                Some("type")
            }
            _ => None,
        };
        if let Some(field) = type_field.and_then(|name| node.child_by_field_name(name)) {
            collect_role_leaf_spans(field, language, &mut sites.types);
        }
        if language == "rust" && kind == "impl_item" {
            for field in ["trait", "type"] {
                if let Some(child) = node.child_by_field_name(field) {
                    collect_role_leaf_spans(child, language, &mut sites.types);
                }
            }
        }
        if language == "java"
            && matches!(
                kind,
                "superclass" | "super_interfaces" | "extends_interfaces"
            )
        {
            collect_role_leaf_spans(node, language, &mut sites.types);
        }
        if language == "javascript" && kind == "class_heritage" {
            collect_role_leaf_spans(node, language, &mut sites.types);
        }
        if language == "python" && kind == "class_definition" {
            let mut cursor = node.walk();
            for child in node
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "argument_list")
            {
                collect_role_leaf_spans(child, language, &mut sites.types);
            }
        }
        if language == "javascript"
            && kind == "binary_expression"
            && let (Some(operator), Some(right)) = (
                node.child_by_field_name("operator"),
                node.child_by_field_name("right"),
            )
            && source.get(operator.byte_range()) == Some(b"instanceof".as_slice())
        {
            collect_role_leaf_spans(right, language, &mut sites.types);
        }
        let import_root = match language {
            "java" => kind == "import_declaration",
            "javascript" => kind == "import_statement",
            "python" => matches!(kind, "import_statement" | "import_from_statement"),
            "rust" => kind == "use_declaration",
            _ => false,
        };
        if import_root {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if language == "python"
                    && kind == "import_from_statement"
                    && node.child_by_field_name("module_name") == Some(child)
                {
                    continue;
                }
                collect_role_leaf_spans(child, language, &mut sites.imports);
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    Ok(sites)
}

fn collect_role_leaf_spans(
    node: tree_sitter::Node<'_>,
    language: &str,
    spans: &mut BTreeSet<(usize, usize)>,
) {
    let leaf = match language {
        "java" => matches!(node.kind(), "identifier" | "type_identifier"),
        "javascript" => matches!(
            node.kind(),
            "identifier" | "property_identifier" | "shorthand_property_identifier"
        ),
        "python" => node.kind() == "identifier",
        "rust" => matches!(node.kind(), "identifier" | "type_identifier"),
        _ => false,
    };
    if leaf {
        spans.insert((node.start_byte(), node.end_byte()));
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_role_leaf_spans(child, language, spans);
    }
}

fn direct_script_types(
    source: &[u8],
    language: tree_sitter::Language,
    owner_kind: &str,
    clause_kind: &str,
) -> Result<Vec<(native::Range, native::Range, semantic::RelationshipKind)>> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language)?;
    let tree = parser
        .parse(source, None)
        .context("semantic source parser failed")?;
    let mut result = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == owner_kind
            && let Some(name) = node.child_by_field_name("name")
        {
            let owner = native::Range {
                start: name.start_byte(),
                end: name.end_byte(),
            };
            let mut cursor = node.walk();
            for clause in node
                .named_children(&mut cursor)
                .filter(|c| c.kind() == clause_kind)
            {
                let mut child_cursor = clause.walk();
                for base in clause.named_children(&mut child_cursor) {
                    let kind = base.kind();
                    if kind == "identifier" {
                        result.push((
                            owner.clone(),
                            native::Range {
                                start: base.start_byte(),
                                end: base.end_byte(),
                            },
                            semantic::RelationshipKind::Extends,
                        ));
                    } else if kind == "attribute" {
                        let mut cursor = base.walk();
                        if let Some(name) = base.named_children(&mut cursor).last() {
                            result.push((
                                owner.clone(),
                                native::Range {
                                    start: name.start_byte(),
                                    end: name.end_byte(),
                                },
                                semantic::RelationshipKind::Extends,
                            ));
                        }
                    }
                }
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    Ok(result)
}
