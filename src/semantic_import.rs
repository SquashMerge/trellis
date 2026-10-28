//! Exact measured joins for authenticated, document-local SCIP claims.
use crate::{
    native_evidence as native, native_ids,
    scip_raw::{CapturedClaim, CapturedScipInventory},
    semantic_evidence as semantic,
};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

mod join_store;
mod memory;
use join_store::{JoinStore, NativeFacts};
use memory::MemoryJoinStore;

type AnchorKey = (native::DocumentKey, usize, usize, semantic::JoinKind);
type ScopedKey = (String, Option<native::DocumentKey>);
type LocalDeclarations = BTreeMap<ScopedKey, BTreeSet<(native::DocumentKey, String)>>;

/// Import-local native indexes. Distinct native identities, not raw-claim counts,
/// determine exactness and ambiguity.
#[derive(Clone)]
pub struct MeasuredFacts<'a> {
    native: &'a native::Artifact,
    sources: BTreeMap<native::DocumentKey, &'a str>,
    documents: BTreeMap<native::DocumentKey, &'a native::Document>,
    candidates: BTreeMap<AnchorKey, BTreeSet<String>>,
    declarations: BTreeMap<String, Vec<&'a native::Declaration>>,
    declaration_names:
        BTreeMap<(native::DocumentKey, usize, usize, String), Vec<&'a native::Declaration>>,
    declaration_keys: BTreeMap<(native::DocumentKey, Vec<u8>), Vec<&'a native::Declaration>>,
    calls: BTreeMap<String, Vec<&'a native::Call>>,
    references: BTreeMap<String, Vec<&'a native::Reference>>,
    support: BTreeMap<(native::DocumentKey, semantic::JoinKind), &'a native::MeasurementSupport>,
}

impl<'a> MeasuredFacts<'a> {
    pub fn new(
        native: &'a native::Artifact,
        sources: &'a BTreeMap<native::DocumentKey, Vec<u8>>,
    ) -> Result<Self> {
        #[cfg(feature = "test-support")]
        count_publication_work(|work| {
            if work.destination_revision == native.revision.id {
                work.destination_index_builds += 1;
                work.destination_document_rows += native.revision.documents.len();
            } else {
                work.original_index_builds += 1;
            }
        });
        let mut documents = BTreeMap::new();
        let mut source_text = BTreeMap::new();
        for doc in &native.revision.documents {
            let bytes = sources
                .get(&doc.key)
                .context("missing authenticated native source")?;
            ensure!(
                bytes.len() == doc.byte_length
                    && native_ids::digest(b"", bytes) == doc.content_hash,
                "native source hash mismatch"
            );
            let text = std::str::from_utf8(bytes).context("non-UTF8 native source")?;
            source_text.entry(doc.key.clone()).or_insert(text);
            documents.entry(doc.key.clone()).or_insert(doc);
        }
        let mut declarations: BTreeMap<String, Vec<&native::Declaration>> = BTreeMap::new();
        let mut calls: BTreeMap<String, Vec<&native::Call>> = BTreeMap::new();
        let mut references: BTreeMap<String, Vec<&native::Reference>> = BTreeMap::new();
        let mut declaration_names: BTreeMap<_, Vec<&native::Declaration>> = BTreeMap::new();
        let mut declaration_keys: BTreeMap<_, Vec<&native::Declaration>> = BTreeMap::new();
        let mut anchors: BTreeMap<AnchorKey, BTreeSet<String>> = BTreeMap::new();
        let mut insert = |doc: &native::DocumentKey, range: &native::Range, kind, id: &str| {
            anchors
                .entry((doc.clone(), range.start, range.end, kind))
                .or_default()
                .insert(id.to_owned());
        };
        for d in &native.declarations {
            declarations.entry(d.syntax_id.clone()).or_default().push(d);
            declaration_keys
                .entry((
                    d.document.clone(),
                    native_ids::canonical(&serde_json::to_value(&d.key)?),
                ))
                .or_default()
                .push(d);
            if let (Some(range), Some(name)) = (&d.name_range, &d.name) {
                declaration_names
                    .entry((d.document.clone(), range.start, range.end, name.clone()))
                    .or_default()
                    .push(d);
            }
            if let Some(range) = &d.name_range {
                insert(
                    &d.document,
                    range,
                    semantic::JoinKind::DeclarationName,
                    &d.syntax_id,
                );
            }
        }
        for c in &native.calls {
            calls.entry(c.id.clone()).or_default().push(c);
            if let Some(range) = &c.callee_range {
                insert(&c.document, range, semantic::JoinKind::Callee, &c.id);
            }
            insert(&c.document, &c.range, semantic::JoinKind::Invocation, &c.id);
        }
        for r in &native.references {
            references.entry(r.id.clone()).or_default().push(r);
            insert(&r.document, &r.range, semantic::JoinKind::Reference, &r.id);
        }
        let mut raw_ranges: BTreeMap<_, Vec<(usize, usize)>> = BTreeMap::new();
        let mut candidates = BTreeMap::new();
        for ((document, start, end, kind), ids) in &anchors {
            raw_ranges
                .entry((document.clone(), *kind))
                .or_default()
                .push((*start, *end));
            #[cfg(test)]
            complexity_work(|work| work.native_gap_index_records += 1);
            let spelling = source_text
                .get(document)
                .and_then(|source| source.get(*start..*end));
            let lookup = if *kind == semantic::JoinKind::Reference {
                spelling.and_then(|s| native::lookup(&document.language, s).ok())
            } else {
                None
            };
            let compatible = ids
                .iter()
                .filter(|id| {
                    #[cfg(test)]
                    complexity_work(|work| work.native_candidates += 1);
                    match kind {
                        semantic::JoinKind::DeclarationName => {
                            declarations.get(*id).is_some_and(|rows| {
                                rows.iter().any(|d| {
                                    &d.document == document && d.name.as_deref() == spelling
                                })
                            })
                        }
                        semantic::JoinKind::Callee => calls.get(*id).is_some_and(|rows| {
                            rows.iter().any(|c| {
                                &c.document == document && c.spelling.as_deref() == spelling
                            })
                        }),
                        semantic::JoinKind::Invocation => calls.get(*id).is_some_and(|rows| {
                            rows.iter().any(|c| {
                                &c.document == document
                                    && c.range.start == *start
                                    && c.range.end == *end
                            })
                        }),
                        semantic::JoinKind::Reference => references.get(*id).is_some_and(|rows| {
                            rows.iter().any(|r| {
                                &r.document == document
                                    && spelling == Some(r.spelling.as_str())
                                    && lookup.as_deref() == Some(r.lookup_key.as_str())
                            })
                        }),
                    }
                })
                .cloned()
                .collect::<BTreeSet<_>>();
            candidates.insert((document.clone(), *start, *end, *kind), compatible);
        }
        for ranges in raw_ranges.values_mut() {
            ranges.sort_unstable();
            let mut min_end = usize::MAX;
            for (_, end) in ranges.iter_mut().rev() {
                #[cfg(test)]
                complexity_work(|work| work.native_gap_index_records += 1);
                min_end = min_end.min(*end);
                *end = min_end;
            }
        }
        let mut support = BTreeMap::new();
        let mut seen_reports = BTreeSet::new();
        for report in &native.capability_reports {
            if report.revision_id != native.revision.id
                || !seen_reports.insert(report.document.clone())
            {
                continue;
            }
            for family in &report.families {
                let kind = match family.kind {
                    native::MeasurementKind::DeclarationName => semantic::JoinKind::DeclarationName,
                    native::MeasurementKind::Callee => semantic::JoinKind::Callee,
                    native::MeasurementKind::Invocation => semantic::JoinKind::Invocation,
                    native::MeasurementKind::Reference => semantic::JoinKind::Reference,
                };
                // Preserve the first report/family selected by the former vector lookup.
                support
                    .entry((report.document.clone(), kind))
                    .or_insert(family);
            }
        }
        for ((document, kind), family) in &support {
            let source = source_text.get(document).context("missing anchor bytes")?;
            for gap in &family.gaps {
                ensure!(
                    gap.range.start < gap.range.end
                        && source.get(gap.range.start..gap.range.end).is_some(),
                    "invalid native capability gap"
                );
                #[cfg(test)]
                complexity_work(|work| work.native_gap_queries += 1);
                let contradicts =
                    raw_ranges
                        .get(&(document.clone(), *kind))
                        .is_some_and(|ranges| {
                            let first = ranges.partition_point(|(start, _)| {
                                #[cfg(test)]
                                complexity_work(|work| work.native_gap_query_records += 1);
                                *start < gap.range.start
                            });
                            ranges.get(first).is_some_and(|(_, min_end)| {
                                #[cfg(test)]
                                complexity_work(|work| work.native_gap_query_records += 1);
                                *min_end <= gap.range.end
                            })
                        });
                ensure!(
                    !contradicts,
                    "native capability gap contradicts measured candidates"
                );
            }
        }
        Ok(Self {
            native,
            sources: source_text,
            documents,
            candidates,
            declarations,
            declaration_names,
            declaration_keys,
            calls,
            references,
            support,
        })
    }

    fn declaration(&self, id: &str) -> Option<&'a native::Declaration> {
        self.declarations
            .get(id)
            .and_then(|rows| rows.first().copied())
    }

    fn call(&self, id: &str) -> Option<&'a native::Call> {
        self.calls.get(id).and_then(|rows| rows.first().copied())
    }

    fn reference(&self, id: &str) -> Option<&'a native::Reference> {
        self.references
            .get(id)
            .and_then(|rows| rows.first().copied())
    }

    pub fn join(
        &self,
        document: &native::DocumentKey,
        range: native::Range,
        kind: semantic::JoinKind,
    ) -> Result<semantic::Join> {
        #[cfg(feature = "test-support")]
        count_publication_work(|work| {
            if work.destination_revision == self.native.revision.id
                && work.reused_documents.contains(document)
            {
                work.destination_joins_on_reused_documents += 1;
            }
        });
        let doc = self
            .documents
            .get(document)
            .context("foreign anchor document")?;
        let source = self.sources.get(document).context("missing anchor bytes")?;
        source
            .get(range.start..range.end)
            .filter(|s| !s.is_empty())
            .context("invalid UTF-8 scalar anchor")?;
        let ids = self
            .candidates
            .get(&(document.clone(), range.start, range.end, kind))
            .into_iter()
            .flat_map(|ids| ids.iter())
            .cloned()
            .collect::<Vec<_>>();
        let support = self
            .support
            .get(&(document.clone(), kind))
            .context("missing native measurement family")?;
        ensure!(
            support.available || ids.is_empty() && support.gaps.is_empty(),
            "unavailable native family has candidates or gaps"
        );
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

type Position = (i32, i32);
type Endpoints = (Position, Position);
type CoordinateMap = BTreeMap<Position, usize>;

fn occurrence_endpoints(claim: &CapturedClaim, admitted_encoding: u8) -> Result<(u8, Endpoints)> {
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
    let endpoints = if values.len() == 3 {
        (
            (i32::try_from(values[0])?, i32::try_from(values[1])?),
            (i32::try_from(values[0])?, i32::try_from(values[2])?),
        )
    } else {
        (
            (i32::try_from(values[0])?, i32::try_from(values[1])?),
            (i32::try_from(values[2])?, i32::try_from(values[3])?),
        )
    };
    Ok((encoding, endpoints))
}

fn mapped_range(endpoints: Endpoints, offsets: &CoordinateMap) -> Result<native::Range> {
    let start = *offsets
        .get(&endpoints.0)
        .context("missing SCIP start position")?;
    let end = *offsets
        .get(&endpoints.1)
        .context("missing SCIP end position")?;
    ensure!(start < end, "empty or reversed SCIP range");
    Ok(native::Range { start, end })
}

/// Convert one standalone range. Import loops use CoordinateMaps instead.
pub fn occurrence_range(
    claim: &CapturedClaim,
    source: &[u8],
    admitted_encoding: u8,
) -> Result<native::Range> {
    let (encoding, endpoints) = occurrence_endpoints(claim, admitted_encoding)?;
    let text = std::str::from_utf8(source).context("non-UTF8 authenticated source")?;
    let offsets = crate::scip_raw::source_byte_offsets(
        text,
        encoding,
        &mut vec![endpoints.0, endpoints.1],
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )?;
    mapped_range(endpoints, &offsets)
}

struct CoordinateMaps {
    offsets: BTreeMap<(String, String, u8), CoordinateMap>,
}

impl CoordinateMaps {
    fn new(
        inventory: &CapturedScipInventory,
        native: &native::Artifact,
        sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
        admitted_encoding: u8,
    ) -> Result<Self> {
        let mut positions: BTreeMap<(String, String, u8), Vec<Position>> = BTreeMap::new();
        for claim in &inventory.claims {
            if claim.envelope.record_kind != "occurrence" {
                continue;
            }
            for enclosing in [false, true] {
                if enclosing && claim.envelope.raw["enclosingRange"].is_null() {
                    continue;
                }
                let mut envelope_claim;
                let range_claim = if enclosing {
                    envelope_claim = claim.clone();
                    envelope_claim.envelope.raw["range"] =
                        claim.envelope.raw["enclosingRange"].clone();
                    &envelope_claim
                } else {
                    claim
                };
                let (encoding, endpoints) = occurrence_endpoints(range_claim, admitted_encoding)?;
                positions
                    .entry((
                        claim.language.clone(),
                        claim.envelope.relative_path.clone(),
                        encoding,
                    ))
                    .or_default()
                    .extend([endpoints.0, endpoints.1]);
            }
        }
        let mut offsets = BTreeMap::new();
        for ((language, path, encoding), mut positions) in positions {
            let document = native::DocumentKey {
                source_set_id: native.revision.source_set_id.clone(),
                language: language.clone(),
                path: path.clone(),
            };
            let bytes = sources
                .get(&document)
                .context("missing admitted claim document")?;
            let source = std::str::from_utf8(bytes).context("non-UTF8 authenticated source")?;
            #[cfg(test)]
            complexity_work(|work| work.coordinate_batches += 1);
            let map = crate::scip_raw::source_byte_offsets(
                source,
                encoding,
                &mut positions,
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )?;
            offsets.insert((language, path, encoding), map);
        }
        Ok(Self { offsets })
    }

    fn range(&self, claim: &CapturedClaim, admitted_encoding: u8) -> Result<native::Range> {
        let (encoding, endpoints) = occurrence_endpoints(claim, admitted_encoding)?;
        let offsets = self
            .offsets
            .get(&(
                claim.language.clone(),
                claim.envelope.relative_path.clone(),
                encoding,
            ))
            .context("missing SCIP coordinate map")?;
        mapped_range(endpoints, offsets)
    }
}

struct InventoryIndex<'a> {
    proofs: BTreeMap<(String, String, String), &'a CapturedClaim>,
    by_proof: BTreeMap<String, &'a CapturedClaim>,
    symbol_info: BTreeMap<(String, String, String), &'a CapturedClaim>,
    declared_symbols: BTreeSet<String>,
    information_symbols: BTreeSet<String>,
}

impl<'a> InventoryIndex<'a> {
    fn new(inventory: &'a CapturedScipInventory) -> Result<Self> {
        let mut index = Self {
            proofs: BTreeMap::new(),
            by_proof: BTreeMap::new(),
            symbol_info: BTreeMap::new(),
            declared_symbols: BTreeSet::new(),
            information_symbols: BTreeSet::new(),
        };
        for claim in &inventory.claims {
            #[cfg(test)]
            complexity_work(|work| work.raw_claims += 1);
            let key = (
                claim.captured_proof_id.clone(),
                claim.envelope.record_kind.clone(),
                claim.fact_digest.clone(),
            );
            if index.proofs.insert(key, claim).is_some() {
                return Err(crate::scip_raw::ScipCaptureError::NonUniqueProof.into());
            }
            index
                .by_proof
                .entry(claim.captured_proof_id.clone())
                .or_insert(claim);
            if let Some(symbol) = claim.envelope.raw["symbol"].as_str() {
                if claim.envelope.record_kind == "symbolInformation" {
                    index.information_symbols.insert(symbol.to_owned());
                    index
                        .symbol_info
                        .entry((
                            claim.language.clone(),
                            claim.envelope.relative_path.clone(),
                            symbol.to_owned(),
                        ))
                        .or_insert(claim);
                } else if claim.envelope.record_kind == "occurrence"
                    && claim.envelope.raw["symbolRoles"]
                        .as_u64()
                        .is_some_and(|roles| roles & 1 != 0)
                {
                    index.declared_symbols.insert(symbol.to_owned());
                }
            }
        }
        Ok(index)
    }

    fn lookup(&self, proof: &str, kind: &str, digest: &str) -> Result<&'a CapturedClaim> {
        #[cfg(test)]
        complexity_work(|work| work.raw_lookups += 1);
        self.proofs
            .get(&(proof.to_owned(), kind.to_owned(), digest.to_owned()))
            .copied()
            .ok_or_else(|| crate::scip_raw::ScipCaptureError::MissingProof.into())
    }
}

#[cfg(test)]
#[derive(Default, Clone, Copy, Debug)]
struct ComplexityWork {
    coordinate_batches: usize,
    native_candidates: usize,
    native_gap_index_records: usize,
    native_gap_queries: usize,
    native_gap_query_records: usize,
    raw_claims: usize,
    raw_lookups: usize,
    reference_map_lookups: usize,
    method_index_rows: usize,
    method_lookup_rows: usize,
    java_type_parses: usize,
}
#[cfg(test)]
thread_local! { static COMPLEXITY_WORK: std::cell::RefCell<ComplexityWork> = std::cell::RefCell::new(ComplexityWork::default()); }
#[cfg(test)]
fn complexity_work(update: impl FnOnce(&mut ComplexityWork)) {
    COMPLEXITY_WORK.with(|work| update(&mut work.borrow_mut()));
}

#[cfg(feature = "test-support")]
#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicationWork {
    destination_index_builds: usize,
    full_fallbacks: usize,
    witnessed_rejoins: usize,
    fallback_reasons: Vec<String>,
    original_index_builds: usize,
    destination_role_parses: usize,
    original_role_parses: usize,
    original_native_rows: usize,
    original_native_documents: Vec<native::DocumentKey>,
    original_source_bytes: usize,
    original_source_documents: Vec<native::DocumentKey>,
    original_raw_proof_bytes: usize,
    original_raw_envelope_parses: usize,
    original_raw_decodes: usize,
    original_source_hash_documents: Vec<native::DocumentKey>,
    parsed_role_site_documents: Vec<native::DocumentKey>,
    parsed_type_documents: Vec<native::DocumentKey>,
    current_target_lookups: Vec<native::DocumentKey>,
    current_target_rows: usize,
    destination_joins_on_reused_documents: usize,
    destination_document_rows: usize,
    destination_revision: String,
    reused_documents: BTreeSet<native::DocumentKey>,
}
#[cfg(feature = "test-support")]
thread_local! { static PUBLICATION_WORK: std::cell::RefCell<PublicationWork> =
std::cell::RefCell::new(PublicationWork::default()); }
#[cfg(feature = "test-support")]
pub(crate) fn reset_publication_work(revision: &str, reused: BTreeSet<native::DocumentKey>) {
    PUBLICATION_WORK.with(|cell| {
        *cell.borrow_mut() = PublicationWork {
            destination_revision: revision.to_owned(),
            reused_documents: reused,
            ..PublicationWork::default()
        }
    });
}
#[cfg(feature = "test-support")]
pub(crate) fn set_publication_reused(reused: BTreeSet<native::DocumentKey>) {
    count_publication_work(|work| work.reused_documents = reused);
}
#[cfg(feature = "test-support")]
pub(crate) fn note_selected_original_native_row(doc: &native::DocumentKey) {
    count_publication_work(|work| {
        work.original_native_rows += 1;
        work.original_native_documents.push(doc.clone());
    });
}
#[cfg(feature = "test-support")]
pub(crate) fn note_selected_original_source_bytes(doc: &native::DocumentKey, length: usize) {
    count_publication_work(|work| {
        work.original_source_bytes += length;
        work.original_source_documents.push(doc.clone());
    });
}
#[cfg(feature = "test-support")]
pub(crate) fn note_selected_original_raw_proof_bytes(length: usize) {
    count_publication_work(|work| work.original_raw_proof_bytes += length);
}
#[cfg(feature = "test-support")]
pub(crate) fn note_original_raw_envelope_parse() {
    count_publication_work(|work| work.original_raw_envelope_parses += 1);
}
#[cfg(feature = "test-support")]
pub(crate) fn note_original_raw_decode() {
    count_publication_work(|work| work.original_raw_decodes += 1);
}
#[cfg(feature = "test-support")]
pub(crate) fn note_original_source_hash(doc: &native::DocumentKey) {
    count_publication_work(|work| work.original_source_hash_documents.push(doc.clone()));
}
#[cfg(feature = "test-support")]
pub(crate) fn note_parsed_role_site_document(doc: &native::DocumentKey) {
    count_publication_work(|work| work.parsed_role_site_documents.push(doc.clone()));
}
#[cfg(feature = "test-support")]
pub(crate) fn note_parsed_type_document(doc: &native::DocumentKey) {
    count_publication_work(|work| work.parsed_type_documents.push(doc.clone()));
}
#[cfg(feature = "test-support")]
pub(crate) fn note_current_target_lookup(doc: &native::DocumentKey) {
    count_publication_work(|work| work.current_target_lookups.push(doc.clone()));
}
#[cfg(feature = "test-support")]
pub(crate) fn note_current_target_rows(rows: usize) {
    count_publication_work(|work| work.current_target_rows += rows);
}
#[cfg(feature = "test-support")]
pub(crate) fn note_full_fallback() {
    count_publication_work(|work| work.full_fallbacks += 1);
}
#[cfg(feature = "test-support")]
pub(crate) fn note_witnessed_rejoin() {
    count_publication_work(|work| work.witnessed_rejoins += 1);
}
#[cfg(feature = "test-support")]
pub(crate) fn clear_witnessed_rejoins() {
    count_publication_work(|work| work.witnessed_rejoins = 0);
}
#[cfg(feature = "test-support")]
pub(crate) fn note_fallback_reason(reason: &str) {
    count_publication_work(|work| work.fallback_reasons.push(reason.to_owned()));
}
#[cfg(feature = "test-support")]
pub(crate) fn publication_work() -> serde_json::Value {
    PUBLICATION_WORK
        .with(|cell| serde_json::to_value(&*cell.borrow()).expect("work counters serialize"))
}
#[cfg(feature = "test-support")]
pub(crate) fn publication_work_checkpoint() -> PublicationWork {
    PUBLICATION_WORK.with(|cell| cell.borrow().clone())
}
#[cfg(feature = "test-support")]
pub(crate) fn restore_publication_work(checkpoint: PublicationWork) {
    PUBLICATION_WORK.with(|cell| *cell.borrow_mut() = checkpoint);
}
#[cfg(feature = "test-support")]
fn count_publication_work(update: impl FnOnce(&mut PublicationWork)) {
    PUBLICATION_WORK.with(|cell| update(&mut cell.borrow_mut()));
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

#[derive(Clone, Debug, Default)]
pub struct JoinedFacts {
    pub symbols: Vec<semantic::Symbol>,
    pub declaration_bindings: Vec<semantic::DeclarationBinding>,
    pub references: Vec<semantic::Reference>,
    pub reference_claims: BTreeMap<String, semantic::Reference>,
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
    let raw_index = InventoryIndex::new(inventory)?;
    let coordinates = CoordinateMaps::new(inventory, native, sources, admitted_encoding)?;
    let measured = MeasuredFacts::new(native, sources)?;
    let store = MemoryJoinStore::new(&raw_index, &coordinates, &measured);
    join_inventory_indexed(inventory, native, sources, admitted_encoding, &store)
}

fn join_inventory_indexed<S: JoinStore>(
    inventory: &CapturedScipInventory,
    native: &native::Artifact,
    sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    admitted_encoding: u8,
    store: &S,
) -> Result<JoinedFacts> {
    let mut out = JoinedFacts::default();
    let mut declarations: LocalDeclarations = BTreeMap::new();
    let mut type_uses: BTreeMap<(native::DocumentKey, usize, usize), BTreeSet<String>> =
        BTreeMap::new();
    let mut occurrences = Vec::new();
    for claim in &inventory.claims {
        store.charge(1)?;
        store.lookup_proof(
            &claim.captured_proof_id,
            &claim.envelope.record_kind,
            &claim.fact_digest,
        )?;
        let doc = native::DocumentKey {
            source_set_id: native.revision.source_set_id.clone(),
            language: claim.language.clone(),
            path: claim.envelope.relative_path.clone(),
        };
        sources
            .get(&doc)
            .context("missing admitted claim document")?;
        if claim.envelope.record_kind == "occurrence" {
            let symbol = claim.envelope.raw["symbol"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("missing occurrence symbol")?;
            let range = store.occurrence_range(claim, admitted_encoding)?;
            #[cfg(feature = "test-support")]
            count_publication_work(|work| {
                if work.destination_revision == native.revision.id {
                    work.destination_role_parses += 1;
                } else {
                    work.original_role_parses += 1;
                }
            });
            let roles = claim.envelope.raw["symbolRoles"]
                .as_u64()
                .context("invalid occurrence roles")?;
            let _syntax_kind = claim.envelope.raw["syntaxKind"]
                .as_i64()
                .context("invalid syntax kind")?;
            let declaration = roles & 1 != 0;
            let binding = store.join(&doc, range.clone(), semantic::JoinKind::DeclarationName)?;
            let reference = store.join(&doc, range.clone(), semantic::JoinKind::Reference)?;
            let call = store.join(&doc, range.clone(), semantic::JoinKind::Callee)?;
            if declaration && binding.status == semantic::JoinStatus::Exact {
                let candidate = store
                    .declaration(&binding.candidate_ids[0])
                    .context("missing exact native declaration")?;
                let symbol_info = store.symbol_info(&doc.language, &doc.path, symbol);
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
        .map(|(doc, bytes)| Ok((doc.clone(), store.role_sites(doc, bytes)?)))
        .collect::<Result<_>>()?;
    let mut call_reference_ids: BTreeMap<String, String> = BTreeMap::new();
    let mut reference_indices: BTreeMap<String, usize> = BTreeMap::new();
    for (claim, doc, symbol, roles, binding, reference, call) in occurrences {
        let key = symbol_key(&symbol, &doc);
        let mut asserted = targets(&doc, &symbol);
        if asserted.is_empty()
            && key.scope == semantic::SymbolScope::Global
            && !store.is_declared_symbol(&symbol)
            && !store.is_information_symbol(&symbol)
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
            let mut enclosing_claim = claim.clone();
            enclosing_claim.envelope.raw["range"] = claim.envelope.raw["enclosingRange"].clone();
            let span = store.occurrence_range(&enclosing_claim, admitted_encoding)?;
            Some(store.join(&doc, span, semantic::JoinKind::Invocation)?)
        } else {
            None
        };
        if reference.status == semantic::JoinStatus::Exact {
            let r = store
                .reference(&reference.candidate_ids[0])
                .context("missing indexed native reference")?;
            let declaration_site = roles & 1 != 0
                && store.has_declaration_name(&doc, r.range.start, r.range.end, &r.spelling);
            let is_call = invocation.as_ref().is_some_and(|j| {
                j.status == semantic::JoinStatus::Exact
                    && store.calls_by_id(&j.candidate_ids[0]).iter().any(|c| {
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
                && store
                    .declarations_by_id(&binding.candidate_ids[0])
                    .iter()
                    .any(|d| {
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
            if !assigned.is_empty() {
                let reference = semantic::Reference {
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
                    roles: assigned.iter().copied().collect(),
                    resolution,
                    declared_target: resolved.clone(),
                    candidates: candidates.clone(),
                    provenance_id: claim.captured_proof_id.clone(),
                };
                out.reference_claims
                    .insert(claim.captured_proof_id.clone(), reference.clone());
                #[cfg(test)]
                complexity_work(|work| work.reference_map_lookups += 1);
                if let Some(&index) = reference_indices.get(&r.id) {
                    let prior = &mut out.references[index];
                    assigned.extend(prior.roles.iter().copied());
                    prior.roles = assigned.into_iter().collect();
                    if prior.roles.contains(&semantic::Role::Definition) {
                        prior.site = semantic::ReferenceSite::Declaration;
                    }
                } else {
                    reference_indices.insert(r.id.clone(), out.references.len());
                    out.references.push(reference);
                }
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
                let actual = store.call(&candidate.candidate_ids[0]);
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
                        && store
                            .references_by_id(&reference.candidate_ids[0])
                            .iter()
                            .any(|r| {
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
            .and_then(|id| {
                #[cfg(test)]
                complexity_work(|work| work.reference_map_lookups += 1);
                reference_indices.get(id)
            })
            .map(|&index| &out.references[index])
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
    let mut source_methods: BTreeMap<_, Vec<&native::Declaration>> = BTreeMap::new();
    let mut target_methods: BTreeMap<_, Vec<&native::Declaration>> = BTreeMap::new();
    let mut document_types: BTreeMap<_, Vec<&native::Declaration>> = BTreeMap::new();
    for declaration in &native.declarations {
        #[cfg(test)]
        complexity_work(|work| work.method_index_rows += 1);
        if declaration.kind == "type" {
            document_types
                .entry(declaration.document.clone())
                .or_default()
                .push(declaration);
        }
        if declaration.kind != "method" {
            continue;
        }
        let Some(range) = &declaration.name_range else {
            continue;
        };
        if let Some(symbols) =
            type_uses.get(&(declaration.document.clone(), range.start, range.end))
        {
            for symbol in symbols {
                source_methods
                    .entry((declaration.document.clone(), symbol.clone()))
                    .or_default()
                    .push(declaration);
                target_methods
                    .entry((
                        declaration.document.source_set_id.clone(),
                        scoped_key(symbol, &declaration.document),
                    ))
                    .or_default()
                    .push(declaration);
            }
        }
    }
    let mut relationships: BTreeMap<_, Vec<&CapturedClaim>> = BTreeMap::new();
    let mut relationship_endpoints: BTreeMap<_, Vec<&CapturedClaim>> = BTreeMap::new();
    for claim in &inventory.claims {
        if claim.envelope.record_kind != "relationship" {
            continue;
        }
        let document = (claim.language.clone(), claim.envelope.relative_path.clone());
        relationships
            .entry(document.clone())
            .or_default()
            .push(claim);
        if let (Some(source), Some(target)) = (
            claim.envelope.raw["sourceSymbol"].as_str(),
            claim.envelope.raw["targetSymbol"].as_str(),
        ) {
            relationship_endpoints
                .entry((document, source.to_owned(), target.to_owned()))
                .or_default()
                .push(claim);
        }
    }
    for (doc, bytes) in sources {
        #[cfg(feature = "test-support")]
        if matches!(doc.language.as_str(), "java" | "javascript" | "python") {
            note_parsed_type_document(doc);
        }
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
        let mut java_bases = BTreeMap::new();
        if doc.language == "java" {
            for (owner, base, _) in &bases {
                java_bases
                    .entry((owner.start, owner.end))
                    .or_insert_with(|| base.clone());
            }
        }
        let document = (doc.language.clone(), doc.path.clone());
        let document_relationships = relationships
            .get(&document)
            .map(Vec::as_slice)
            .unwrap_or_default();
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
                && !store.is_information_symbol(target_symbol)
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
            for claim in relationship_endpoints
                .get(&(
                    document.clone(),
                    source_symbol.clone(),
                    target_symbol.clone(),
                ))
                .into_iter()
                .flatten()
            {
                out.type_relationships.push(semantic::TypeRelationship {
                    kind,
                    source: source.clone(),
                    target: target.clone(),
                    provenance_id: claim.captured_proof_id.clone(),
                });
            }
        }
        if doc.language == "java" {
            for claim in document_relationships {
                let raw = &claim.envelope.raw;
                let (Some(source_symbol), Some(target_symbol)) =
                    (raw["sourceSymbol"].as_str(), raw["targetSymbol"].as_str())
                else {
                    continue;
                };
                let source_methods = source_methods
                    .get(&(doc.clone(), source_symbol.to_owned()))
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                #[cfg(test)]
                complexity_work(|work| work.method_lookup_rows += source_methods.len());
                let [source_method] = source_methods else {
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
                let target_methods = target_methods
                    .get(&(doc.source_set_id.clone(), scoped_key(target_symbol, doc)))
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                #[cfg(test)]
                complexity_work(|work| work.method_lookup_rows += target_methods.len());
                if target_methods.len() > 1 {
                    out.diagnostics
                        .entry(doc.clone())
                        .or_default()
                        .insert("unmeasurable_override_endpoint".into());
                    continue;
                }
                let target_method = target_methods.first().copied();
                let source_owner = source_method.ancestors.last();
                let owner_rows = source_owner
                    .and_then(|key| serde_json::to_value(key).ok())
                    .map(|key| native_ids::canonical(&key))
                    .map(|key| store.declarations_by_key(doc, &key));
                let owner_declarations: Vec<_> = owner_rows
                    .into_iter()
                    .flatten()
                    .copied()
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
                    .and_then(|r| java_bases.get(&(r.start, r.end)))
                    .and_then(|range| type_uses.get(&(doc.clone(), range.start, range.end)))
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
                        method
                            .ancestors
                            .last()
                            .and_then(|key| serde_json::to_value(key).ok())
                            .map(|key| native_ids::canonical(&key))
                            .map(|key| store.declarations_by_key(&method.document, &key))
                            .into_iter()
                            .flatten()
                            .copied()
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
            for declaration in document_types.get(doc).into_iter().flatten().filter(|d| {
                std::str::from_utf8(bytes).ok().is_some_and(|source| {
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
                for claim in document_relationships.iter().filter(|c| {
                    c.envelope.raw["sourceSymbol"].as_str() == Some(source_symbol)
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
    #[cfg(test)]
    complexity_work(|work| work.java_type_parses += 1);
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

pub(crate) fn original_associations_from_selected(
    snapshot: &semantic::SemanticSnapshot,
    inventory: &crate::scip_raw::ProofInventory,
    producer_id: &str,
) -> Option<JoinedFacts> {
    let mut by_provenance = BTreeMap::new();
    for row in snapshot
        .provenance
        .iter()
        .filter(|row| row.producer_id == producer_id)
    {
        let derived = row.derived_from.as_ref()?;
        let kind = serde_json::to_value(derived.record_kind).ok()?;
        let claim = inventory
            .lookup(&derived.provenance_id, kind.as_str()?, &derived.fact_digest)
            .ok()?;
        if row.document.language != claim.language || row.document.path != claim.path {
            return None;
        }
        by_provenance.insert(row.id.as_str(), derived.provenance_id.as_str());
    }
    let expected: BTreeSet<_> = inventory
        .claims
        .iter()
        .map(|claim| claim.proof_id.as_str())
        .collect();
    let observed: BTreeSet<_> = by_provenance.values().copied().collect();
    if expected != observed {
        return None;
    }
    let mut facts = JoinedFacts::default();
    for row in &snapshot.symbols {
        if let Some(raw) = by_provenance.get(row.provenance_id.as_str()) {
            let mut value = row.clone();
            value.provenance_id = (*raw).to_owned();
            facts.symbols.push(value);
        }
    }
    for row in &snapshot.declaration_bindings {
        if let Some(raw) = by_provenance.get(row.provenance_id.as_str()) {
            let mut value = row.clone();
            value.provenance_id = (*raw).to_owned();
            facts.declaration_bindings.push(value);
        }
    }
    for row in &snapshot.references {
        if let Some(raw) = by_provenance.get(row.provenance_id.as_str()) {
            if facts.reference_claims.contains_key(*raw) {
                return None;
            }
            let mut value = row.clone();
            value.provenance_id = (*raw).to_owned();
            facts
                .reference_claims
                .insert((*raw).to_owned(), value.clone());
            facts.references.push(value);
        }
    }
    for row in &snapshot.call_bindings {
        if let Some(raw) = by_provenance.get(row.provenance_id.as_str()) {
            let mut value = row.clone();
            value.provenance_id = (*raw).to_owned();
            facts.call_bindings.push(value);
        }
    }
    for row in &snapshot.type_relationships {
        if let Some(raw) = by_provenance.get(row.provenance_id.as_str()) {
            let mut value = row.clone();
            value.provenance_id = (*raw).to_owned();
            facts.type_relationships.push(value);
        }
    }
    // A selected tuple is not a complete original inventory when claims were
    // fused, omitted, unsupported or unmatched. Fall back instead of guessing.
    let fact_proofs: BTreeSet<_> = facts
        .symbols
        .iter()
        .map(|row| row.provenance_id.as_str())
        .chain(
            facts
                .declaration_bindings
                .iter()
                .map(|row| row.provenance_id.as_str()),
        )
        .chain(facts.reference_claims.keys().map(String::as_str))
        .chain(
            facts
                .call_bindings
                .iter()
                .map(|row| row.provenance_id.as_str()),
        )
        .chain(
            facts
                .type_relationships
                .iter()
                .map(|row| row.provenance_id.as_str()),
        )
        .collect();
    (expected == fact_proofs).then_some(facts)
}

pub(crate) fn required_original_targets(
    snapshot: &semantic::SemanticSnapshot,
    inventory: &crate::scip_raw::ProofInventory,
    producer_id: &str,
) -> Option<BTreeSet<(native::DocumentKey, String)>> {
    let facts = original_associations_from_selected(snapshot, inventory, producer_id)?;
    let mut keys = BTreeSet::new();
    let mut add = |target: &semantic::Target| {
        if let semantic::Target::Internal {
            syntax_id,
            document,
            ..
        } = target
        {
            keys.insert((document.clone(), syntax_id.clone()));
        }
    };
    for symbol in &facts.symbols {
        for target in &symbol.declarations {
            add(target);
        }
    }
    for reference in &facts.references {
        if let Some(target) = &reference.declared_target {
            add(target);
        }
        for target in &reference.candidates {
            add(target);
        }
    }
    for call in &facts.call_bindings {
        if let Some(target) = &call.declared_target {
            add(target);
        }
        for target in &call.candidates {
            add(target);
        }
        for target in &call.possible_dispatch {
            add(target);
        }
    }
    for row in &facts.type_relationships {
        add(&row.source);
        add(&row.target);
    }
    Some(keys)
}

fn reuse_captured_source_associations(
    captured: &JoinedFacts,
    reusable: &BTreeSet<native::DocumentKey>,
    inventory: &crate::scip_raw::ProofInventory,
    source_set_id: &str,
    revision: &str,
) -> JoinedFacts {
    let mut facts = captured.clone();
    let is_reused = |doc: &native::DocumentKey| reusable.contains(doc);
    let proof_documents: BTreeMap<_, _> = inventory
        .claims
        .iter()
        .map(|claim| {
            (
                claim.proof_id.as_str(),
                native::DocumentKey {
                    source_set_id: source_set_id.to_owned(),
                    language: claim.language.clone(),
                    path: claim.path.clone(),
                },
            )
        })
        .collect();
    facts.symbols.retain(|row| {
        proof_documents
            .get(row.provenance_id.as_str())
            .is_some_and(is_reused)
    });
    facts
        .declaration_bindings
        .retain(|row| is_reused(&row.join.anchor.document));
    facts.references.retain(|row| is_reused(&row.document));
    facts
        .reference_claims
        .retain(|_, row| is_reused(&row.document));
    facts
        .call_bindings
        .retain(|row| is_reused(&row.join.anchor.document));
    facts.type_relationships.retain(|row| {
        proof_documents
            .get(row.provenance_id.as_str())
            .is_some_and(is_reused)
    });
    facts
        .joins
        .retain(|(_, join)| is_reused(&join.anchor.document));
    facts.diagnostics.retain(|doc, _| is_reused(doc));
    for (_, join) in &mut facts.joins {
        join.anchor.revision_id = revision.to_owned();
    }
    for binding in &mut facts.declaration_bindings {
        binding.join.anchor.revision_id = revision.to_owned();
    }
    for call in &mut facts.call_bindings {
        call.join.anchor.revision_id = revision.to_owned();
    }
    for reference in facts
        .references
        .iter_mut()
        .chain(facts.reference_claims.values_mut())
    {
        reference.revision_id = revision.to_owned();
    }
    facts
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
    let r2 = MeasuredFacts::new(destination_native, destination_sources)?;
    rejoin_inventory_with_destination(
        inventory,
        captured_native,
        captured_sources,
        destination_native,
        destination_sources,
        producer,
        basis,
        admitted_encoding,
        &r2,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rejoin_inventory_with_destination(
    inventory: &CapturedScipInventory,
    captured_native: &native::Artifact,
    captured_sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    destination_native: &native::Artifact,
    destination_sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    producer: semantic::Producer,
    basis: &BTreeMap<native::DocumentKey, semantic::SemanticBasis>,
    admitted_encoding: u8,
    r2: &MeasuredFacts<'_>,
) -> Result<RejoinResult> {
    rejoin_inventory_with_reuse(
        inventory,
        captured_native,
        captured_sources,
        destination_native,
        destination_sources,
        producer,
        basis,
        admitted_encoding,
        r2,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rejoin_inventory_with_reuse(
    inventory: &CapturedScipInventory,
    captured_native: &native::Artifact,
    captured_sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    destination_native: &native::Artifact,
    destination_sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    producer: semantic::Producer,
    basis: &BTreeMap<native::DocumentKey, semantic::SemanticBasis>,
    admitted_encoding: u8,
    r2: &MeasuredFacts<'_>,
    reusable: Option<&BTreeSet<native::DocumentKey>>,
) -> Result<RejoinResult> {
    ensure!(
        inventory.source_manifest_hash == manifest_hash(&captured_native.revision)
            && captured_native.revision.source_set_id == destination_native.revision.source_set_id,
        "captured source manifest/source set mismatch"
    );
    let r1 = MeasuredFacts::new(captured_native, captured_sources)?;
    let raw_index = InventoryIndex::new(inventory)?;
    let coordinates = CoordinateMaps::new(
        inventory,
        captured_native,
        captured_sources,
        admitted_encoding,
    )?;
    let captured_store = MemoryJoinStore::new(&raw_index, &coordinates, &r1);
    let destination_store = MemoryJoinStore::new(&raw_index, &coordinates, r2);
    rejoin_with_stores(
        inventory,
        captured_native,
        captured_sources,
        destination_native,
        destination_sources,
        producer,
        basis,
        admitted_encoding,
        &captured_store,
        &destination_store,
        reusable,
    )
}

#[allow(clippy::too_many_arguments)]
fn rejoin_with_stores<S: JoinStore>(
    inventory: &CapturedScipInventory,
    captured_native: &native::Artifact,
    captured_sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    destination_native: &native::Artifact,
    destination_sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    producer: semantic::Producer,
    basis: &BTreeMap<native::DocumentKey, semantic::SemanticBasis>,
    admitted_encoding: u8,
    captured_store: &S,
    destination_store: &S,
    reusable: Option<&BTreeSet<native::DocumentKey>>,
) -> Result<RejoinResult> {
    let captured = join_inventory_indexed(
        inventory,
        captured_native,
        captured_sources,
        admitted_encoding,
        captured_store,
    )?;
    let captured_declaration_proofs: BTreeSet<_> = captured
        .declaration_bindings
        .iter()
        .filter(|binding| binding.syntax_id.is_some())
        .map(|binding| binding.provenance_id.as_str())
        .collect();
    let captured_calls: BTreeMap<_, _> = captured
        .call_bindings
        .iter()
        .filter(|binding| binding.call_id.is_some())
        .map(|binding| (binding.provenance_id.as_str(), binding))
        .collect();
    let captured_association_proofs: BTreeSet<_> = captured_declaration_proofs
        .iter()
        .copied()
        .chain(captured.reference_claims.keys().map(String::as_str))
        .chain(captured_calls.keys().copied())
        .chain(
            captured
                .type_relationships
                .iter()
                .map(|relationship| relationship.provenance_id.as_str()),
        )
        .collect();
    let mut eligible = BTreeSet::new();
    let mut unsupported_claims = BTreeSet::new();
    let mut unsupported_documents = BTreeSet::new();
    let mut failed = BTreeSet::new();
    let mut requested = BTreeSet::new();
    let unchanged_documents: BTreeSet<_> = captured_sources
        .iter()
        .filter(|(document, bytes)| destination_sources.get(*document) == Some(*bytes))
        .map(|(document, _)| document.clone())
        .collect();
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
                || (reason != Some(&"notRequested") && !unchanged_documents.contains(&document.key))
            {
                failed.insert(document.key.clone());
            }
        }
    }
    for claim in &inventory.claims {
        captured_store.charge(1)?;
        captured_store.lookup_proof(
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
        captured_store
            .document(&doc)
            .context("captured claim outside source manifest")?;
        // MeasuredFacts::new authenticated each document once, not once per claim.
        captured_sources
            .get(&doc)
            .context("missing captured claim source")?;
        if !unchanged_documents.contains(&doc) {
            failed.insert(doc);
            continue;
        }
        if claim.envelope.record_kind == "occurrence" {
            let range = captured_store.occurrence_range(claim, admitted_encoding)?;
            #[cfg(feature = "test-support")]
            count_publication_work(|work| work.original_role_parses += 1);
            let roles = claim.envelope.raw["symbolRoles"]
                .as_u64()
                .context("invalid raw roles")?;
            // Rejoin only the families that actually authored a captured source fact.
            // A tempting callee at the same bytes cannot replace a missing reference,
            // nor can an enclosing range alone turn a plain reference into a call.
            let captured_declaration =
                captured_declaration_proofs.contains(claim.captured_proof_id.as_str());
            let captured_reference = captured
                .reference_claims
                .contains_key(&claim.captured_proof_id);
            let required = if captured_declaration || (!captured_reference && roles & 1 != 0) {
                semantic::JoinKind::DeclarationName
            } else {
                semantic::JoinKind::Reference
            };
            let reused_source = reusable.is_some_and(|docs| docs.contains(&doc));
            let measured = if reused_source {
                // The authenticated selected version witnesses the same measured
                // source association; only targets still consult the current r2.
                captured_store
            } else {
                destination_store
            };
            let first = captured_store.join(&doc, range.clone(), required)?;
            let second = if reused_source {
                first.clone()
            } else {
                measured.join(&doc, range, required)?
            };
            // Only claims without a captured semantic association may keep their
            // unavailable outcome without failing the caller tuple.
            let authored_association =
                captured_association_proofs.contains(claim.captured_proof_id.as_str());
            if !authored_association
                && first.status == semantic::JoinStatus::Unsupported
                && second.status == semantic::JoinStatus::Unsupported
            {
                unsupported_claims.insert(claim.captured_proof_id.clone());
                unsupported_documents.insert(doc.clone());
                eligible.insert(doc);
                continue;
            }
            if !authored_association
                && first.status == semantic::JoinStatus::Unmatched
                && second.status == semantic::JoinStatus::Unmatched
            {
                eligible.insert(doc);
                continue;
            }
            let mut exact = first.status == semantic::JoinStatus::Exact
                && second.status == semantic::JoinStatus::Exact;
            if captured_reference && required != semantic::JoinKind::Reference {
                let first_reference = captured_store.join(
                    &doc,
                    first.anchor.range.clone(),
                    semantic::JoinKind::Reference,
                )?;
                let second_reference = if reused_source {
                    first_reference.clone()
                } else {
                    measured.join(
                        &doc,
                        first.anchor.range.clone(),
                        semantic::JoinKind::Reference,
                    )?
                };
                exact &= first_reference.status == semantic::JoinStatus::Exact
                    && second_reference.status == semantic::JoinStatus::Exact;
            }
            let captured_call = captured_calls
                .get(claim.captured_proof_id.as_str())
                .copied();
            if let Some(binding) = captured_call {
                let call = measured.join(
                    &doc,
                    binding.join.anchor.range.clone(),
                    binding.join.anchor.kind,
                )?;
                let first_call = if reused_source {
                    call.clone()
                } else {
                    captured_store.join(
                        &doc,
                        binding.join.anchor.range.clone(),
                        binding.join.anchor.kind,
                    )?
                };
                exact &= call.status == semantic::JoinStatus::Exact
                    && first_call.status == semantic::JoinStatus::Exact
                    && first_call.candidate_ids == binding.join.candidate_ids;
                // The selected reference and invocation must belong to that same
                // measured call, not merely be independent exact spans.
                exact &= second.candidate_ids.first().is_some_and(|id| {
                    measured.references_by_id(id).iter().any(|reference| {
                        &reference.id == id
                            && call
                                .candidate_ids
                                .first()
                                .into_iter()
                                .flat_map(|id| measured.calls_by_id(id).iter())
                                .any(|candidate| {
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
    let filtered_store = destination_store.restrict(&filtered)?;
    let destination = if let Some(reusable) = reusable {
        let metadata = inventory.proof_identities();
        let mut facts = reuse_captured_source_associations(
            &captured,
            reusable,
            &metadata,
            &destination_native.revision.source_set_id,
            &destination_native.revision.id,
        );
        let mut delta = filtered.clone();
        delta.claims.retain(|claim| {
            !reusable.iter().any(|doc| {
                doc.language == claim.language && doc.path == claim.envelope.relative_path
            })
        });
        if !delta.claims.is_empty() {
            let delta_store = destination_store.restrict(&delta)?;
            let joined = join_inventory_indexed(
                &delta,
                destination_native,
                destination_sources,
                admitted_encoding,
                &delta_store,
            )?;
            facts.symbols.extend(joined.symbols);
            facts
                .declaration_bindings
                .extend(joined.declaration_bindings);
            facts.references.extend(joined.references);
            facts.reference_claims.extend(joined.reference_claims);
            facts.call_bindings.extend(joined.call_bindings);
            facts.type_relationships.extend(joined.type_relationships);
            facts.joins.extend(joined.joins);
            facts.diagnostics.extend(joined.diagnostics);
        }
        let order: BTreeMap<_, _> = inventory
            .claims
            .iter()
            .enumerate()
            .map(|(index, claim)| (claim.captured_proof_id.as_str(), index))
            .collect();
        facts
            .symbols
            .sort_by_key(|row| order.get(row.provenance_id.as_str()).copied());
        facts
            .declaration_bindings
            .sort_by_key(|row| order.get(row.provenance_id.as_str()).copied());
        facts
            .call_bindings
            .sort_by_key(|row| order.get(row.provenance_id.as_str()).copied());
        facts
            .type_relationships
            .sort_by_key(|row| order.get(row.provenance_id.as_str()).copied());
        facts
    } else {
        join_inventory_indexed(
            &filtered,
            destination_native,
            destination_sources,
            admitted_encoding,
            &filtered_store,
        )?
    };
    let metadata = inventory.proof_identities();
    let filtered_metadata = filtered.proof_identities();
    let proofs = ProofIdentityIndex::new(&metadata)?;
    let filtered_proofs = ProofIdentityIndex::new(&filtered_metadata)?;
    finish_rejoin(
        &metadata,
        captured_native,
        destination_native,
        destination_sources,
        producer,
        basis,
        Some(captured_store),
        destination_store,
        None,
        &proofs,
        &filtered_proofs,
        &filtered_metadata,
        &exclusions,
        captured,
        destination,
        requested,
        failed,
        unsupported_claims,
        unsupported_documents,
    )
}

struct ProofIdentityIndex<'a> {
    by_proof: BTreeMap<String, &'a crate::scip_raw::RawProofIdentity>,
    inventory: &'a crate::scip_raw::ProofInventory,
}
impl<'a> ProofIdentityIndex<'a> {
    fn new(inventory: &'a crate::scip_raw::ProofInventory) -> Result<Self> {
        let mut by_proof = BTreeMap::new();
        for claim in &inventory.claims {
            ensure!(
                by_proof.insert(claim.proof_id.clone(), claim).is_none(),
                "non-unique original proof ID"
            );
        }
        Ok(Self {
            by_proof,
            inventory,
        })
    }
    fn lookup(
        &self,
        id: &str,
        kind: &str,
        digest: &str,
    ) -> Result<&crate::scip_raw::RawProofIdentity> {
        Ok(self.inventory.lookup(id, kind, digest)?)
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_rejoin<F: NativeFacts>(
    inventory: &crate::scip_raw::ProofInventory,
    captured_native: &native::Artifact,
    destination_native: &native::Artifact,
    destination_sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    producer: semantic::Producer,
    basis: &BTreeMap<native::DocumentKey, semantic::SemanticBasis>,
    r1: Option<&F>,
    r2: &F,
    target_projection: Option<
        &BTreeMap<(native::DocumentKey, String), semantic::TargetDeclaration>,
    >,
    raw_index: &ProofIdentityIndex<'_>,
    filtered_index: &ProofIdentityIndex<'_>,
    filtered: &crate::scip_raw::ProofInventory,
    exclusions: &BTreeMap<(&str, &str), &str>,
    captured: JoinedFacts,
    destination: JoinedFacts,
    requested: BTreeSet<native::DocumentKey>,
    failed: BTreeSet<native::DocumentKey>,
    unsupported_claims: BTreeSet<String>,
    unsupported_documents: BTreeSet<native::DocumentKey>,
) -> Result<RejoinResult> {
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
    let captured_internal_symbols: BTreeSet<_> = captured_targets
        .iter()
        .filter(|(_, targets)| !targets.is_empty())
        .map(|((_, symbol), _)| symbol.clone())
        .collect();
    let prior_declarations: BTreeSet<_> = captured
        .declaration_bindings
        .iter()
        .filter_map(|row| {
            row.syntax_id
                .as_ref()
                .map(|id| (row.join.anchor.document.clone(), id.clone()))
        })
        .collect();
    let retarget = |target: &mut semantic::Target| -> bool {
        match target {
            semantic::Target::External { symbol }
                if captured_internal_symbols.contains(&symbol.symbol) =>
            {
                false
            }
            semantic::Target::Internal {
                syntax_id,
                document,
                revision_id,
            } => {
                let original = if let Some(r1) = r1 {
                    r1.declarations_by_id(syntax_id).iter().any(|d| {
                        d.syntax_id == *syntax_id
                            && d.document == *document
                            && d.name_range.is_some()
                    })
                } else {
                    prior_declarations.contains(&(document.clone(), syntax_id.clone()))
                };
                if !original {
                    return false;
                }
                if let Some(projection) = target_projection {
                    if let Some(decl) = projection.get(&(document.clone(), syntax_id.clone())) {
                        if decl.name_range.end <= decl.name_range.start || decl.name.is_empty() {
                            return false;
                        }
                        if let Some(source) = r2.source(document)
                            && source.get(decl.name_range.start..decl.name_range.end)
                                != Some(decl.name.as_str())
                        {
                            return false;
                        }
                    } else {
                        return false;
                    }
                } else {
                    let candidates: Vec<_> = r2
                        .declarations_by_id(syntax_id)
                        .iter()
                        .copied()
                        .filter(|d| {
                            d.syntax_id == *syntax_id
                                && d.document == *document
                                && d.name_range.is_some()
                        })
                        .collect();
                    if candidates.len() != 1 || !destination_sources.contains_key(document) {
                        return false;
                    }
                    let declaration = candidates[0];
                    let Some(name) = declaration.name_range.as_ref() else {
                        return false;
                    };
                    if r2
                        .source(document)
                        .and_then(|s| s.get(name.start..name.end))
                        != declaration.name.as_deref()
                    {
                        return false;
                    }
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
        let raw = filtered_index.by_proof.get(&symbol.provenance_id).copied();
        if let Some(claim) = raw {
            let doc = native::DocumentKey {
                source_set_id: destination_native.revision.source_set_id.clone(),
                language: claim.language.clone(),
                path: claim.path.clone(),
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
        } else if let Some(claim) = filtered_index.by_proof.get(&symbol.provenance_id).copied() {
            omitted.insert(Some(native::DocumentKey {
                source_set_id: destination_native.revision.source_set_id.clone(),
                language: claim.language.clone(),
                path: claim.path.clone(),
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
        .filter_map(|binding| {
            captured
                .reference_claims
                .get(&binding.provenance_id)
                .map(|reference| {
                    (
                        binding.provenance_id.clone(),
                        (
                            reference.resolution,
                            reference.declared_target.clone(),
                            reference.candidates.clone(),
                        ),
                    )
                })
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
    let mut reference_rows = BTreeMap::<String, semantic::Reference>::new();
    let mut reference_targets = BTreeMap::<String, Vec<semantic::Target>>::new();
    let mut reference_proofs = BTreeMap::<String, Vec<String>>::new();
    let mut accepted_reference_ids = BTreeMap::new();
    for (raw_id, mut reference) in facts.reference_claims {
        let Some(prior) = captured.reference_claims.get(&raw_id) else {
            continue;
        };
        ensure!(
            prior.document == reference.document && prior.range == reference.range,
            "captured reference anchor differs"
        );
        reference.roles.retain(|role| prior.roles.contains(role));
        if reference.roles.is_empty() {
            continue;
        }
        reference.resolution = prior.resolution;
        reference.declared_target = prior.declared_target.clone();
        reference.candidates = prior.candidates.clone();
        if !reference
            .declared_target
            .iter_mut()
            .chain(reference.candidates.iter_mut())
            .all(&retarget)
        {
            omitted.insert(Some(reference.document.clone()));
            continue;
        }
        reference_targets
            .entry(reference.id.clone())
            .or_default()
            .extend(
                reference
                    .declared_target
                    .iter()
                    .chain(&reference.candidates)
                    .cloned(),
            );
        reference_proofs
            .entry(reference.id.clone())
            .or_default()
            .push(raw_id.clone());
        accepted_reference_ids.insert(raw_id.clone(), reference.id.clone());
        provenance_sources.insert((raw_id, semantic::EvidenceKind::SemanticReference));
        if let Some(row) = reference_rows.get_mut(&reference.id) {
            let roles: BTreeSet<_> = row.roles.iter().chain(&reference.roles).copied().collect();
            row.roles = roles.into_iter().collect();
            if row.roles.contains(&semantic::Role::Definition) {
                row.site = semantic::ReferenceSite::Declaration;
            }
        } else {
            reference_rows.insert(reference.id.clone(), reference);
        }
    }
    let mut references = Vec::new();
    let mut additional_reference_proofs = BTreeMap::new();
    for (id, mut reference) in reference_rows {
        let mut targets = reference_targets.remove(&id).unwrap_or_default();
        targets.sort_by_key(|target| {
            native_ids::canonical(&serde_json::to_value(target).expect("serializable target"))
        });
        targets.dedup();
        reference.declared_target = None;
        reference.candidates.clear();
        if targets.len() > 1 {
            reference.resolution = semantic::Resolution::Ambiguous;
            reference.candidates = targets;
        } else if let Some(target) = targets.pop() {
            reference.resolution = if matches!(target, semantic::Target::Internal { .. }) {
                semantic::Resolution::Resolved
            } else {
                semantic::Resolution::External
            };
            reference.declared_target = Some(target);
        } else {
            reference.resolution = semantic::Resolution::Unresolved;
        }
        for raw_id in reference_proofs.remove(&id).unwrap_or_default() {
            additional_reference_proofs.insert(
                raw_id,
                if reference.resolution == semantic::Resolution::Ambiguous {
                    3
                } else {
                    1
                },
            );
        }
        references.push(reference);
    }
    let references_by_id: BTreeMap<_, _> =
        references.iter().map(|row| (row.id.clone(), row)).collect();
    calls.retain_mut(|call| {
        let Some(reference) = accepted_reference_ids
            .get(&call.provenance_id)
            .and_then(|id| references_by_id.get(id))
        else {
            return false;
        };
        call.resolution = reference.resolution;
        call.declared_target = reference.declared_target.clone();
        call.candidates = reference.candidates.clone();
        call.stale_target = call.declared_target.as_ref().and_then(|target| {
            matches!(target, semantic::Target::Internal { .. }).then_some(false)
        });
        true
    });
    let eligible_proofs: BTreeSet<_> = filtered
        .claims
        .iter()
        .map(|claim| claim.proof_id.as_str())
        .collect();
    let mut relationships = Vec::new();
    // Changed B claims need not survive the destination producer join. Its measured
    // same-ID declaration must still independently pass the r2 retarget check.
    for mut relationship in captured
        .type_relationships
        .iter()
        .filter(|row| eligible_proofs.contains(row.provenance_id.as_str()))
        .cloned()
    {
        if retarget(&mut relationship.source) && retarget(&mut relationship.target) {
            provenance_sources.insert((
                relationship.provenance_id.clone(),
                semantic::EvidenceKind::TypeRelationship,
            ));
            relationships.push(relationship);
        } else if let Some(claim) = filtered_index
            .by_proof
            .get(&relationship.provenance_id)
            .copied()
        {
            omitted.insert(Some(native::DocumentKey {
                source_set_id: destination_native.revision.source_set_id.clone(),
                language: claim.language.clone(),
                path: claim.path.clone(),
            }));
        }
    }
    let mut provenance = Vec::new();
    let destination_manifest_hash = manifest_hash(&destination_native.revision);
    let mut ids = BTreeMap::new();
    for (raw_id, kind) in provenance_sources {
        let claim = raw_index
            .by_proof
            .get(&raw_id)
            .copied()
            .context("missing raw fact for joined provenance")?;
        raw_index.lookup(&raw_id, &claim.record_kind, &claim.fact_digest)?;
        let doc = native::DocumentKey {
            source_set_id: destination_native.revision.source_set_id.clone(),
            language: claim.language.clone(),
            path: claim.path.clone(),
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
        let dst = r2.document(&doc).context("missing destination document")?;
        let freshness = if captured_native
            .revision
            .documents
            .iter()
            .find(|old| old.key == doc)
            .is_none_or(|old| old.content_hash != dst.content_hash)
        {
            semantic::Freshness::Stale
        } else if captured_basis.revision_id != destination_native.revision.id
            || captured_basis.source_manifest_hash != destination_manifest_hash
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
                record_kind: serde_json::from_value(serde_json::json!(claim.record_kind))?,
                fact_digest: claim.fact_digest.clone(),
            }),
        });
    }
    let mut outcomes = BTreeMap::<String, usize>::new();
    for claim in inventory
        .claims
        .iter()
        .filter(|c| c.record_kind != "symbolInformation")
    {
        outcomes.insert(claim.proof_id.clone(), 2);
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
    for (raw_id, outcome) in &additional_reference_proofs {
        outcomes.insert(raw_id.clone(), *outcome);
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
            || captured.diagnostics.contains_key(&doc.key);
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
                && c.path == row.document_path
                && c.record_kind != "symbolInformation"
        }) {
            counts[0] += 1;
            let outcome = if row.state == semantic::CoverageState::Failed {
                2
            } else {
                *outcomes
                    .get(&claim.proof_id)
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn rejoin_inventory_with_original_witness(
    inventory: &crate::scip_raw::ProofInventory,
    captured_native: &native::Artifact,
    destination_native: &native::Artifact,
    changed_sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    producer: semantic::Producer,
    basis: &BTreeMap<native::DocumentKey, semantic::SemanticBasis>,
    original: &semantic::SemanticSnapshot,
    reusable: &BTreeSet<native::DocumentKey>,
    current_targets: &BTreeMap<(native::DocumentKey, String), semantic::TargetDeclaration>,
    shared_changed_index: &MeasuredFacts<'_>,
) -> Result<Option<RejoinResult>> {
    if inventory.source_manifest_hash != manifest_hash(&captured_native.revision)
        || captured_native.revision.source_set_id != destination_native.revision.source_set_id
    {
        return Ok(None);
    }
    let Some(captured) = original_associations_from_selected(original, inventory, &producer.id)
    else {
        return Ok(None);
    };
    let raw_index = ProofIdentityIndex::new(inventory)?;
    let mut filtered = inventory.clone();
    filtered.claims.retain(|claim| {
        reusable
            .iter()
            .any(|doc| doc.language == claim.language && doc.path == claim.path)
    });
    let filtered_index = ProofIdentityIndex::new(&filtered)?;
    let destination = reuse_captured_source_associations(
        &captured,
        reusable,
        inventory,
        &destination_native.revision.source_set_id,
        &destination_native.revision.id,
    );
    let mut r2 = shared_changed_index.clone();
    for doc in &destination_native.revision.documents {
        r2.documents.insert(doc.key.clone(), doc);
    }
    let exclusions: BTreeMap<_, _> = inventory
        .exclusions
        .iter()
        .map(|row| {
            (
                (row.language.as_str(), row.relative_path.as_str()),
                row.reason.as_str(),
            )
        })
        .collect();
    let mut requested = BTreeSet::new();
    let mut failed = BTreeSet::new();
    for doc in &captured_native.revision.documents {
        let language: semantic::Language =
            serde_json::from_value(serde_json::json!(doc.key.language))?;
        if producer.languages.contains(&language) {
            let reason = exclusions.get(&(doc.key.language.as_str(), doc.key.path.as_str()));
            if reason != Some(&"notRequested") {
                requested.insert(doc.key.clone());
            }
            if reason == Some(&"failed")
                || (reason != Some(&"notRequested") && !reusable.contains(&doc.key))
            {
                failed.insert(doc.key.clone());
            }
        }
    }
    for claim in &inventory.claims {
        requested.insert(native::DocumentKey {
            source_set_id: captured_native.revision.source_set_id.clone(),
            language: claim.language.clone(),
            path: claim.path.clone(),
        });
    }
    finish_rejoin(
        inventory,
        captured_native,
        destination_native,
        changed_sources,
        producer,
        basis,
        None,
        &r2,
        Some(current_targets),
        &raw_index,
        &filtered_index,
        &filtered,
        &exclusions,
        captured,
        destination,
        requested,
        failed,
        BTreeSet::new(),
        BTreeSet::new(),
    )
    .map(Some)
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

#[cfg(test)]
#[path = "semantic_import_complexity_tests.rs"]
mod complexity_tests;
