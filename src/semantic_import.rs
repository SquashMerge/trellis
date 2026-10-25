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
    let mut positions = Vec::with_capacity(values.len());
    for pair in values.as_chunks::<2>().0 {
        positions.push((i32::try_from(pair[0])?, i32::try_from(pair[1])?));
    }
    if values.len() == 3 {
        positions.push((i32::try_from(values[0])?, i32::try_from(values[2])?));
    }
    let endpoints = (positions[0], positions[1]);
    let offsets = crate::scip_raw::source_byte_offsets(
        text,
        encoding,
        &mut positions,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )?;
    let start = offsets[&endpoints.0];
    let end = offsets[&endpoints.1];
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
            if !declaration_site
                && roles & 4 == 0
                && roles & 2 == 0
                && (!assigned.contains(&semantic::Role::Type) || doc.language == "javascript")
            {
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
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                js_direct_class_span(child, &mut sites.types);
            }
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
            js_direct_class_span(right, &mut sites.types);
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

fn js_direct_class_span(node: tree_sitter::Node<'_>, spans: &mut BTreeSet<(usize, usize)>) {
    match node.kind() {
        "identifier" => {
            spans.insert((node.start_byte(), node.end_byte()));
        }
        "member_expression" => {
            if let Some(property) = node.child_by_field_name("property") {
                spans.insert((property.start_byte(), property.end_byte()));
            }
        }
        _ => {}
    }
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

/// Publication-time rejoin uses only retained, authenticated source and measured snapshots.
/// The Store validates the returned publication and its raw links before its paired-pin CAS.
#[derive(Debug)]
pub struct RejoinResult {
    pub publication: semantic::SemanticPublication,
    pub report: serde_json::Value,
}

#[allow(clippy::too_many_arguments)]
pub fn rejoin_inventory(
    inventory: &CapturedScipInventory,
    captured_native: &native::Artifact,
    captured_sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    destination_native: &native::Artifact,
    destination_sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    producer: semantic::Producer,
    basis: &BTreeMap<native::DocumentKey, semantic::SemanticBasis>,
    admitted_encoding: u8,
) -> Result<RejoinResult> {
    ensure!(
        inventory.source_manifest_hash == manifest_hash(&captured_native.revision)
            && captured_native.revision.source_set_id == destination_native.revision.source_set_id,
        "captured source manifest/source set mismatch"
    );
    let r1 = MeasuredFacts::new(captured_native, captured_sources)?;
    let r2 = MeasuredFacts::new(destination_native, destination_sources)?;
    let captured = join_inventory(
        inventory,
        captured_native,
        captured_sources,
        admitted_encoding,
    )?;
    let mut eligible = BTreeSet::new();
    let mut unsupported_claims = BTreeSet::new();
    let mut unsupported_documents = BTreeSet::new();
    let mut failed = BTreeSet::new();
    let mut requested = BTreeSet::new();
    let exclusions: BTreeMap<_, _> = inventory
        .exclusions
        .iter()
        .map(|exclusion| {
            (
                (
                    exclusion.language.as_str(),
                    exclusion.relative_path.as_str(),
                ),
                exclusion.reason.as_str(),
            )
        })
        .collect();
    for document in &captured_native.revision.documents {
        let language: semantic::Language =
            serde_json::from_value(serde_json::json!(document.key.language))?;
        if producer.languages.contains(&language) {
            let reason =
                exclusions.get(&(document.key.language.as_str(), document.key.path.as_str()));
            if reason != Some(&"notRequested") {
                requested.insert(document.key.clone());
            }
            if reason == Some(&"failed")
                || (reason != Some(&"notRequested")
                    && destination_sources.get(&document.key)
                        != captured_sources.get(&document.key))
            {
                failed.insert(document.key.clone());
            }
        }
    }
    for claim in &inventory.claims {
        inventory.lookup(
            &claim.captured_proof_id,
            &claim.envelope.record_kind,
            &claim.fact_digest,
        )?;
        let doc = native::DocumentKey {
            source_set_id: captured_native.revision.source_set_id.clone(),
            language: claim.language.clone(),
            path: claim.envelope.relative_path.clone(),
        };
        requested.insert(doc.clone());
        let captured_document = captured_native
            .revision
            .documents
            .iter()
            .find(|d| d.key == doc)
            .context("captured claim outside source manifest")?;
        let bytes = captured_sources
            .get(&doc)
            .context("missing captured claim source")?;
        ensure!(
            native_ids::digest(b"", bytes) == captured_document.content_hash,
            "captured source hash mismatch"
        );
        if destination_sources.get(&doc) != Some(bytes) {
            failed.insert(doc);
            continue;
        }
        if claim.envelope.record_kind == "occurrence" {
            let range = occurrence_range(claim, bytes, admitted_encoding)?;
            let roles = claim.envelope.raw["symbolRoles"]
                .as_u64()
                .context("invalid raw roles")?;
            // Rejoin only the families that actually authored a captured source fact.
            // A tempting callee at the same bytes cannot replace a missing reference,
            // nor can an enclosing range alone turn a plain reference into a call.
            let required = if roles & 1 != 0 {
                semantic::JoinKind::DeclarationName
            } else {
                semantic::JoinKind::Reference
            };
            let first = r1.join(&doc, range.clone(), required)?;
            let second = r2.join(&doc, range, required)?;
            if first.status == semantic::JoinStatus::Unsupported
                && second.status == semantic::JoinStatus::Unsupported
            {
                unsupported_claims.insert(claim.captured_proof_id.clone());
                unsupported_documents.insert(doc.clone());
                eligible.insert(doc);
                continue;
            }
            if first.status == semantic::JoinStatus::Unmatched
                && second.status == semantic::JoinStatus::Unmatched
            {
                eligible.insert(doc);
                continue;
            }
            let mut exact = first.status == semantic::JoinStatus::Exact
                && second.status == semantic::JoinStatus::Exact;
            let captured_call = captured.call_bindings.iter().find(|binding| {
                binding.provenance_id == claim.captured_proof_id && binding.call_id.is_some()
            });
            if let Some(binding) = captured_call {
                let call = r2.join(
                    &doc,
                    binding.join.anchor.range.clone(),
                    binding.join.anchor.kind,
                )?;
                let first_call = r1.join(
                    &doc,
                    binding.join.anchor.range.clone(),
                    binding.join.anchor.kind,
                )?;
                exact &= call.status == semantic::JoinStatus::Exact
                    && first_call.status == semantic::JoinStatus::Exact
                    && first_call.candidate_ids == binding.join.candidate_ids;
                // The selected reference and invocation must belong to that same
                // measured call, not merely be independent exact spans.
                exact &= second.candidate_ids.first().is_some_and(|id| {
                    destination_native.references.iter().any(|reference| {
                        &reference.id == id
                            && destination_native.calls.iter().any(|candidate| {
                                call.candidate_ids == vec![candidate.id.clone()]
                                    && candidate.owner_syntax_id == reference.owner_syntax_id
                                    && candidate.range.start <= reference.range.start
                                    && reference.range.end <= candidate.range.end
                                    && candidate
                                        .callee_range
                                        .as_ref()
                                        .is_none_or(|callee| callee == &reference.range)
                            })
                    })
                });
            }
            if exact {
                eligible.insert(doc);
            } else {
                failed.insert(doc);
            }
        } else {
            eligible.insert(doc);
        }
    }
    eligible.retain(|doc| !failed.contains(doc));
    let mut filtered = inventory.clone();
    filtered.claims.retain(|claim| {
        eligible
            .iter()
            .any(|doc| doc.language == claim.language && doc.path == claim.envelope.relative_path)
    });
    let destination = join_inventory(
        &filtered,
        destination_native,
        destination_sources,
        admitted_encoding,
    )?;
    let mut captured_targets: BTreeMap<(native::DocumentKey, String), Vec<semantic::Target>> =
        BTreeMap::new();
    for binding in &captured.declaration_bindings {
        if let Some(syntax_id) = &binding.syntax_id {
            for symbol in &binding.symbols {
                captured_targets
                    .entry((binding.join.anchor.document.clone(), symbol.symbol.clone()))
                    .or_default()
                    .push(semantic::Target::Internal {
                        syntax_id: syntax_id.clone(),
                        document: binding.join.anchor.document.clone(),
                        revision_id: captured_native.revision.id.clone(),
                    });
            }
        }
    }
    for targets in captured_targets.values_mut() {
        targets.sort_by_key(|target| {
            native_ids::canonical(&serde_json::to_value(target).expect("serializable target"))
        });
        targets.dedup();
    }
    let captured_declarations: BTreeMap<_, _> = captured
        .declaration_bindings
        .iter()
        .filter_map(|b| {
            b.syntax_id
                .as_ref()
                .map(|id| (b.provenance_id.clone(), id.clone()))
        })
        .collect();
    let mut omitted = BTreeSet::new();
    let retarget = |target: &mut semantic::Target| -> bool {
        match target {
            semantic::Target::External { symbol }
                if captured_targets
                    .iter()
                    .any(|((_, key), targets)| key == &symbol.symbol && !targets.is_empty()) =>
            {
                false
            }
            semantic::Target::Internal {
                syntax_id,
                document,
                revision_id,
            } => {
                if !captured_native.declarations.iter().any(|d| {
                    d.syntax_id == *syntax_id && d.document == *document && d.name_range.is_some()
                }) {
                    return false;
                }
                let candidates: Vec<_> = destination_native
                    .declarations
                    .iter()
                    .filter(|d| {
                        d.syntax_id == *syntax_id
                            && d.document == *document
                            && d.name_range.is_some()
                    })
                    .collect();
                if candidates.len() != 1 {
                    return false;
                }
                let declaration = candidates[0];
                let Some(bytes) = destination_sources.get(document) else {
                    return false;
                };
                let Some(name) = declaration.name_range.as_ref() else {
                    return false;
                };
                if std::str::from_utf8(bytes)
                    .ok()
                    .and_then(|s| s.get(name.start..name.end))
                    != declaration.name.as_deref()
                {
                    return false;
                }
                *revision_id = destination_native.revision.id.clone();
                true
            }
            _ => true,
        }
    };
    let facts = destination;
    let mut provenance_sources = BTreeSet::<(String, semantic::EvidenceKind)>::new();
    let mut symbol_rows = Vec::new();
    for mut symbol in facts.symbols {
        let raw = filtered
            .claims
            .iter()
            .find(|c| c.captured_proof_id == symbol.provenance_id);
        if let Some(claim) = raw {
            let doc = native::DocumentKey {
                source_set_id: destination_native.revision.source_set_id.clone(),
                language: claim.language.clone(),
                path: claim.envelope.relative_path.clone(),
            };
            if let Some(targets) = captured_targets.get(&(doc, symbol.key.symbol.clone())) {
                symbol.declarations = targets.clone();
            }
        }
        if symbol.declarations.iter_mut().all(&retarget) {
            provenance_sources.insert((
                symbol.provenance_id.clone(),
                semantic::EvidenceKind::DeclarationBinding,
            ));
            symbol_rows.push(symbol);
        } else if let Some(claim) = filtered
            .claims
            .iter()
            .find(|c| c.captured_proof_id == symbol.provenance_id)
        {
            omitted.insert(Some(native::DocumentKey {
                source_set_id: destination_native.revision.source_set_id.clone(),
                language: claim.language.clone(),
                path: claim.envelope.relative_path.clone(),
            }));
        }
    }
    let mut bindings = Vec::new();
    for binding in facts.declaration_bindings {
        if binding.syntax_id.is_some()
            && captured_declarations.get(&binding.provenance_id) == binding.syntax_id.as_ref()
        {
            provenance_sources.insert((
                binding.provenance_id.clone(),
                semantic::EvidenceKind::DeclarationBinding,
            ));
            bindings.push(binding);
        }
    }
    let old_targets: BTreeMap<_, _> = captured
        .call_bindings
        .iter()
        .map(|b| {
            (
                b.provenance_id.clone(),
                (
                    b.resolution,
                    b.declared_target.clone(),
                    b.candidates.clone(),
                ),
            )
        })
        .collect();
    let mut calls = Vec::new();
    for mut call in facts.call_bindings {
        let Some((resolution, target, candidates)) = old_targets.get(&call.provenance_id) else {
            continue;
        };
        call.resolution = *resolution;
        call.declared_target = target.clone();
        call.candidates = candidates.clone();
        if call
            .declared_target
            .iter_mut()
            .chain(call.candidates.iter_mut())
            .all(&retarget)
        {
            call.stale_target = matches!(
                call.declared_target,
                Some(semantic::Target::Internal { .. })
            )
            .then_some(false);
            provenance_sources.insert((
                call.provenance_id.clone(),
                semantic::EvidenceKind::SemanticReference,
            ));
            calls.push(call);
        } else {
            omitted.insert(Some(call.join.anchor.document.clone()));
        }
    }
    let old_refs: BTreeMap<_, _> = captured
        .references
        .iter()
        .map(|r| {
            (
                r.provenance_id.clone(),
                (
                    r.resolution,
                    r.declared_target.clone(),
                    r.candidates.clone(),
                ),
            )
        })
        .collect();
    let mut references = Vec::new();
    for mut reference in facts.references {
        if let Some((resolution, target, candidates)) = old_refs.get(&reference.provenance_id) {
            reference.resolution = *resolution;
            reference.declared_target = target.clone();
            reference.candidates = candidates.clone();
        }
        if reference
            .declared_target
            .iter_mut()
            .chain(reference.candidates.iter_mut())
            .all(&retarget)
        {
            provenance_sources.insert((
                reference.provenance_id.clone(),
                semantic::EvidenceKind::SemanticReference,
            ));
            references.push(reference);
        } else {
            omitted.insert(Some(reference.document.clone()));
        }
    }
    // A canonical native reference has one stored row, but each distinct exact raw
    // claim contributing to its ambiguity must keep its own r2 raw-linked proof.
    let mut additional_reference_proofs = BTreeSet::new();
    for claim in &filtered.claims {
        if claim.envelope.record_kind != "occurrence"
            || captured
                .call_bindings
                .iter()
                .any(|b| b.provenance_id == claim.captured_proof_id)
            || references
                .iter()
                .any(|r| r.provenance_id == claim.captured_proof_id)
        {
            continue;
        }
        let Some((_, joined)) = facts.joins.iter().find(|(id, join)| {
            id == &claim.captured_proof_id
                && join.anchor.kind == semantic::JoinKind::Reference
                && join.status == semantic::JoinStatus::Exact
        }) else {
            continue;
        };
        let Some((_, captured_join)) = captured.joins.iter().find(|(id, join)| {
            id == &claim.captured_proof_id
                && join.anchor.kind == semantic::JoinKind::Reference
                && join.status == semantic::JoinStatus::Exact
        }) else {
            continue;
        };
        let Some(reference) = references.iter().find(|r| {
            r.id == joined.candidate_ids[0]
                && r.document == joined.anchor.document
                && r.range == joined.anchor.range
                && r.resolution == semantic::Resolution::Ambiguous
        }) else {
            continue;
        };
        ensure!(
            captured_join.anchor.document == reference.document
                && captured_join.anchor.range == reference.range,
            "captured reference anchor differs"
        );
        let symbol = claim.envelope.raw["symbol"]
            .as_str()
            .context("missing raw reference symbol")?;
        let mut asserted: Vec<_> = captured_targets
            .iter()
            .filter(|((document, key), _)| {
                key == symbol && (!symbol.starts_with("local ") || document == &reference.document)
            })
            .flat_map(|(_, targets)| targets.iter().cloned())
            .collect();
        asserted.sort_by_key(|target| {
            native_ids::canonical(&serde_json::to_value(target).expect("serializable target"))
        });
        asserted.dedup();
        if asserted.is_empty()
            && !symbol.starts_with("local ")
            && !inventory.claims.iter().any(|other| {
                other.envelope.raw["symbol"].as_str() == Some(symbol)
                    && (other.envelope.record_kind == "symbolInformation"
                        || other.envelope.raw["symbolRoles"]
                            .as_u64()
                            .is_some_and(|roles| roles & 1 != 0))
            })
        {
            asserted.push(semantic::Target::External {
                symbol: symbol_key(symbol, &reference.document),
            });
        }
        if asserted.len() == 1
            && retarget(&mut asserted[0])
            && reference.candidates.contains(&asserted[0])
        {
            additional_reference_proofs.insert(claim.captured_proof_id.clone());
            provenance_sources.insert((
                claim.captured_proof_id.clone(),
                semantic::EvidenceKind::SemanticReference,
            ));
        }
    }
    let mut relationships = Vec::new();
    for mut relationship in facts.type_relationships {
        if retarget(&mut relationship.source) && retarget(&mut relationship.target) {
            provenance_sources.insert((
                relationship.provenance_id.clone(),
                semantic::EvidenceKind::TypeRelationship,
            ));
            relationships.push(relationship);
        } else if let Some(claim) = filtered
            .claims
            .iter()
            .find(|c| c.captured_proof_id == relationship.provenance_id)
        {
            omitted.insert(Some(native::DocumentKey {
                source_set_id: destination_native.revision.source_set_id.clone(),
                language: claim.language.clone(),
                path: claim.envelope.relative_path.clone(),
            }));
        }
    }
    let mut provenance = Vec::new();
    let mut ids = BTreeMap::new();
    for (raw_id, kind) in provenance_sources {
        let claim = inventory
            .claims
            .iter()
            .find(|c| c.captured_proof_id == raw_id)
            .context("missing raw fact for joined provenance")?;
        inventory.lookup(&raw_id, &claim.envelope.record_kind, &claim.fact_digest)?;
        let doc = native::DocumentKey {
            source_set_id: destination_native.revision.source_set_id.clone(),
            language: claim.language.clone(),
            path: claim.envelope.relative_path.clone(),
        };
        let captured_basis = basis
            .get(&doc)
            .context("missing immutable captured basis")?;
        ensure!(
            captured_basis.revision_id == captured_native.revision.id
                && captured_basis.artifact_hash == inventory.artifact_hash
                && captured_basis.source_manifest_hash == inventory.source_manifest_hash
                && captured_basis.producer_id == producer.id
                && captured_basis.language
                    == serde_json::from_value::<semantic::Language>(serde_json::json!(
                        doc.language
                    ))?,
            "captured semantic basis differs from authenticated inventory"
        );
        let dst = destination_native
            .revision
            .documents
            .iter()
            .find(|d| d.key == doc)
            .context("missing destination document")?;
        let freshness = if captured_native
            .revision
            .documents
            .iter()
            .find(|d| d.key == doc)
            .is_none_or(|d| d.content_hash != dst.content_hash)
        {
            semantic::Freshness::Stale
        } else if captured_basis.revision_id != destination_native.revision.id
            || captured_basis.source_manifest_hash != manifest_hash(&destination_native.revision)
            || captured_basis.toolchain_hash != destination_native.revision.toolchain_hash
            || captured_basis.config_hash != destination_native.revision.config_hash
            || captured_basis.dependency_hash != destination_native.revision.dependency_hash
            || captured_basis.producer_version != producer.version
            || captured_basis.producer_hash != producer.executable_hash
            || !producer.languages.contains(&captured_basis.language)
        {
            semantic::Freshness::PossiblyStale
        } else {
            semantic::Freshness::Fresh
        };
        let id = native_ids::digest(
            b"trellis.semantic-rejoin.v1\0",
            &native_ids::canonical(
                &serde_json::json!({"proof":raw_id,"revision":destination_native.revision.id,"kind":kind}),
            ),
        );
        ids.insert((raw_id.clone(), kind), id.clone());
        provenance.push(semantic::Provenance {
            id,
            producer_id: producer.id.clone(),
            document: doc,
            revision_id: destination_native.revision.id.clone(),
            content_hash: dst.content_hash.clone(),
            evidence_kind: kind,
            basis: Some(captured_basis.clone()),
            freshness,
            derived_from: Some(semantic::DerivedFrom {
                provenance_id: raw_id,
                record_kind: serde_json::from_value(serde_json::json!(claim.envelope.record_kind))?,
                fact_digest: claim.fact_digest.clone(),
            }),
        });
    }
    let mut outcomes = BTreeMap::<String, usize>::new();
    for claim in inventory
        .claims
        .iter()
        .filter(|c| c.envelope.record_kind != "symbolInformation")
    {
        outcomes.insert(claim.captured_proof_id.clone(), 2);
    }
    for raw_id in &unsupported_claims {
        outcomes.insert(raw_id.clone(), 4);
    }
    for row in &bindings {
        if row.syntax_id.is_some() {
            outcomes.insert(row.provenance_id.clone(), 1);
        }
    }
    for row in &references {
        outcomes.insert(
            row.provenance_id.clone(),
            if row.resolution == semantic::Resolution::Ambiguous {
                3
            } else {
                1
            },
        );
    }
    for row in &calls {
        outcomes.insert(
            row.provenance_id.clone(),
            if row.resolution == semantic::Resolution::Ambiguous {
                3
            } else {
                1
            },
        );
    }
    for raw_id in &additional_reference_proofs {
        outcomes.insert(raw_id.clone(), 3);
    }
    for row in &relationships {
        outcomes.insert(row.provenance_id.clone(), 1);
    }
    for row in &mut symbol_rows {
        row.provenance_id = ids[&(
            row.provenance_id.clone(),
            semantic::EvidenceKind::DeclarationBinding,
        )]
            .clone();
    }
    for row in &mut bindings {
        row.provenance_id = ids[&(
            row.provenance_id.clone(),
            semantic::EvidenceKind::DeclarationBinding,
        )]
            .clone();
    }
    for row in &mut references {
        row.provenance_id = ids[&(
            row.provenance_id.clone(),
            semantic::EvidenceKind::SemanticReference,
        )]
            .clone();
    }
    for row in &mut calls {
        row.provenance_id = ids[&(
            row.provenance_id.clone(),
            semantic::EvidenceKind::SemanticReference,
        )]
            .clone();
    }
    for row in &mut relationships {
        row.provenance_id = ids[&(
            row.provenance_id.clone(),
            semantic::EvidenceKind::TypeRelationship,
        )]
            .clone();
    }
    let mut coverage = Vec::new();
    for doc in &destination_native.revision.documents {
        let language: semantic::Language =
            serde_json::from_value(serde_json::json!(doc.key.language))?;
        if !producer.languages.contains(&language) {
            continue;
        }
        let requested = requested.contains(&doc.key);
        let failed = failed.contains(&doc.key);
        let excluded = exclusions.get(&(doc.key.language.as_str(), doc.key.path.as_str()));
        let partial = excluded == Some(&"unsupported")
            || unsupported_documents.contains(&doc.key)
            || omitted.contains(&Some(doc.key.clone()))
            || facts.diagnostics.contains_key(&doc.key);
        let state = if !requested {
            semantic::CoverageState::NotRequested
        } else if failed {
            semantic::CoverageState::Failed
        } else if excluded == Some(&"unsupported") {
            semantic::CoverageState::Omitted
        } else if partial {
            semantic::CoverageState::Partial
        } else {
            semantic::CoverageState::Complete
        };
        let mut observed = BTreeSet::new();
        if bindings
            .iter()
            .any(|b| b.join.anchor.document == doc.key && b.syntax_id.is_some())
        {
            observed.insert(semantic::Role::Definition);
        }
        for row in references.iter().filter(|r| r.document == doc.key) {
            observed.extend(row.roles.iter().copied());
        }
        if calls.iter().any(|b| b.join.anchor.document == doc.key) {
            observed.insert(semantic::Role::Call);
        }
        if relationships.iter().any(|r| {
            provenance
                .iter()
                .any(|p| p.id == r.provenance_id && p.document == doc.key)
        }) {
            observed.insert(semantic::Role::Type);
        }
        let supported_roles = [
            semantic::Role::Definition,
            semantic::Role::Read,
            semantic::Role::Write,
            semantic::Role::Call,
            semantic::Role::Type,
            semantic::Role::Import,
            semantic::Role::Alias,
        ]
        .into_iter()
        .filter(|r| language != semantic::Language::Java || *r != semantic::Role::Alias)
        .collect();
        coverage.push(semantic::Coverage {
            producer_id: producer.id.clone(),
            language,
            source_set_id: doc.key.source_set_id.clone(),
            document_path: doc.key.path.clone(),
            revision_id: destination_native.revision.id.clone(),
            requested,
            selected: requested && state != semantic::CoverageState::Omitted,
            state,
            supported_roles,
            observed_roles: observed.into_iter().collect(),
            diagnostic: if failed {
                Some(
                    if excluded == Some(&"failed") {
                        "excluded_failed_by_producer"
                    } else {
                        "unmeasurable_source_anchor"
                    }
                    .into(),
                )
            } else if excluded == Some(&"unsupported") {
                Some("excluded_by_producer".into())
            } else if unsupported_documents.contains(&doc.key) {
                Some("unsupported_native_gap".into())
            } else if partial {
                Some("unmeasurable_internal_target".into())
            } else {
                None
            },
        });
    }
    let mut document_counts = Vec::new();
    let mut language_counts: BTreeMap<semantic::Language, [usize; 5]> = BTreeMap::new();
    for row in &coverage {
        let mut counts = [0usize; 5];
        for claim in inventory.claims.iter().filter(|c| {
            c.language
                == serde_json::to_value(row.language)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
                && c.envelope.relative_path == row.document_path
                && c.envelope.record_kind != "symbolInformation"
        }) {
            counts[0] += 1;
            let outcome = if row.state == semantic::CoverageState::Failed {
                2
            } else {
                *outcomes
                    .get(&claim.captured_proof_id)
                    .context("missing rejoin claim outcome")?
            };
            counts[outcome] += 1;
        }
        ensure!(
            counts[0] == counts[1] + counts[2] + counts[3] + counts[4],
            "incomplete semantic import claim tally"
        );
        let aggregate = language_counts.entry(row.language).or_default();
        for (total, count) in aggregate.iter_mut().zip(counts) {
            *total += count;
        }
        document_counts.push((
            row.language,
            row.document_path.clone(),
            serde_json::json!({
                "language": row.language, "path": row.document_path, "state": row.state,
                "supportedRoles": row.supported_roles, "observedRoles": row.observed_roles,
                "diagnostic": row.diagnostic, "requested": counts[0], "matched": counts[1],
                "skipped": counts[2], "ambiguous": counts[3], "unsupported": counts[4]
            }),
        ));
    }
    document_counts.sort_by(|a, b| (a.0, a.1.as_bytes()).cmp(&(b.0, b.1.as_bytes())));
    let languages: Vec<_> = language_counts
        .into_iter()
        .map(|(language, counts)| {
            serde_json::json!({
                "language":language,"requested":counts[0],"matched":counts[1],"skipped":counts[2],
                "ambiguous":counts[3],"unsupported":counts[4]
            })
        })
        .collect();
    #[cfg(feature = "test-support")]
    if let Ok(bytes) = std::env::var("TRELLIS_TEST_SEMANTIC_REPORT_PADDING")
        && let Ok(bytes) = bytes.parse::<usize>()
        && bytes <= 1024 * 1024
    {
        document_counts.push((
            semantic::Language::Java,
            "test-only report padding".into(),
            serde_json::json!({"diagnostic":"x".repeat(bytes)}),
        ));
    }
    let report = serde_json::json!({"producerId":producer.id,"sourceSetId":destination_native.revision.source_set_id,
        "capturedRevision":captured_native.revision.id,"destinationRevision":destination_native.revision.id,
        "languages":languages,"documents":document_counts.into_iter().map(|(_,_,row)|row).collect::<Vec<_>>()});
    ensure!(
        serde_json::to_vec(&report)?.len() <= 1024 * 1024,
        "semantic import report exceeds response bound"
    );
    Ok(RejoinResult {
        publication: semantic::SemanticPublication {
            producers: vec![producer],
            coverage,
            provenance,
            symbols: symbol_rows,
            declaration_bindings: bindings,
            references,
            call_bindings: calls,
            type_relationships: relationships,
        },
        report,
    })
}

fn manifest_hash(revision: &native::Revision) -> String {
    native_ids::digest(
        b"",
        &native_ids::canonical(&serde_json::json!(
            revision
                .documents
                .iter()
                .map(|d| serde_json::json!({"document":d.key,"contentHash":d.content_hash}))
                .collect::<Vec<_>>()
        )),
    )
}
