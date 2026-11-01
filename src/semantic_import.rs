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
mod staged;
use join_store::{
    BasisFor, DocumentJoinState, GlobalDiscoverySink, GlobalJoinReads, IndexedJoinStore, JoinIndex,
    JoinSink, JoinStore, MethodSummary, NativeFacts, OccurrenceRow, ProofIdentities, ProofRef,
    PublicationSink, RelationshipRow,
};
use memory::{MemoryJoinSink, MemoryJoinStore};
#[cfg(feature = "test-support")]
pub(crate) use staged::cursor_sql as staged_cursor_sql;
pub use staged::join_staged;
pub(crate) use staged::publication::read_documents as read_staged_documents;
#[cfg(feature = "test-support")]
pub(crate) use staged::publication::statements as staged_publication_statements;
#[cfg(feature = "test-support")]
pub(crate) use staged::target_callable_sql as staged_target_callable_sql;

/// Per-document anchor candidates and capability families.
pub(crate) type AnchorParts = (
    BTreeMap<(usize, usize, semantic::JoinKind), BTreeSet<String>>,
    BTreeMap<semantic::JoinKind, native::MeasurementSupport>,
);

type AnchorKey = (native::DocumentKey, usize, usize, semantic::JoinKind);
/// An `AnchorKey` whose document is its rank among the artifact's interned documents.
type RankedAnchor = (usize, usize, usize, semantic::JoinKind);
/// A symbol's join identity: its raw spelling and its scope document, which is the containing
/// document of a `local ` symbol, the crate root of a Cargo-target-scoped one (#394), and
/// `None` for a global one.
type ScopedKey = (String, Option<native::DocumentKey>);

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
    by_document: BTreeMap<native::DocumentKey, Vec<&'a native::Declaration>>,
    calls: BTreeMap<String, Vec<&'a native::Call>>,
    references: BTreeMap<String, Vec<&'a native::Reference>>,
    support: BTreeMap<(native::DocumentKey, semantic::JoinKind), &'a native::MeasurementSupport>,
}

/// The measured anchor index of a native artifact: what `MeasuredFacts` and a staged document
/// group share. The per-declaration name, key and document indexes are `MeasuredFacts`' own.
struct MeasuredAnchors<'a> {
    sources: BTreeMap<native::DocumentKey, &'a str>,
    documents: BTreeMap<native::DocumentKey, &'a native::Document>,
    /// The interned documents, in key order: `candidates` names a document by its rank here.
    ranks: Vec<native::DocumentKey>,
    candidates: BTreeMap<RankedAnchor, BTreeSet<String>>,
    declarations: BTreeMap<String, Vec<&'a native::Declaration>>,
    calls: BTreeMap<String, Vec<&'a native::Call>>,
    references: BTreeMap<String, Vec<&'a native::Reference>>,
    support: BTreeMap<(native::DocumentKey, semantic::JoinKind), &'a native::MeasurementSupport>,
}

impl<'a> MeasuredFacts<'a> {
    /// These facts over `destination`'s whole manifest: reused documents have no facts here,
    /// only their native document.
    pub(crate) fn over_manifest(&self, destination: &'a native::Artifact) -> Self {
        let mut facts = self.clone();
        for doc in &destination.revision.documents {
            facts.documents.insert(doc.key.clone(), doc);
        }
        facts
    }

    pub fn new(
        native: &'a native::Artifact,
        sources: &'a BTreeMap<native::DocumentKey, Vec<u8>>,
    ) -> Result<Self> {
        let MeasuredAnchors {
            sources,
            documents,
            ranks,
            candidates,
            declarations,
            calls,
            references,
            support,
        } = Self::anchors(native, sources)?;
        let candidates = candidates
            .into_iter()
            .map(|((rank, start, end, kind), ids)| ((ranks[rank].clone(), start, end, kind), ids))
            .collect();
        let mut declaration_names: BTreeMap<_, Vec<&native::Declaration>> = BTreeMap::new();
        let mut declaration_keys: BTreeMap<_, Vec<&native::Declaration>> = BTreeMap::new();
        let mut by_document: BTreeMap<_, Vec<&native::Declaration>> = BTreeMap::new();
        for d in &native.declarations {
            by_document.entry(d.document.clone()).or_default().push(d);
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
        }
        Ok(Self {
            native,
            sources,
            documents,
            candidates,
            declarations,
            declaration_names,
            declaration_keys,
            by_document,
            calls,
            references,
            support,
        })
    }

    /// The anchor candidates and capability families of `document` alone, without building the
    /// per-record indexes a document group keeps itself.
    pub(crate) fn anchor_parts(
        native: &'a native::Artifact,
        sources: &'a BTreeMap<native::DocumentKey, Vec<u8>>,
        document: &native::DocumentKey,
    ) -> Result<AnchorParts> {
        let anchors = Self::anchors(native, sources)?;
        let rank = anchors.ranks.binary_search(document).ok();
        Ok(Self::parts(
            anchors.candidates,
            anchors.support,
            rank,
            document,
        ))
    }

    fn anchors(
        native: &'a native::Artifact,
        sources: &'a BTreeMap<native::DocumentKey, Vec<u8>>,
    ) -> Result<MeasuredAnchors<'a>> {
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
        // Every document a record or the revision names, interned to its rank in key order:
        // ordering anchors by rank is ordering them by document key, without a key copy or
        // comparison per record.
        let mut keys: Vec<&native::DocumentKey> =
            native.revision.documents.iter().map(|d| &d.key).collect();
        let named = native
            .declarations
            .iter()
            .map(|d| &d.document)
            .chain(native.calls.iter().map(|c| &c.document))
            .chain(native.references.iter().map(|r| &r.document));
        for document in named {
            if keys.last() != Some(&document) {
                keys.push(document);
            }
        }
        keys.sort_unstable();
        keys.dedup();
        let mut last = 0;
        let mut rank = |document: &native::DocumentKey| -> usize {
            if keys.get(last) != Some(&document) {
                last = keys
                    .binary_search(&document)
                    .expect("every record document is interned");
            }
            last
        };
        let mut declarations: BTreeMap<String, Vec<&native::Declaration>> = BTreeMap::new();
        let mut calls: BTreeMap<String, Vec<&native::Call>> = BTreeMap::new();
        let mut references: BTreeMap<String, Vec<&native::Reference>> = BTreeMap::new();
        let mut anchors: BTreeMap<RankedAnchor, BTreeSet<String>> = BTreeMap::new();
        let mut insert = |doc: usize, range: &native::Range, kind, id: &str| {
            anchors
                .entry((doc, range.start, range.end, kind))
                .or_default()
                .insert(id.to_owned());
        };
        for d in &native.declarations {
            declarations.entry(d.syntax_id.clone()).or_default().push(d);
            if let Some(range) = &d.name_range {
                insert(
                    rank(&d.document),
                    range,
                    semantic::JoinKind::DeclarationName,
                    &d.syntax_id,
                );
            }
        }
        for c in &native.calls {
            calls.entry(c.id.clone()).or_default().push(c);
            let doc = rank(&c.document);
            if let Some(range) = &c.callee_range {
                insert(doc, range, semantic::JoinKind::Callee, &c.id);
            }
            insert(doc, &c.range, semantic::JoinKind::Invocation, &c.id);
        }
        for r in &native.references {
            references.entry(r.id.clone()).or_default().push(r);
            insert(
                rank(&r.document),
                &r.range,
                semantic::JoinKind::Reference,
                &r.id,
            );
        }
        let texts: Vec<Option<&str>> = keys
            .iter()
            .map(|key| source_text.get(*key).copied())
            .collect();
        let mut raw_ranges: BTreeMap<_, Vec<(usize, usize)>> = BTreeMap::new();
        // Each anchor keeps, in place, the candidates compatible with its spelling.
        for ((rank, start, end, kind), ids) in anchors.iter_mut() {
            let document = keys[*rank];
            raw_ranges
                .entry((*rank, *kind))
                .or_default()
                .push((*start, *end));
            #[cfg(test)]
            complexity_work(|work| work.native_gap_index_records += 1);
            let spelling = texts[*rank].and_then(|source| source.get(*start..*end));
            let lookup = if *kind == semantic::JoinKind::Reference {
                spelling.and_then(|s| native::lookup(&document.language, s).ok())
            } else {
                None
            };
            ids.retain(|id| {
                #[cfg(test)]
                complexity_work(|work| work.native_candidates += 1);
                match kind {
                    semantic::JoinKind::DeclarationName => {
                        declarations.get(id).is_some_and(|rows| {
                            rows.iter()
                                .any(|d| &d.document == document && d.name.as_deref() == spelling)
                        })
                    }
                    semantic::JoinKind::Callee => calls.get(id).is_some_and(|rows| {
                        rows.iter()
                            .any(|c| &c.document == document && c.spelling.as_deref() == spelling)
                    }),
                    semantic::JoinKind::Invocation => calls.get(id).is_some_and(|rows| {
                        rows.iter().any(|c| {
                            &c.document == document
                                && c.range.start == *start
                                && c.range.end == *end
                        })
                    }),
                    semantic::JoinKind::Reference => references.get(id).is_some_and(|rows| {
                        rows.iter().any(|r| {
                            &r.document == document
                                && spelling == Some(r.spelling.as_str())
                                && lookup.as_deref() == Some(r.lookup_key.as_str())
                        })
                    }),
                }
            });
        }
        let candidates = anchors;
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
                let contradicts = keys
                    .binary_search(&document)
                    .ok()
                    .and_then(|rank| raw_ranges.get(&(rank, *kind)))
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
        let documents_by_rank = keys.into_iter().cloned().collect();
        Ok(MeasuredAnchors {
            sources: source_text,
            documents,
            ranks: documents_by_rank,
            candidates,
            declarations,
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
        let ids = self
            .candidates
            .get(&(document.clone(), range.start, range.end, kind))
            .into_iter()
            .flat_map(|ids| ids.iter())
            .cloned()
            .collect::<Vec<_>>();
        compose_join(
            &self.native.revision.id,
            doc,
            source,
            ids,
            self.support.get(&(document.clone(), kind)).copied(),
            range,
            kind,
        )
    }

    /// The anchor candidates and capability families of one document. A document group keeps
    /// these and serves joins from them.
    fn parts(
        candidates: BTreeMap<RankedAnchor, BTreeSet<String>>,
        support: BTreeMap<
            (native::DocumentKey, semantic::JoinKind),
            &'a native::MeasurementSupport,
        >,
        rank: Option<usize>,
        document: &native::DocumentKey,
    ) -> AnchorParts {
        let candidates = candidates
            .into_iter()
            .filter(|((owner, ..), _)| Some(*owner) == rank)
            .map(|((_, start, end, kind), ids)| ((start, end, kind), ids))
            .collect();
        let support = support
            .into_iter()
            .filter(|((owner, _), _)| owner == document)
            .map(|((_, kind), family)| (kind, family.clone()))
            .collect();
        (candidates, support)
    }
}

/// JSON bytes of the partial documents one language of an import report lists with their
/// reasons (#520). Further partial documents are only counted, so the four languages stay well
/// inside the report's 1 MiB bound.
pub(crate) const REPORT_PARTIAL_DOCUMENT_BYTES: usize = 64 * 1024;

/// One language's coverage reasons in an import report (#520): partial documents with each
/// reason, unsupported occurrences by native gap kind, and the partial documents in coverage
/// order up to [`REPORT_PARTIAL_DOCUMENT_BYTES`]. The in-memory and staged joins feed it the
/// same rows in the same order.
#[derive(Default)]
pub(crate) struct LanguageReasons {
    documents: u64,
    native_gaps: u64,
    native_gaps_only: u64,
    capped: u64,
    omitted: u64,
    captured_diagnostics: u64,
    occurrences: [u64; 4],
    listed: Vec<serde_json::Value>,
    listed_bytes: usize,
    /// Set by the first document that did not fit, so later ones are never listed after it.
    full: bool,
}

impl LanguageReasons {
    pub(crate) fn add(&mut self, row: &semantic::Coverage) {
        let Some(reasons) = &row.reasons else {
            return;
        };
        let gaps = reasons.native_gaps() > 0;
        self.documents += 1;
        self.native_gaps += u64::from(gaps);
        self.native_gaps_only +=
            u64::from(gaps && !reasons.capped && !reasons.omitted && !reasons.captured_diagnostics);
        self.capped += u64::from(reasons.capped);
        self.omitted += u64::from(reasons.omitted);
        self.captured_diagnostics += u64::from(reasons.captured_diagnostics);
        for (total, count) in self.occurrences.iter_mut().zip(reasons.gaps()) {
            *total = total.saturating_add(count);
        }
        if self.full {
            return;
        }
        let entry = serde_json::json!({
            "path": row.document_path, "diagnostic": row.diagnostic, "reasons": reasons,
        });
        // The serialized array is `[` + entries joined by `,` + `]`: each entry plus its
        // separator or opening bracket, plus the closing bracket, must fit the bound.
        let bytes = entry.to_string().len() + 1;
        if self.listed_bytes + bytes < REPORT_PARTIAL_DOCUMENT_BYTES {
            self.listed_bytes += bytes;
            self.listed.push(entry);
        } else {
            self.full = true;
        }
    }

    /// The language's `partialReasons` report object.
    pub(crate) fn json(self) -> serde_json::Value {
        let occurrences: serde_json::Map<_, _> = semantic::GAP_KINDS
            .iter()
            .zip(self.occurrences)
            .map(|(kind, count)| ((*kind).to_owned(), count.into()))
            .collect();
        let unlisted = self.documents - self.listed.len() as u64;
        serde_json::json!({
            "documents": self.documents, "nativeGaps": self.native_gaps,
            "nativeGapsOnly": self.native_gaps_only, "capped": self.capped,
            "omitted": self.omitted, "capturedDiagnostics": self.captured_diagnostics,
            "unsupportedOccurrences": occurrences,
            "listed": self.listed, "unlisted": unlisted,
        })
    }
}

/// Classifies one anchor from its measured candidates. Shared by the in-memory facts and the
/// staged document groups, so both apply one set of rules.
pub(crate) fn compose_join(
    revision_id: &str,
    doc: &native::Document,
    source: &str,
    ids: Vec<String>,
    support: Option<&native::MeasurementSupport>,
    range: native::Range,
    kind: semantic::JoinKind,
) -> Result<semantic::Join> {
    source
        .get(range.start..range.end)
        .filter(|s| !s.is_empty())
        .context("invalid UTF-8 scalar anchor")?;
    let support = support.context("missing native measurement family")?;
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
            document: doc.key.clone(),
            revision_id: revision_id.to_owned(),
            content_hash: doc.content_hash.clone(),
            range,
            kind,
        },
        status,
        candidate_ids: ids,
        diagnostic,
    })
}

pub(crate) type Position = (i32, i32);
pub(crate) type Endpoints = (Position, Position);
type CoordinateMap = BTreeMap<Position, usize>;

fn occurrence_endpoints(claim: &CapturedClaim, admitted_encoding: u8) -> Result<(u8, Endpoints)> {
    ensure!(
        claim.envelope.record_kind == "occurrence",
        "not an occurrence"
    );
    range_endpoints(
        &claim.envelope.raw["range"],
        claim.envelope.position_encoding,
        admitted_encoding,
    )
}

fn range_endpoints(
    raw: &serde_json::Value,
    position_encoding: u8,
    admitted_encoding: u8,
) -> Result<(u8, Endpoints)> {
    let mut values = [0u64; 4];
    let length = match raw["encoding"]
        .as_str()
        .context("missing raw range encoding")?
    {
        "legacy" => {
            let source = raw["values"].as_array().context("missing legacy values")?;
            ensure!(matches!(source.len(), 3 | 4), "invalid raw range length");
            for (target, value) in values.iter_mut().zip(source) {
                *target = value.as_u64().context("invalid legacy value")?;
            }
            source.len()
        }
        "singleLine" => {
            values[..3].copy_from_slice(&[
                raw["line"].as_u64().context("invalid line")?,
                raw["startCharacter"].as_u64().context("invalid start")?,
                raw["endCharacter"].as_u64().context("invalid end")?,
            ]);
            3
        }
        "multiLine" => {
            values = [
                raw["startLine"].as_u64().context("invalid start line")?,
                raw["startCharacter"].as_u64().context("invalid start")?,
                raw["endLine"].as_u64().context("invalid end line")?,
                raw["endCharacter"].as_u64().context("invalid end")?,
            ];
            4
        }
        _ => anyhow::bail!("invalid raw range shape"),
    };
    let encoding = if position_encoding == 0 {
        admitted_encoding
    } else {
        position_encoding
    };
    ensure!(
        matches!(encoding, 1..=3) && encoding == admitted_encoding,
        "SCIP encoding differs from admission"
    );
    let endpoints = if length == 3 {
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

/// The normalized endpoints of a stored occurrence's canonical envelope. One span has several
/// valid wire shapes, each kept verbatim in the envelope, so spans are compared only as these.
pub(crate) fn stored_occurrence_endpoints(
    envelope: &serde_json::Value,
    admitted_encoding: u8,
) -> Result<Endpoints> {
    let position_encoding = u8::try_from(
        envelope["positionEncoding"]
            .as_u64()
            .context("raw position encoding")?,
    )?;
    let (_, endpoints) = range_endpoints(
        &envelope["raw"]["range"],
        position_encoding,
        admitted_encoding,
    )?;
    Ok(endpoints)
}

/// Recognize the pinned producers' raw file/module markers, not a measured name anchor.
fn module_definition(claim: &CapturedClaim) -> bool {
    use scip::types::descriptor::Suffix;
    if claim.envelope.record_kind != "occurrence"
        || !claim.envelope.raw["symbolRoles"]
            .as_u64()
            .is_some_and(|roles| roles & 1 != 0)
    {
        return false;
    }
    let Some(symbol) = claim.envelope.raw["symbol"].as_str() else {
        return false;
    };
    let Ok(symbol) = scip::symbol::parse_symbol(symbol) else {
        return false;
    };
    let package = |descriptor: &scip::types::Descriptor| {
        descriptor.suffix.enum_value() == Ok(Suffix::Package)
    };
    match (claim.language.as_str(), symbol.scheme.as_str()) {
        // Admission decoded this claim and rejected any empty marker that does not name its
        // document under the authenticated package or project root (owner ruling 2026-10-29,
        // #443), so the marker shape suffices here.
        ("javascript", "scip-typescript") => {
            crate::scip_raw::module_roots::typescript_module_path(&symbol).is_some()
        }
        ("python", "scip-python") => {
            symbol
                .descriptors
                .split_last()
                .is_some_and(|(last, prefix)| {
                    last.suffix.enum_value() == Ok(Suffix::Meta)
                        && last.name == "__init__"
                        && !prefix.is_empty()
                        && prefix.iter().all(package)
                        && claim
                            .envelope
                            .relative_path
                            .strip_suffix("/__init__.py")
                            .or_else(|| claim.envelope.relative_path.strip_suffix(".py"))
                            .is_some_and(|path| {
                                prefix
                                    .iter()
                                    .map(|descriptor| descriptor.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(".")
                                    == path.replace('/', ".")
                            })
                })
        }
        _ => false,
    }
}

fn mapped_range(
    claim: &CapturedClaim,
    endpoints: Endpoints,
    offsets: &CoordinateMap,
) -> Result<native::Range> {
    let start = *offsets
        .get(&endpoints.0)
        .context("missing SCIP start position")?;
    let end = *offsets
        .get(&endpoints.1)
        .context("missing SCIP end position")?;
    if start > end || (start == end && !module_definition(claim)) {
        return Err(crate::scip_raw::ScipCaptureError::InvalidArtifact(
            "empty or reversed SCIP range".into(),
        )
        .into());
    }
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
    mapped_range(claim, endpoints, &offsets)
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
        mapped_range(claim, endpoints, offsets)
    }
}

struct InventoryIndex<'a> {
    claims: &'a [CapturedClaim],
    proofs: BTreeMap<(String, String, String), &'a CapturedClaim>,
    by_proof: BTreeMap<String, &'a CapturedClaim>,
    symbol_info: BTreeMap<(String, String, String), &'a CapturedClaim>,
    declared_symbols: BTreeSet<String>,
    information_symbols: BTreeSet<String>,
}

impl<'a> InventoryIndex<'a> {
    fn new(inventory: &'a CapturedScipInventory) -> Result<Self> {
        let mut index = Self {
            claims: &inventory.claims,
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
        self.resolve(proof, kind, digest)
    }

    fn resolve(&self, proof: &str, kind: &str, digest: &str) -> Result<&'a CapturedClaim> {
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
    scoped_symbol_key(&scoped_key(symbol, document))
}

/// The contract key of a join identity: document scope for a local symbol, target scope for
/// one scoped to a Cargo target's crate root (owner ruling 2026-10-30, #394), else global.
fn scoped_symbol_key(key: &ScopedKey) -> semantic::SymbolKey {
    semantic::SymbolKey {
        scheme: semantic::SymbolScheme::Scip,
        symbol: key.0.clone(),
        scope: match &key.1 {
            None => semantic::SymbolScope::Global,
            Some(_) if key.0.starts_with("local ") => semantic::SymbolScope::Document,
            Some(_) => semantic::SymbolScope::Target,
        },
        document: key.1.clone(),
    }
}

/// The Cargo target of a claim's document, if its target is determined (#394).
fn claim_target(source_set_id: &str, claim: &CapturedClaim) -> Option<join_store::ClaimTarget> {
    claim.envelope.cargo_target.as_ref().map(|target| {
        (
            native::DocumentKey {
                source_set_id: source_set_id.to_owned(),
                language: claim.language.clone(),
                path: target.root.clone(),
            },
            target
                .is_lib()
                .then(|| (target.package.clone(), target.version.clone())),
        )
    })
}

/// The identity a definition or symbol information defines: a local symbol in its document,
/// any other in its document's Cargo target, or globally when that target is not determined.
fn defining_key(
    symbol: &str,
    document: &native::DocumentKey,
    target: Option<&join_store::ClaimTarget>,
) -> ScopedKey {
    match target {
        Some((root, _)) if !symbol.starts_with("local ") => (symbol.to_owned(), Some(root.clone())),
        _ => scoped_key(symbol, document),
    }
}

/// Whether a SCIP key's package is the Cargo package `name` (and `version`, when the manifest
/// states one). A key that names no package, or a package without a name, passes: it carries
/// nothing to check against.
fn key_names_package(symbol: &str, name: &str, version: Option<&str>) -> bool {
    let Ok(parsed) = scip::symbol::parse_symbol(symbol) else {
        return false;
    };
    match parsed.package.as_ref() {
        Some(package) if !package.name.is_empty() => {
            package.name == name && version.is_none_or(|version| package.version == version)
        }
        _ => true,
    }
}

/// The identity a use names (#394). A symbol present in the use's own Cargo target is that
/// target's. Otherwise one present in exactly one library target is that library's: a library
/// is the only kind of target another can depend on, and rust-analyzer's key names its
/// package. A use in a document without a determined target takes a library's key only when
/// that library is the one target defining it, since the document may belong to a target that
/// defines the same key. Every other use names the global key.
fn using_key<G: GlobalJoinReads>(
    index: &G,
    symbol: &str,
    document: &native::DocumentKey,
    target: Option<&join_store::ClaimTarget>,
) -> Result<ScopedKey> {
    if symbol.starts_with("local ") {
        return Ok(scoped_key(symbol, document));
    }
    if let Some((root, _)) = target
        && index.presence(symbol, Some(root))? != (false, false)
    {
        return Ok((symbol.to_owned(), Some(root.clone())));
    }
    let defining = index.defining_targets(symbol)?;
    // The library must also be the package rust-analyzer's key names: its manifest name, and
    // its version when the manifest states one (supervisor ruling on #394 ruling 2).
    let named = |package: &Option<join_store::LibraryPackage>| {
        package
            .as_ref()
            .is_some_and(|(name, version)| key_names_package(symbol, name, version.as_deref()))
    };
    let library = match (target, &defining[..]) {
        (None, [(root, package)]) if named(package) => Some(root),
        (None, _) => None,
        (Some(_), _) => match &defining
            .iter()
            .filter(|(_, package)| named(package))
            .collect::<Vec<_>>()[..]
        {
            [(root, _)] => Some(root),
            _ => None,
        },
    };
    Ok((symbol.to_owned(), library.cloned()))
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
    /// Raw proofs of a reused document fused into another proof's reference row: they keep
    /// their minted proof but own no row.
    pub fused_proofs: BTreeSet<String>,
    /// The proofs of a reference row that a fused call proof also claims.
    pub fused_calls: BTreeSet<String>,
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
    let store = MemoryJoinStore::new(&raw_index, &coordinates, &measured, sources);
    join_into_facts(admitted_encoding, &store)
}

fn join_into_facts<S: IndexedJoinStore>(admitted_encoding: u8, store: &S) -> Result<JoinedFacts> {
    let mut sink = MemoryJoinSink::default();
    join_inventory_indexed(admitted_encoding, store, &mut sink)?;
    Ok(sink.into_facts())
}

fn join_inventory_indexed<S: IndexedJoinStore, K: JoinSink>(
    admitted_encoding: u8,
    store: &S,
    out: &mut K,
) -> Result<()> {
    let mut index = build_join_index(admitted_encoding, store)?;
    emit_join_symbols(store, &index, out)?;
    join_occurrences(admitted_encoding, store, &mut index, out)?;
    settle_join_references(store, &mut index, out)?;
    join_type_relationships(store, &mut index, out)
}

fn declaration_targets<S: JoinStore, G: GlobalJoinReads>(
    store: &S,
    index: &G,
    document: &native::DocumentKey,
    symbol: &str,
    operation: &mut S::Operation,
) -> Result<Vec<semantic::Target>> {
    scoped_declaration_targets(store, index, &scoped_key(symbol, document), operation)
}

fn scoped_declaration_targets<S: JoinStore, G: GlobalJoinReads>(
    store: &S,
    index: &G,
    key: &ScopedKey,
    operation: &mut S::Operation,
) -> Result<Vec<semantic::Target>> {
    store.reserve_operation(operation, || {
        staged::sizes::sum(&[
            std::mem::size_of::<ScopedKey>() as u64,
            key.0.len() as u64,
            key.1.as_ref().map_or(Ok(0), staged::sizes::document)?,
        ])
    })?;
    let rows = index.declarations(key, &mut |bytes| {
        store.reserve_operation(operation, || Ok(bytes))
    })?;
    store.reserve_operation(operation, || {
        let revisions = (rows.len() as u64)
            .checked_mul(store.revision_id().len() as u64)
            .ok_or_else(|| {
                crate::scip_raw::ScipCaptureError::LimitExceeded("unit expansion".into())
            })?;
        staged::sizes::sum(&[
            std::mem::size_of::<Vec<semantic::Target>>() as u64,
            staged::target_size::target_slots(rows.len())?,
            revisions,
        ])
    })?;
    let mut targets = Vec::with_capacity(rows.len());
    targets.extend(
        rows.into_iter()
            .map(|(document, syntax_id)| semantic::Target::Internal {
                syntax_id,
                document,
                revision_id: store.revision_id().to_owned(),
            }),
    );
    Ok(targets)
}
fn declaration_target<S: JoinStore, G: GlobalJoinReads>(
    store: &S,
    index: &G,
    document: &native::DocumentKey,
    symbol: &str,
    operation: &mut S::Operation,
) -> Result<Option<semantic::Target>> {
    let items = declaration_targets(store, index, document, symbol, operation)?;
    Ok((items.len() == 1)
        .then(|| items.into_iter().next())
        .flatten())
}

struct MeasuredOccurrence {
    row: OccurrenceRow,
    compatible_syntax_id: Option<String>,
    /// How the exactly bound definition's native declaration can be called (#470).
    callable: TargetCallable,
}

/// How a call's exact bound target declaration can be called, measured from its native
/// declaration (owner rulings 2026-10-30, #470, and 2026-10-31, #557). Python and a Rust
/// `impl Trait for Type` method are not classified; they, like every other declaration, are
/// `Other`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum TargetCallable {
    #[default]
    Other = 0,
    /// A free function: a JavaScript `function` declaration or expression, or a Rust `fn`
    /// that is not an `impl` or trait item.
    FreeFunction = 1,
    /// A JavaScript class, or its `constructor` method.
    Constructor = 2,
    /// One fixed body: a Rust method of an inherent `impl`, or a Java `static`, `private` or
    /// `final` method.
    FixedMethod = 3,
    /// A declaration, not the runtime body: a Rust `trait` method, a JavaScript method other
    /// than `constructor`, or any other Java method.
    DynamicMethod = 4,
}

impl TargetCallable {
    /// `parent` is the immediate parent declaration of a Rust `implementation` method.
    fn of(declaration: &native::Declaration, parent: Option<&native::Declaration>) -> Self {
        match (
            declaration.document.language.as_str(),
            declaration.kind.as_str(),
        ) {
            ("javascript" | "rust", "function") => Self::FreeFunction,
            ("javascript", "type") => Self::Constructor,
            ("javascript", "method") if declaration.name.as_deref() == Some("constructor") => {
                Self::Constructor
            }
            ("javascript", "method") => Self::DynamicMethod,
            // The only native `type` that holds a method is a trait.
            ("rust", "method")
                if declaration
                    .ancestors
                    .last()
                    .is_some_and(|parent| parent.kind == "type") =>
            {
                Self::DynamicMethod
            }
            // `impl Type` measures no trait base; `impl Trait for Type` stays unclassified.
            ("rust", "method")
                if parent.is_some_and(|parent| {
                    parent.kind == "implementation" && parent.header.bases.is_empty()
                }) =>
            {
                Self::FixedMethod
            }
            ("java", "method")
                if declaration.header.modifiers.iter().any(|modifier| {
                    matches!(modifier.as_str(), "static" | "private" | "final")
                }) =>
            {
                Self::FixedMethod
            }
            ("java", "method") => Self::DynamicMethod,
            _ => Self::Other,
        }
    }

    /// The immediate parent a Rust method's classification reads, which is only its
    /// `implementation`: a method's measured ancestors name the parent's identity input.
    fn parent<'s, S: JoinStore>(
        store: &'s S,
        declaration: &native::Declaration,
    ) -> Result<Option<std::borrow::Cow<'s, native::Declaration>>> {
        let document = &declaration.document;
        match declaration.ancestors.split_last() {
            Some((parent, ancestors))
                if document.language == "rust"
                    && declaration.kind == "method"
                    && parent.kind == "implementation" =>
            {
                let id = native_ids::syntax_id(&serde_json::json!({
                    "sourceSet": store.source_set_id(),
                    "path": document.path,
                    "language": document.language,
                    "ancestors": ancestors,
                    "declaration": parent,
                }))?;
                store.declaration_of(document, &id)
            }
            _ => Ok(None),
        }
    }

    pub(super) fn from_code(code: i64) -> Result<Self> {
        Ok(match code {
            0 => Self::Other,
            1 => Self::FreeFunction,
            2 => Self::Constructor,
            3 => Self::FixedMethod,
            4 => Self::DynamicMethod,
            _ => anyhow::bail!("corrupt staged target callable"),
        })
    }
}

/// The measured syntactic shape of a native call (#470), read from its parser-measured
/// invocation and callee ranges and the authenticated source bytes between their starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallShape {
    /// The callee token starts the call: a JavaScript or Rust identifier callee (`foo()`,
    /// Rust `foo::<T>()`).
    Plain,
    /// A Rust path callee (`m::foo()`, `crate::m::foo()`): only path segments precede it.
    Path,
    /// A JavaScript `new` expression whose constructor is an identifier or dotted name.
    New,
    /// The callee token follows a `.`: `x.m()`, `self.m()`, `a?.b()`, Java `this.m()`.
    Member,
    /// A compound callee, a call without a measured callee token, or anything not proved above.
    Other,
}

fn call_shape(language: &str, call: &native::Call, source: &[u8]) -> CallShape {
    let Some(callee) = &call.callee_range else {
        return CallShape::Other;
    };
    let Some(prefix) = source.get(call.range.start..callee.start) else {
        return CallShape::Other;
    };
    if prefix.is_empty() {
        return CallShape::Plain;
    }
    match language {
        // `new`, whitespace, then only a dotted name: the native callee of a `new_expression`
        // is its constructor identifier or the last property of its dotted constructor, and
        // no call or member expression on another value fits in that prefix.
        "javascript" => match prefix.strip_prefix(b"new") {
            Some(rest)
                if rest.first().is_some_and(u8::is_ascii_whitespace)
                    && rest.iter().all(|b| {
                        b.is_ascii_alphanumeric()
                            || b.is_ascii_whitespace()
                            || matches!(b, b'_' | b'$' | b'.')
                    }) =>
            {
                CallShape::New
            }
            _ if member_prefix(prefix) => CallShape::Member,
            _ => CallShape::Other,
        },
        // A path callee's native token is its `scoped_identifier` name, so the prefix is path
        // segments ending in `::`. A method call's prefix holds the receiver and its `.`.
        "rust"
            if prefix.trim_ascii_end().ends_with(b"::")
                && !prefix.iter().any(|b| {
                    matches!(
                        b,
                        b'.' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'/' | b'"' | b'\''
                    )
                }) =>
        {
            CallShape::Path
        }
        "rust" | "java" if member_prefix(prefix) => CallShape::Member,
        _ => CallShape::Other,
    }
}

/// A call whose callee token directly follows a `.`. The receiver before it can be any
/// expression; only the separator is read.
fn member_prefix(prefix: &[u8]) -> bool {
    prefix.trim_ascii_end().ends_with(b".")
}

/// A call binding's dispatch (owner rulings 2026-10-30, #470, and 2026-10-31, #557), from the
/// measured native call shape and the exact bound target's native declaration only: a plain
/// JavaScript call, or a plain or path Rust call, to a free function is `direct`, and so is a
/// Rust member or path call to an inherent method and a Java plain or member call to a
/// `static`, `private` or `final` method; a JavaScript `new` of a class or its constructor is
/// `constructor`; a Rust trait method, a JavaScript member call to a method and any other Java
/// method call is `dynamic`; everything else stays `unknown`.
fn call_dispatch(language: &str, shape: CallShape, callable: TargetCallable) -> semantic::Dispatch {
    use CallShape::{Member, New, Path, Plain};
    use TargetCallable::{Constructor, DynamicMethod, FixedMethod, FreeFunction};
    match (language, shape, callable) {
        ("javascript", Plain, FreeFunction)
        | ("rust", Plain | Path, FreeFunction)
        | ("rust", Member | Path, FixedMethod)
        | ("java", Plain | Member, FixedMethod) => semantic::Dispatch::Direct,
        ("javascript", New, Constructor) => semantic::Dispatch::Constructor,
        ("javascript", Member, DynamicMethod)
        | ("rust", Member | Path, DynamicMethod)
        | ("java", Plain | Member, DynamicMethod) => semantic::Dispatch::Dynamic,
        _ => semantic::Dispatch::Unknown,
    }
}

/// A settled call binding's dispatch (#470): its join-time classification holds only while
/// the settled row still resolves, with a verified target, to the same internal declaration it
/// was classified against. Ambiguous, external, unresolved and stale outcomes are `unknown`.
pub(super) fn settled_dispatch(
    joined: semantic::Dispatch,
    joined_target: Option<&semantic::Target>,
    settled: &semantic::CallBinding,
) -> semantic::Dispatch {
    if resolves_to(joined_target, settled) {
        joined
    } else {
        semantic::Dispatch::Unknown
    }
}

/// Whether the settled row still resolves, with a verified target, to `joined_target`.
fn resolves_to(joined_target: Option<&semantic::Target>, settled: &semantic::CallBinding) -> bool {
    matches!(
        (joined_target, &settled.declared_target),
        (
            Some(semantic::Target::Internal {
                syntax_id: a,
                document: x,
                ..
            }),
            Some(semantic::Target::Internal {
                syntax_id: b,
                document: y,
                ..
            }),
        ) if settled.resolution == semantic::Resolution::Resolved
            && settled.stale_target == Some(false)
            && a == b
            && x == y
    )
}

/// A rejoined call's dispatch: the classification of whichever join, destination first then
/// captured, was made against the settled target. A rejoin can settle a call on a captured
/// target retargeted to the destination (its syntax ID verified unchanged) that the
/// destination join alone did not resolve; the call's document bytes are unchanged, so the
/// captured shape and target kind still hold. A method's `direct` is the exception: the facts
/// behind it, its `impl`'s bases and its modifiers, are outside its syntax ID, so it does not
/// survive without a destination classification (#557). `target_is_method` reads the
/// destination's declaration of the settled target.
pub(super) fn rejoined_dispatch(
    destination: Option<(semantic::Dispatch, Option<&semantic::Target>)>,
    captured: Option<(semantic::Dispatch, Option<&semantic::Target>)>,
    settled: &semantic::CallBinding,
    target_is_method: impl FnOnce(&str, &native::DocumentKey) -> Result<bool>,
) -> Result<semantic::Dispatch> {
    if let Some((dispatch, target)) = destination
        && resolves_to(target, settled)
    {
        return Ok(dispatch);
    }
    let Some((dispatch, target)) = captured.filter(|(_, target)| resolves_to(*target, settled))
    else {
        return Ok(semantic::Dispatch::Unknown);
    };
    if dispatch == semantic::Dispatch::Direct
        && let Some(semantic::Target::Internal {
            syntax_id,
            document,
            ..
        }) = target
        && target_is_method(syntax_id, document)?
    {
        return Ok(semantic::Dispatch::Unknown);
    }
    Ok(dispatch)
}

fn measure_occurrence<S: JoinStore>(
    store: &S,
    claim: &CapturedClaim,
    doc: &native::DocumentKey,
    symbol: &str,
    roles: u64,
    range: native::Range,
    operation: &mut S::Operation,
) -> Result<Option<MeasuredOccurrence>> {
    // Valid empty module markers retain their raw proof and declared symbol,
    // but cannot author a measured name, reference or call association.
    if range.start == range.end {
        return Ok(None);
    }
    let binding = store.join_reserved(
        doc,
        range.clone(),
        semantic::JoinKind::DeclarationName,
        operation,
    )?;
    let reference =
        store.join_reserved(doc, range.clone(), semantic::JoinKind::Reference, operation)?;
    let call = store.join_reserved(doc, range.clone(), semantic::JoinKind::Callee, operation)?;
    let mut callable = TargetCallable::Other;
    let compatible_syntax_id = if roles & 1 != 0 && binding.status == semantic::JoinStatus::Exact {
        let candidate = store
            .declaration_of(doc, &binding.candidate_ids[0])?
            .context("missing exact native declaration")?;
        let parent = TargetCallable::parent(store, &candidate)?;
        callable = TargetCallable::of(&candidate, parent.as_deref());
        // A stated SCIP kind must agree with the measured native kind. An unspecified kind
        // (`0`, as scip-typescript 0.4.0 emits for every symbol) states nothing to conflict,
        // so it defers to the measured native kind of this exact, authenticated target (#473).
        let compatible = store
            .symbol_kind(&doc.language, &doc.path, symbol)?
            .is_some_and(|kind| match candidate.kind.as_str() {
                _ if kind == 0 => true,
                "type" => matches!(kind, 7 | 11 | 21),
                // Native JavaScript represents a class constructor as a method. SCIP can
                // state either its constructor semantics or its method declaration shape.
                "method"
                    if candidate.document.language == "javascript"
                        && candidate.name.as_deref() == Some("constructor") =>
                {
                    matches!(kind, 9 | 26)
                }
                // rust-analyzer 1.98.1 states `TraitMethod` for a bodiless trait method and
                // `StaticMethod` for an associated function without `self` (#557).
                "method" => matches!(kind, 26 | 70 | 80),
                _ => true,
            });
        if compatible {
            store.reserve_operation(operation, || Ok(binding.candidate_ids[0].len() as u64))?;
        }
        compatible.then(|| binding.candidate_ids[0].clone())
    } else {
        None
    };
    store.reserve_operation(operation, || {
        staged::sizes::sum(&[
            (std::mem::size_of::<OccurrenceRow>() - 3 * std::mem::size_of::<semantic::Join>())
                as u64,
            claim.captured_proof_id.len() as u64,
            claim.fact_digest.len() as u64,
            staged::sizes::document(doc)?,
            symbol.len() as u64,
        ])
    })?;
    Ok(Some(MeasuredOccurrence {
        row: OccurrenceRow {
            proof_id: claim.captured_proof_id.clone(),
            fact_digest: claim.fact_digest.clone(),
            document: doc.clone(),
            symbol: symbol.to_owned(),
            roles,
            binding,
            reference,
            call,
        },
        compatible_syntax_id,
        callable,
    }))
}

fn discover_join_claim<S: JoinStore, G: GlobalDiscoverySink>(
    admitted_encoding: u8,
    store: &S,
    global: &mut G,
    claim: &CapturedClaim,
    operation: &mut S::Operation,
) -> Result<Option<(native::Range, OccurrenceRow)>> {
    #[cfg(feature = "test-support")]
    let revision_id = store.revision_id();
    store.charge(1)?;
    store.lookup_proof(
        &claim.captured_proof_id,
        &claim.envelope.record_kind,
        &claim.fact_digest,
    )?;
    store.reserve_operation(operation, || {
        staged::sizes::sum(&[
            std::mem::size_of::<native::DocumentKey>() as u64,
            store.source_set_id().len() as u64,
            claim.language.len() as u64,
            claim.envelope.relative_path.len() as u64,
        ])
    })?;
    let doc = native::DocumentKey {
        source_set_id: store.source_set_id().to_owned(),
        language: claim.language.clone(),
        path: claim.envelope.relative_path.clone(),
    };
    store
        .bytes_of(&doc)?
        .context("missing admitted claim document")?;
    let target = claim_target(store.source_set_id(), claim);
    if claim.envelope.record_kind == "symbolInformation" {
        if let Some(symbol) = claim.envelope.raw["symbol"].as_str() {
            global.symbol_presence(symbol, target.as_ref(), false, true)?;
        }
    } else if claim.envelope.record_kind == "occurrence" {
        let symbol = claim.envelope.raw["symbol"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("missing occurrence symbol")?;
        let range = store.occurrence_range(claim, admitted_encoding)?;
        #[cfg(feature = "test-support")]
        count_publication_work(|work| {
            if work.destination_revision == revision_id {
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
        if declaration {
            global.symbol_presence(symbol, target.as_ref(), true, false)?;
        }
        let Some(measured) =
            measure_occurrence(store, claim, &doc, symbol, roles, range.clone(), operation)?
        else {
            return Ok(None);
        };
        if roles & 1 != 0 && measured.row.binding.status == semantic::JoinStatus::Exact {
            let key = defining_key(symbol, &doc, target.as_ref());
            store.reserve_operation(operation, || {
                staged::sizes::sum(&[
                    std::mem::size_of::<ScopedKey>() as u64,
                    symbol.len() as u64,
                    key.1.as_ref().map_or(Ok(0), staged::sizes::document)?,
                    std::mem::size_of::<native::DocumentKey>() as u64,
                    staged::sizes::document(&doc)?,
                    std::mem::size_of::<String>() as u64,
                    measured.row.binding.candidate_ids[0].len() as u64,
                ])
            })?;
            global.exact_definition(
                key,
                doc.clone(),
                measured.row.binding.candidate_ids[0].clone(),
                measured.compatible_syntax_id.is_some(),
                measured.callable,
            )?;
        }
        return Ok(Some((range, measured.row)));
    }
    Ok(None)
}

fn build_join_index<S: IndexedJoinStore>(admitted_encoding: u8, store: &S) -> Result<S::Index> {
    let mut index = store.new_index();
    for claim in store.claims() {
        let claim = claim?;
        let mut operation = store.operation()?;
        if let Some((range, row)) =
            discover_join_claim(admitted_encoding, store, &mut index, &claim, &mut operation)?
        {
            index.put_type_use(&row.document, range.start, range.end, &row.symbol)?;
            index.put_occurrence(row)?;
        }
    }
    Ok(index)
}

fn emit_join_symbols<S: JoinStore, G: GlobalJoinReads, K: JoinSink>(
    store: &S,
    index: &G,
    out: &mut K,
) -> Result<()> {
    let source_set_id = store.source_set_id();
    for claim in store.claims() {
        let claim = claim?;
        if claim.envelope.record_kind != "symbolInformation" {
            continue;
        }
        let mut operation = store.operation()?;
        store.reserve_operation(&mut operation, || {
            staged::sizes::sum(&[
                std::mem::size_of::<native::DocumentKey>() as u64,
                source_set_id.len() as u64,
                claim.language.len() as u64,
                claim.envelope.relative_path.len() as u64,
            ])
        })?;
        let doc = native::DocumentKey {
            source_set_id: source_set_id.to_owned(),
            language: claim.language.clone(),
            path: claim.envelope.relative_path.clone(),
        };
        let key = claim.envelope.raw["symbol"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("missing symbol key")?;
        let scoped = defining_key(key, &doc, claim_target(source_set_id, &claim).as_ref());
        let declarations = scoped_declaration_targets(store, index, &scoped, &mut operation)?;
        let display_name = claim.envelope.raw["displayName"]
            .as_str()
            .filter(|s| !s.is_empty());
        store.reserve_operation(&mut operation, || {
            staged::sizes::sum(&[
                std::mem::size_of::<semantic::Symbol>() as u64,
                key.len() as u64,
                scoped.1.as_ref().map_or(Ok(0), staged::sizes::document)?,
                display_name.map_or(0, |s| s.len() as u64),
                claim.captured_proof_id.len() as u64,
            ])
        })?;
        out.symbol(semantic::Symbol {
            key: scoped_symbol_key(&scoped),
            display_name: display_name.map(str::to_owned),
            declarations,
            provenance_id: claim.captured_proof_id.clone(),
        })?;
    }
    Ok(())
}

fn join_occurrences<S: IndexedJoinStore, K: JoinSink>(
    admitted_encoding: u8,
    store: &S,
    index: &mut S::Index,
    out: &mut K,
) -> Result<()> {
    let mut role_sites = None;
    for ordinal in 0..index.occurrence_count() {
        let mut operation = store.operation()?;
        store.reserve_operation(&mut operation, || index.occurrence_clone_bytes(ordinal))?;
        let row = index.occurrence(ordinal)?;
        join_occurrence(
            admitted_encoding,
            store,
            index,
            out,
            row,
            &mut role_sites,
            &mut operation,
        )?;
    }
    Ok(())
}

fn join_occurrence<S: JoinStore, I: DocumentJoinState, K: JoinSink>(
    admitted_encoding: u8,
    store: &S,
    index: &mut I,
    out: &mut K,
    measured: OccurrenceRow,
    role_sites: &mut Option<(native::DocumentKey, RoleSites, S::Operation)>,
    operation: &mut S::Operation,
) -> Result<()> {
    let revision_id = store.revision_id();
    let OccurrenceRow {
        proof_id,
        fact_digest,
        document: doc,
        symbol,
        roles,
        binding,
        reference,
        call,
    } = measured;
    let claim = store.claim(&proof_id, "occurrence", &fact_digest)?;
    let scoped = using_key(
        index,
        &symbol,
        &doc,
        claim_target(store.source_set_id(), &claim).as_ref(),
    )?;
    store.reserve_operation(operation, || {
        staged::sizes::sum(&[
            std::mem::size_of::<semantic::SymbolKey>() as u64,
            symbol.len() as u64,
            scoped.1.as_ref().map_or(Ok(0), staged::sizes::document)?,
        ])
    })?;
    let key = scoped_symbol_key(&scoped);
    let mut asserted = scoped_declaration_targets(store, index, &scoped, operation)?;
    // A symbol the artifact never declares nor describes is external, unless a source of this
    // language wasn't read: the producer saw it, but its claims are dropped (#576).
    if asserted.is_empty()
        && key.scope == semantic::SymbolScope::Global
        && index.presence(&symbol, None)? == (false, false)
        && !store.has_unread_source(&doc.language)?
    {
        store.reserve_operation(operation, || {
            staged::sizes::sum(&[
                staged::sizes::symbol_key(&key)?,
                4 * std::mem::size_of::<semantic::Target>() as u64,
            ])
        })?;
        asserted.push(semantic::Target::External {
            symbol: key.clone(),
        });
    }
    store.reserve_operation(operation, || staged::target_size::sort(&asserted))?;
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
        out.join(&claim.captured_proof_id, &binding)?;
        store.reserve_operation(operation, || {
            staged::sizes::sum(&[
                (std::mem::size_of::<semantic::DeclarationBinding>()
                    - std::mem::size_of::<semantic::Join>()) as u64,
                staged::sizes::join(&binding)?,
                claim.captured_proof_id.len() as u64,
                if binding.status == semantic::JoinStatus::Exact {
                    staged::sizes::sum(&[
                        binding.candidate_ids[0].len() as u64,
                        std::mem::size_of::<semantic::SymbolKey>() as u64,
                        staged::sizes::symbol_key(&key)?,
                    ])?
                } else {
                    0
                },
            ])
        })?;
        out.declaration_binding(semantic::DeclarationBinding {
            syntax_id: (binding.status == semantic::JoinStatus::Exact)
                .then(|| binding.candidate_ids[0].clone()),
            symbols: if binding.status == semantic::JoinStatus::Exact {
                vec![key.clone()]
            } else {
                vec![]
            },
            join: binding.clone(),
            provenance_id: claim.captured_proof_id.clone(),
        })?;
    }
    out.join(&claim.captured_proof_id, &reference)?;
    let invocation = if roles & 1 == 0 && !claim.envelope.raw["enclosingRange"].is_null() {
        let span = store.enclosing_occurrence_range(&claim, admitted_encoding)?;
        let own = store.occurrence_range(&claim, admitted_encoding)?;
        // A SCIP enclosing range encloses its own occurrence. rust-analyzer instead gives a
        // same-document reference its target's definition range (#394), which encloses some
        // other span and says nothing about a call; that is no invocation witness.
        (span.start <= own.start && own.end <= span.end)
            .then(|| store.join_reserved(&doc, span, semantic::JoinKind::Invocation, operation))
            .transpose()?
            // Owner ruling 4 (2026-10-30, #394): an enclosing range that does not exactly join
            // an invocation is treated as absent. A recursive rust-analyzer call's range is its
            // target's own definition, which encloses the reference but is no call.
            .filter(|join| join.status == semantic::JoinStatus::Exact)
    } else {
        None
    };
    // The measured call this use occurrence names (#444). SCIP has no call role, and
    // scip-typescript, scip-java and scip-python never give a reference an enclosing range,
    // so the occurrence's exact span on one measured callee identifies the call by itself.
    // A producer-supplied enclosing range that exactly joins an invocation must join that
    // same call; one that joins none is absent, and the exact callee span alone decides.
    let witness = match &invocation {
        Some(j) => (j.status == semantic::JoinStatus::Exact
            && (call.status != semantic::JoinStatus::Exact
                || call.candidate_ids == j.candidate_ids))
            .then_some(j),
        None => (roles & 1 == 0 && call.status == semantic::JoinStatus::Exact).then_some(&call),
    };
    if reference.status == semantic::JoinStatus::Exact {
        let r = store
            .reference_of(&doc, &reference.candidate_ids[0])?
            .context("missing indexed native reference")?;
        let declaration_site = roles & 1 != 0
            && store.has_declaration_name(&doc, r.range.start, r.range.end, &r.spelling)?;
        let is_call = match witness {
            Some(j) => store
                .calls_with_id(&doc, &j.candidate_ids[0], operation)?
                .iter()
                .any(|c| {
                    c.id == j.candidate_ids[0]
                        && c.document == doc
                        && c.owner_syntax_id == r.owner_syntax_id
                        && c.range.start <= r.range.start
                        && r.range.end <= c.range.end
                        && c.spelling.as_deref() == Some(&r.spelling)
                        && c.callee_range
                            .as_ref()
                            .is_none_or(|callee| callee == &r.range)
                }),
            None => false,
        };
        store.reserve_operation(operation, || {
            staged::sizes::map_entries::<semantic::Role, ()>(7)
        })?;
        let mut assigned = BTreeSet::new();
        if declaration_site {
            assigned.insert(semantic::Role::Definition);
        }
        let site = (r.range.start, r.range.end);
        if role_sites
            .as_ref()
            .is_none_or(|(owner, _, _)| owner != &doc)
        {
            let mut cache_operation = store.operation()?;
            store.reserve_operation(&mut cache_operation, || {
                staged::sizes::sum(&[
                    std::mem::size_of::<native::DocumentKey>() as u64,
                    staged::sizes::document(&doc)?,
                ])
            })?;
            let sites = store.role_sites_reserved(&doc, &mut cache_operation)?;
            *role_sites = Some((doc.clone(), sites, cache_operation));
        }
        let sites = &role_sites.as_ref().expect("role sites just cached").1;
        if roles & 2 != 0 && sites.imports.contains(&site) {
            assigned.insert(semantic::Role::Import);
        }
        if !declaration_site && sites.types.contains(&site) && {
            store.reserve_operation(operation, || {
                index.type_uses_clone_bytes(&doc, site.0, site.1)
            })?;
            let keys = index.type_uses(&doc, site.0, site.1)?;
            keys.len() == 1 && keys.contains(&symbol)
        } {
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
                .declarations_with_id(&doc, &binding.candidate_ids[0], operation)?
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
                store.reserve_operation(operation, || staged::sizes::target(target))?;
                index.put_reference_claim(&r.id, &claim.captured_proof_id, target.clone())?;
            }
        }
        if !assigned.is_empty() {
            store.reserve_operation(operation, || {
                staged::sizes::sum(&[
                    std::mem::size_of::<semantic::Reference>() as u64,
                    r.id.len() as u64,
                    r.owner_syntax_id.len() as u64,
                    staged::sizes::document(&doc)?,
                    revision_id.len() as u64,
                    r.spelling.len() as u64,
                    r.lookup_key.len() as u64,
                    (assigned.len() * std::mem::size_of::<semantic::Role>()) as u64,
                    resolved
                        .as_ref()
                        .map_or(Ok(0), staged::sizes::target_payload)?,
                    staged::sizes::targets(&candidates)?,
                    claim.captured_proof_id.len() as u64,
                ])
            })?;
            let reference = semantic::Reference {
                id: r.id.clone(),
                owner_syntax_id: r.owner_syntax_id.clone(),
                ordinal: r.ordinal,
                document: doc.clone(),
                revision_id: revision_id.to_owned(),
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
            store.reserve_operation(operation, || staged::sizes::reference(&reference))?;
            out.reference_claim(&claim.captured_proof_id, reference.clone())?;
            #[cfg(test)]
            complexity_work(|work| work.reference_map_lookups += 1);
            if let Some(slot) = out.reference_slot(&r.id)? {
                store.reserve_operation(operation, || out.reference_clone_bytes(slot))?;
                let mut prior = out.reference(slot)?;
                assigned.extend(prior.roles.iter().copied());
                store.reserve_operation(operation, || {
                    Ok((assigned.len() * std::mem::size_of::<semantic::Role>()) as u64)
                })?;
                prior.roles = assigned.into_iter().collect();
                if prior.roles.contains(&semantic::Role::Definition) {
                    prior.site = semantic::ReferenceSite::Declaration;
                }
                out.put_reference(Some(slot), prior)?;
            } else {
                out.put_reference(None, reference)?;
            }
        }
    }
    if roles & 1 == 0 {
        let source = store.bytes_of(&doc)?.context("missing occurrence source")?;
        if let Some(join) = &invocation {
            out.join(&claim.captured_proof_id, join)?;
        }
        let selected = if call.status == semantic::JoinStatus::Exact {
            &call
        } else {
            invocation
                .as_ref()
                .filter(|j| j.status == semantic::JoinStatus::Exact)
                .unwrap_or(&call)
        };
        store.reserve_operation(operation, || staged::sizes::join(selected))?;
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
        out.join(&claim.captured_proof_id, &call)?;
        if candidate.status == semantic::JoinStatus::Exact
            && reference.status == semantic::JoinStatus::Exact
            && witness.is_some_and(|j| j.candidate_ids == candidate.candidate_ids)
        {
            let actual = store.call_of(&doc, &candidate.candidate_ids[0])?;
            let owning_references =
                store.references_with_id(&doc, &reference.candidate_ids[0], operation)?;
            if let Some(actual) = actual.as_deref().filter(|c| {
                let primary = &reference.anchor.range;
                c.document == doc
                    && c.range.start <= primary.start
                    && primary.end <= c.range.end
                    && c.callee_range.as_ref().is_none_or(|r| r == primary)
                    && c.spelling.as_deref()
                        == source
                            .get(primary.start..primary.end)
                            .and_then(|b| std::str::from_utf8(b).ok())
                    && owning_references.iter().any(|r| {
                        r.id == reference.candidate_ids[0]
                            && r.owner_syntax_id == c.owner_syntax_id
                            && r.range == *primary
                    })
            }) {
                // #470: only an exact, resolved internal target of this language is classified.
                let dispatch = match &resolved {
                    Some(semantic::Target::Internal {
                        syntax_id,
                        document,
                        ..
                    }) if resolution == semantic::Resolution::Resolved
                        && document.language == doc.language =>
                    {
                        match call_shape(&doc.language, actual, &source) {
                            CallShape::Other => semantic::Dispatch::Unknown,
                            shape => call_dispatch(
                                &doc.language,
                                shape,
                                index.target_callable(document, syntax_id)?,
                            ),
                        }
                    }
                    _ => semantic::Dispatch::Unknown,
                };
                index.put_call_reference(&claim.captured_proof_id, &reference.candidate_ids[0])?;
                store.reserve_operation(operation, || {
                    staged::sizes::sum(&[
                        (std::mem::size_of::<semantic::CallBinding>()
                            - std::mem::size_of::<semantic::Join>()) as u64,
                        candidate.candidate_ids[0].len() as u64,
                        claim.captured_proof_id.len() as u64,
                    ])
                })?;
                out.put_call_binding(
                    None,
                    semantic::CallBinding {
                        call_id: Some(candidate.candidate_ids[0].clone()),
                        join: candidate,
                        resolution,
                        stale_target: resolved.as_ref().and_then(|t| {
                            matches!(t, semantic::Target::Internal { .. }).then_some(false)
                        }),
                        declared_target: resolved,
                        candidates,
                        dispatch,
                        possible_dispatch: vec![],
                        possible_dispatch_complete: false,
                        provenance_id: claim.captured_proof_id.clone(),
                    },
                )?;
            }
        }
    }
    Ok(())
}

fn settle_join_references<S: JoinStore, I: DocumentJoinState, K: JoinSink>(
    store: &S,
    index: &mut I,
    out: &mut K,
) -> Result<()> {
    for slot in 0..out.reference_count() {
        let mut operation = store.operation()?;
        store.reserve_operation(&mut operation, || out.reference_clone_bytes(slot))?;
        let mut reference = out.reference(slot)?;
        let (mut claims, _claims_reservation) = index.take_reference_claims(&reference.id)?;
        store.reserve_operation(&mut operation, || {
            staged::target_size::stable_sort::<(String, semantic::Target)>(claims.len())
        })?;
        claims.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some((proof, _)) = claims.first() {
            store.reserve_operation(&mut operation, || Ok(proof.len() as u64))?;
            reference.provenance_id = proof.clone();
        }
        store.reserve_operation(&mut operation, || {
            staged::target_size::target_slots(claims.len())
        })?;
        let mut targets: Vec<_> = claims.into_iter().map(|(_, target)| target).collect();
        store.reserve_operation(&mut operation, || staged::target_size::sort(&targets))?;
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
        out.put_reference(Some(slot), reference)?;
    }
    for slot in 0..out.call_binding_count() {
        let mut operation = store.operation()?;
        store.reserve_operation(&mut operation, || out.call_binding_clone_bytes(slot))?;
        let mut binding = out.call_binding(slot)?;
        store.reserve_operation(&mut operation, || {
            index.call_reference_clone_bytes(&binding.provenance_id)
        })?;
        let reference_slot = match index.call_reference(&binding.provenance_id)? {
            Some(id) => {
                #[cfg(test)]
                complexity_work(|work| work.reference_map_lookups += 1);
                out.reference_slot(&id)?
            }
            None => None,
        };
        if let Some(slot_of_reference) = reference_slot {
            store.reserve_operation(&mut operation, || {
                out.reference_clone_bytes(slot_of_reference)
            })?;
            let reference = out.reference(slot_of_reference)?;
            binding.resolution = reference.resolution;
            let joined = std::mem::replace(&mut binding.declared_target, reference.declared_target);
            binding.candidates = reference.candidates;
            binding.stale_target = binding
                .declared_target
                .as_ref()
                .and_then(|t| matches!(t, semantic::Target::Internal { .. }).then_some(false));
            binding.dispatch = settled_dispatch(binding.dispatch, joined.as_ref(), &binding);
            out.put_call_binding(Some(slot), binding)?;
        }
    }
    Ok(())
}

fn reserved_key_bytes<S: JoinStore>(
    store: &S,
    key: &native::Key,
    operation: &mut S::Operation,
) -> Result<Vec<u8>> {
    let mut capacity = 0;
    store.reserve_operation(operation, || {
        let (length, scratch) = staged::group_size::key_bytes(key)?;
        capacity = usize::try_from(length)?;
        staged::sizes::sum(&[length, scratch, std::mem::size_of::<Vec<u8>>() as u64])
    })?;
    let value = serde_json::to_value(key)?;
    let mut bytes = Vec::with_capacity(capacity);
    native_ids::canonical_into(&value, &mut |part| bytes.extend_from_slice(part));
    Ok(bytes)
}

fn method_target_owner_matches<S: JoinStore, I: DocumentJoinState>(
    store: &S,
    index: &I,
    method: &native::Declaration,
    symbol: &str,
) -> Result<bool> {
    let mut operation = store.operation()?;
    let Some((owner_symbol, _)) = symbol.rsplit_once('#') else {
        return Ok(false);
    };
    let owner_bytes = method
        .ancestors
        .last()
        .map(|key| reserved_key_bytes(store, key, &mut operation))
        .transpose()?;
    let owners = owner_bytes
        .as_ref()
        .map(|key| store.declarations_with_key(&method.document, key, &mut operation))
        .transpose()?
        .into_iter()
        .flatten();
    let mut matching = 0usize;
    for owner in owners {
        let Some(name) = owner.name_range.as_ref() else {
            continue;
        };
        if owner.document == method.document
            && owner.kind == "type"
            && method.ancestors.last() == Some(&owner.key)
            && owner.range.start <= method.range.start
            && method.range.end <= owner.range.end
        {
            let mut keys_operation = store.operation()?;
            store.reserve_operation(&mut keys_operation, || {
                index.type_uses_clone_bytes(&owner.document, name.start, name.end)
            })?;
            let keys = index.type_uses(&owner.document, name.start, name.end)?;
            if keys.len() == 1
                && keys
                    .iter()
                    .next()
                    .is_some_and(|key| key.strip_suffix('#') == Some(owner_symbol))
            {
                matching += 1;
            }
        }
    }
    Ok(matching == 1)
}

fn discover_join_methods<S: IndexedJoinStore>(store: &S, index: &mut S::Index) -> Result<()> {
    for doc in store.documents() {
        let doc = doc?;
        let mut operation = store.operation()?;
        for declaration in store.document_declarations(&doc, &mut operation)? {
            #[cfg(test)]
            complexity_work(|work| work.method_index_rows += 1);
            if declaration.kind != "method" {
                continue;
            }
            let Some(range) = &declaration.name_range else {
                continue;
            };
            let mut symbols_operation = store.operation()?;
            store.reserve_operation(&mut symbols_operation, || {
                index.type_uses_clone_bytes(&declaration.document, range.start, range.end)
            })?;
            for symbol in index.type_uses(&declaration.document, range.start, range.end)? {
                let summary = MethodSummary {
                    target_owner_matches: method_target_owner_matches(
                        store,
                        index,
                        &declaration,
                        &symbol,
                    )?,
                    method: declaration.as_ref().clone(),
                };
                index.put_method(&declaration.document, &symbol, &summary)?;
            }
        }
    }
    Ok(())
}

fn discover_relationship_claims<S: JoinStore, I: DocumentJoinState>(
    store: &S,
    index: &mut I,
) -> Result<()> {
    for claim in store.claims() {
        let claim = claim?;
        if claim.envelope.record_kind != "relationship" {
            continue;
        }
        let (language, path) = (&claim.language, &claim.envelope.relative_path);
        let source = claim.envelope.raw["sourceSymbol"].as_str();
        let target = claim.envelope.raw["targetSymbol"].as_str();
        let mut operation = store.operation()?;
        store.reserve_operation(&mut operation, || {
            staged::sizes::sum(&[
                std::mem::size_of::<RelationshipRow>() as u64,
                claim.captured_proof_id.len() as u64,
                source.map_or(0, |s| s.len() as u64),
                target.map_or(0, |s| s.len() as u64),
            ])
        })?;
        let row = RelationshipRow {
            proof_id: claim.captured_proof_id.clone(),
            source_symbol: source.map(str::to_owned),
            target_symbol: target.map(str::to_owned),
        };
        store.reserve_operation(&mut operation, || staged::sizes::relationship(&row))?;
        index.put_relationship(language, path, row.clone())?;
        if let (Some(source), Some(target)) = (&row.source_symbol, &row.target_symbol) {
            index.put_relationship_endpoint(language, path, source, target, &row)?;
        }
    }
    Ok(())
}

fn join_type_relationships<S: IndexedJoinStore, K: JoinSink>(
    store: &S,
    index: &mut S::Index,
    out: &mut K,
) -> Result<()> {
    discover_join_methods(store, index)?;
    discover_relationship_claims(store, index)?;
    emit_type_relationships(store, index, out)
}

fn emit_type_relationships<S: JoinStore, I: DocumentJoinState, K: JoinSink>(
    store: &S,
    index: &I,
    out: &mut K,
) -> Result<()> {
    let revision_id = store.revision_id();
    for doc in store.documents() {
        let doc = &doc?;
        let bytes = store
            .bytes_of(doc)?
            .context("missing admitted document source")?;
        let bytes: &[u8] = &bytes;
        #[cfg(feature = "test-support")]
        if matches!(doc.language.as_str(), "java" | "javascript" | "python") {
            note_parsed_type_document(doc);
        }
        let mut relationship_operation = store.operation()?;
        let parsed = store.take_parsed_tree(doc);
        let mut reserve_bases =
            |bytes: LazyBytes<'_>| store.reserve_operation(&mut relationship_operation, bytes);
        let bases = match doc.language.as_str() {
            "java" => direct_java_types(bytes, parsed, &mut reserve_bases)?,
            "javascript" => direct_script_types(
                bytes,
                tree_sitter_javascript::LANGUAGE.into(),
                "class_declaration",
                "class_heritage",
                parsed,
                &mut reserve_bases,
            )?,
            "python" => direct_script_types(
                bytes,
                tree_sitter_python::LANGUAGE.into(),
                "class_definition",
                "argument_list",
                parsed,
                &mut reserve_bases,
            )?,
            _ => continue,
        };
        store.reserve_operation(&mut relationship_operation, || {
            Ok(std::mem::size_of::<BTreeMap<(usize, usize), native::Range>>() as u64)
        })?;
        let mut java_bases = BTreeMap::new();
        if doc.language == "java" {
            for (owner, base, _) in &bases {
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    java_bases.entry((owner.start, owner.end))
                {
                    store.reserve_operation(&mut relationship_operation, || {
                        staged::sizes::map_entries::<(usize, usize), native::Range>(1)
                    })?;
                    entry.insert(base.clone());
                }
            }
        }
        store.reserve_operation(&mut relationship_operation, || {
            index.relationships_clone_bytes(&doc.language, &doc.path)
        })?;
        let document_relationships = index.relationships(&doc.language, &doc.path)?;
        for (owner_range, base_range, kind) in bases {
            let mut operation = store.operation()?;
            store.reserve_operation(&mut operation, || {
                index.type_uses_clone_bytes(doc, owner_range.start, owner_range.end)
            })?;
            store.reserve_operation(&mut operation, || {
                index.type_uses_clone_bytes(doc, base_range.start, base_range.end)
            })?;
            let owner = index.type_uses(doc, owner_range.start, owner_range.end)?;
            let base = index.type_uses(doc, base_range.start, base_range.end)?;
            if owner.len() != 1 || base.len() != 1 {
                continue;
            }
            let (source_symbol, target_symbol) =
                (owner.iter().next().unwrap(), base.iter().next().unwrap());
            let Some(source) =
                declaration_target(store, index, doc, source_symbol, &mut operation)?
            else {
                continue;
            };
            let mut target_candidates =
                declaration_targets(store, index, doc, target_symbol, &mut operation)?;
            if target_candidates.is_empty()
                && !target_symbol.starts_with("local ")
                && !index.is_information_symbol(target_symbol)?
                && !store.has_unread_source(&doc.language)?
            {
                store.reserve_operation(&mut operation, || {
                    staged::sizes::sum(&[
                        staged::target_size::target_slots(4)?,
                        target_symbol.len() as u64,
                    ])
                })?;
                target_candidates.push(semantic::Target::External {
                    symbol: symbol_key(target_symbol, doc),
                });
            }
            let Some(target) = (target_candidates.len() == 1)
                .then(|| target_candidates.pop())
                .flatten()
            else {
                if doc.language == "java" {
                    out.diagnostic(doc, "unmeasurable_external_base")?;
                }
                continue;
            };
            store.reserve_operation(&mut operation, || {
                index.relationship_endpoints_clone_bytes(
                    &doc.language,
                    &doc.path,
                    source_symbol,
                    target_symbol,
                )
            })?;
            for claim in index.relationship_endpoints(
                &doc.language,
                &doc.path,
                source_symbol,
                target_symbol,
            )? {
                let mut output_operation = store.operation()?;
                store.reserve_operation(&mut output_operation, || {
                    staged::sizes::sum(&[
                        std::mem::size_of::<semantic::TypeRelationship>() as u64,
                        staged::sizes::target_payload(&source)?,
                        staged::sizes::target_payload(&target)?,
                    ])
                })?;
                out.type_relationship(semantic::TypeRelationship {
                    kind,
                    source: source.clone(),
                    target: target.clone(),
                    provenance_id: claim.proof_id,
                })?;
            }
        }
        if doc.language == "java" {
            for claim in &document_relationships {
                let mut operation = store.operation()?;
                let (Some(source_symbol), Some(target_symbol)) = (
                    claim.source_symbol.as_deref(),
                    claim.target_symbol.as_deref(),
                ) else {
                    continue;
                };
                let source_methods = index.source_methods(doc, source_symbol, &mut |bytes| {
                    store.reserve_operation(&mut operation, || Ok(bytes))
                })?;
                #[cfg(test)]
                complexity_work(|work| work.method_lookup_rows += source_methods.len());
                let [source_method] = source_methods.as_slice() else {
                    if source_methods.is_empty()
                        && declaration_target(store, index, doc, source_symbol, &mut operation)?
                            .is_none()
                    {
                        out.diagnostic(doc, "unmeasurable_override_endpoint")?;
                    }
                    continue;
                };
                let Some(source_target) =
                    declaration_target(store, index, doc, source_symbol, &mut operation)?
                else {
                    continue;
                };
                let target_methods = index.target_methods(doc, target_symbol, &mut |bytes| {
                    store.reserve_operation(&mut operation, || Ok(bytes))
                })?;
                #[cfg(test)]
                complexity_work(|work| work.method_lookup_rows += target_methods.len());
                if target_methods.len() > 1 {
                    out.diagnostic(doc, "unmeasurable_override_endpoint")?;
                    continue;
                }
                let target_method = target_methods.first();
                let source_owner = source_method.ancestors.last();
                let owner_bytes = source_owner
                    .map(|key| reserved_key_bytes(store, key, &mut operation))
                    .transpose()?;
                let owner_rows = owner_bytes
                    .as_ref()
                    .map(|key| store.declarations_with_key(doc, key, &mut operation))
                    .transpose()?;
                let mut owner_declarations = owner_rows.unwrap_or_default();
                owner_declarations.retain(|d| {
                    d.document == *doc
                        && d.kind == "type"
                        && source_owner == Some(&d.key)
                        && d.range.start <= source_method.range.start
                        && source_method.range.end <= d.range.end
                });
                let base_uses = match (owner_declarations.len() == 1)
                    .then(|| &owner_declarations[0])
                    .and_then(|owner| owner.name_range.as_ref())
                    .and_then(|r| java_bases.get(&(r.start, r.end)))
                {
                    Some(range) => {
                        store.reserve_operation(&mut operation, || {
                            index.type_uses_clone_bytes(doc, range.start, range.end)
                        })?;
                        index.type_uses(doc, range.start, range.end)?
                    }
                    None => BTreeSet::new(),
                };
                let direct_base = (base_uses.len() == 1)
                    .then(|| base_uses.iter().next())
                    .flatten();
                let target_owner_key = target_symbol.rsplit_once('#').map(|(owner, _)| owner);
                let direct_owner = direct_base.and_then(|base| base.strip_suffix('#'));
                let method_descriptor_matches =
                    target_symbol.rsplit_once('#').is_some_and(|(_, suffix)| {
                        source_symbol
                            .rsplit_once('#')
                            .is_some_and(|(_, source_suffix)| {
                                target_method.is_none() || suffix == source_suffix
                            })
                    });
                let target_owner_matches =
                    target_method.is_none_or(|method| method.target_owner_matches);
                let owner_matches = direct_owner.is_some()
                    && direct_owner == target_owner_key
                    && method_descriptor_matches
                    && target_owner_matches;
                if !owner_matches {
                    if target_method.is_none()
                        && (direct_owner.is_none() || direct_owner != target_owner_key)
                    {
                        out.diagnostic(doc, "unmeasurable_override_endpoint")?;
                    }
                    continue;
                }
                let mut destination =
                    declaration_target(store, index, doc, target_symbol, &mut operation)?;
                // A global symbol could be declared in a source that wasn't read (#576).
                if destination.is_none()
                    && target_method.is_none()
                    && (target_symbol.starts_with("local ")
                        || !store.has_unread_source(&doc.language)?)
                {
                    store.reserve_operation(&mut operation, || {
                        staged::sizes::sum(&[
                            std::mem::size_of::<semantic::Target>() as u64,
                            target_symbol.len() as u64,
                            if target_symbol.starts_with("local ") {
                                staged::sizes::document(doc)?
                            } else {
                                0
                            },
                        ])
                    })?;
                    destination = Some(semantic::Target::External {
                        symbol: symbol_key(target_symbol, doc),
                    });
                }
                if let Some(destination) = destination {
                    store.reserve_operation(&mut operation, || {
                        staged::sizes::sum(&[
                            std::mem::size_of::<semantic::TypeRelationship>() as u64,
                            claim.proof_id.len() as u64,
                        ])
                    })?;
                    out.type_relationship(semantic::TypeRelationship {
                        kind: semantic::RelationshipKind::Overrides,
                        source: source_target,
                        target: destination,
                        provenance_id: claim.proof_id.clone(),
                    })?;
                }
            }
            let mut operation = store.operation()?;
            let document_types: Vec<_> = store
                .document_declarations(doc, &mut operation)?
                .into_iter()
                .filter(|d| d.kind == "type")
                .collect();
            for declaration in document_types.iter().filter(|d| {
                std::str::from_utf8(bytes).ok().is_some_and(|source| {
                    d.name_range
                        .as_ref()
                        .and_then(|r| source.get(..r.start))
                        .and_then(|prefix| prefix.rsplit(['{', '}', ';']).next())
                        .is_some_and(|prefix| prefix.split_whitespace().last() == Some("enum"))
                })
            }) {
                let mut enum_operation = store.operation()?;
                let Some(name) = &declaration.name_range else {
                    continue;
                };
                store.reserve_operation(&mut enum_operation, || {
                    index.type_uses_clone_bytes(doc, name.start, name.end)
                })?;
                let keys = index.type_uses(doc, name.start, name.end)?;
                if keys.len() != 1 {
                    continue;
                }
                let source_symbol = keys.iter().next().unwrap();
                if !matches!(declaration_target(store,index,doc,source_symbol,&mut enum_operation)?,
                    Some(semantic::Target::Internal {syntax_id,document,revision_id: target_revision})
                    if syntax_id==declaration.syntax_id && document==*doc && target_revision==revision_id)
                {
                    continue;
                }
                for claim in document_relationships.iter().filter(|c| {
                    c.source_symbol.as_deref() == Some(source_symbol.as_str())
                        && c.target_symbol
                            .as_deref()
                            .is_some_and(|s| s.ends_with("java/lang/Enum#"))
                }) {
                    let key = claim.target_symbol.as_deref().unwrap();
                    let mut output_operation = store.operation()?;
                    store.reserve_operation(&mut output_operation, || {
                        staged::sizes::sum(&[
                            std::mem::size_of::<semantic::TypeRelationship>() as u64,
                            declaration.syntax_id.len() as u64,
                            staged::sizes::document(doc)?,
                            revision_id.len() as u64,
                            key.len() as u64,
                            if key.starts_with("local ") {
                                staged::sizes::document(doc)?
                            } else {
                                0
                            },
                            claim.proof_id.len() as u64,
                        ])
                    })?;
                    out.type_relationship(semantic::TypeRelationship {
                        kind: semantic::RelationshipKind::Extends,
                        source: semantic::Target::Internal {
                            syntax_id: declaration.syntax_id.clone(),
                            document: doc.clone(),
                            revision_id: revision_id.to_owned(),
                        },
                        target: semantic::Target::External {
                            symbol: symbol_key(key, doc),
                        },
                        provenance_id: claim.proof_id.clone(),
                    })?;
                }
            }
        }
    }
    Ok(())
}

/// A clause node contributes only direct named type-use spans, never ancestors inferred
/// from producer flags. The exact raw occurrence at that span supplies endpoint identity.
type DirectBase = (native::Range, native::Range, semantic::RelationshipKind);

/// A lazily computed byte estimate handed to a reservation callback. A callback without a
/// live budget (the in-memory oracle) drops it unevaluated.
type LazyBytes<'a> = &'a mut dyn FnMut() -> Result<u64>;

// Count and build from the same parsed tree. Only Rust-owned output is admitted here;
// tree-sitter's C tree allocation is not covered by this reservation. The counting walk is
// part of the estimate, so an unmetered caller builds the Vec by ordinary growth.
fn collect_direct_bases(
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
    mut emit: impl FnMut(&mut dyn FnMut(DirectBase) -> Result<()>) -> Result<()>,
) -> Result<Vec<DirectBase>> {
    let mut count = 0usize;
    reserve(&mut || {
        emit(&mut |_| {
            count = count.checked_add(1).ok_or_else(|| {
                crate::scip_raw::ScipCaptureError::LimitExceeded("unit expansion".into())
            })?;
            Ok(())
        })?;
        let slots = (count as u64)
            .checked_mul(std::mem::size_of::<DirectBase>() as u64)
            .ok_or_else(|| {
                crate::scip_raw::ScipCaptureError::LimitExceeded("unit expansion".into())
            })?;
        staged::sizes::sum(&[std::mem::size_of::<Vec<DirectBase>>() as u64, slots])
    })?;
    let mut result = Vec::with_capacity(count);
    emit(&mut |row| {
        result.push(row);
        Ok(())
    })?;
    Ok(result)
}

fn direct_java_types(
    source: &[u8],
    parsed: Option<tree_sitter::Tree>,
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<Vec<DirectBase>> {
    let tree = match parsed {
        Some(tree) => tree,
        None => {
            #[cfg(test)]
            complexity_work(|work| work.java_type_parses += 1);
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&tree_sitter_java::LANGUAGE.into())?;
            parser.parse(source, None).context("Java parser failed")?
        }
    };
    collect_direct_bases(reserve, |emit| java_base_rows(tree.root_node(), emit))
}

fn java_base_rows(
    root: tree_sitter::Node<'_>,
    emit: &mut dyn FnMut(DirectBase) -> Result<()>,
) -> Result<()> {
    walk_named_reverse(root, &mut |node| {
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
            for (field, relationship) in [
                ("superclass", semantic::RelationshipKind::Extends),
                ("interfaces", semantic::RelationshipKind::Implements),
            ] {
                if let Some(clause) = node.child_by_field_name(field) {
                    java_clause_rows(clause, &owner, relationship, emit)?;
                }
            }
            if kind == "interface_declaration" {
                let mut cursor = node.walk();
                for child in node
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "extends_interfaces")
                {
                    java_clause_rows(child, &owner, semantic::RelationshipKind::Extends, emit)?;
                }
            }
        }
        Ok(true)
    })
}
fn java_clause_rows(
    clause: tree_sitter::Node<'_>,
    owner: &native::Range,
    relationship: semantic::RelationshipKind,
    emit: &mut dyn FnMut(DirectBase) -> Result<()>,
) -> Result<()> {
    walk_named_reverse(clause, &mut |item| {
        let mut base = None;
        if item.kind() == "type_identifier" {
            base = Some(item);
        } else if matches!(item.kind(), "generic_type" | "scoped_type_identifier") {
            let mut outer = item;
            while matches!(outer.kind(), "generic_type" | "scoped_type_identifier") {
                let index = if outer.kind() == "generic_type" {
                    0
                } else {
                    u32::try_from(outer.named_child_count().saturating_sub(1))?
                };
                let Some(child) = outer.named_child(index) else {
                    break;
                };
                outer = child;
                base = Some(outer);
            }
        } else {
            return Ok(true);
        }
        if let Some(base) = base {
            emit((
                owner.clone(),
                native::Range {
                    start: base.start_byte(),
                    end: base.end_byte(),
                },
                relationship,
            ))?;
        }
        Ok(false)
    })
}

#[derive(Default)]
struct RoleSites {
    types: BTreeSet<(usize, usize)>,
    imports: BTreeSet<(usize, usize)>,
}

/// Visits `root`'s named subtree in reverse pre-order (a node, then its named children last to
/// first), descending only where `visit` returns true: the former push-forward/pop-last
/// traversal. One tree cursor walks it; its stack is bounded by the tree depth, and unlike
/// `Node::parent` and `Node::prev_named_sibling` no step re-descends from the tree root.
fn walk_named_reverse<'t>(
    root: tree_sitter::Node<'t>,
    visit: &mut impl FnMut(tree_sitter::Node<'t>) -> Result<bool>,
) -> Result<()> {
    let mut cursor = root.walk();
    loop {
        if visit(cursor.node())? && goto_last_named_child(&mut cursor) {
            continue;
        }
        loop {
            if cursor.node() == root {
                return Ok(());
            }
            if goto_previous_named_sibling(&mut cursor) {
                break;
            }
            // No earlier named sibling: the cursor rests on a sibling, whose parent is the same.
            anyhow::ensure!(cursor.goto_parent(), "missing AST parent");
        }
    }
}

/// Moves to the last named child, or leaves the cursor where it was.
fn goto_last_named_child(cursor: &mut tree_sitter::TreeCursor<'_>) -> bool {
    if !cursor.goto_last_child() {
        return false;
    }
    if goto_named_or_previous(cursor) {
        return true;
    }
    cursor.goto_parent();
    false
}

/// Moves to the nearest earlier named sibling. On failure the cursor rests on some sibling of
/// the start node (or the start node itself).
fn goto_previous_named_sibling(cursor: &mut tree_sitter::TreeCursor<'_>) -> bool {
    cursor.goto_previous_sibling() && goto_named_or_previous(cursor)
}

/// Stays on a named node, else steps back through siblings to the first named one.
fn goto_named_or_previous(cursor: &mut tree_sitter::TreeCursor<'_>) -> bool {
    loop {
        if cursor.node().is_named() {
            return true;
        }
        if !cursor.goto_previous_sibling() {
            return false;
        }
    }
}

fn reserve_role_span(
    spans: &mut BTreeSet<(usize, usize)>,
    span: (usize, usize),
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<()> {
    if !spans.contains(&span) {
        reserve(&mut || staged::sizes::map_entries::<(usize, usize), ()>(1))?;
        spans.insert(span);
    }
    Ok(())
}

fn source_role_sites(
    language: &str,
    source: &[u8],
    report: Option<&native::CapabilityReport>,
) -> Result<RoleSites> {
    source_role_sites_reserved(language, source, report, &mut |_| Ok(()))
}

fn source_role_sites_reserved(
    language: &str,
    source: &[u8],
    report: Option<&native::CapabilityReport>,
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<RoleSites> {
    Ok(source_role_sites_parsed(language, source, report, reserve)?.0)
}

/// The role sites of `source`, and the syntax tree they were read from (`None` for a language
/// without role sites). Each supported language's tree is the one its direct type bases are
/// read from too. `report` is the document's capability report, whose measured macros hold sites
/// of their own.
fn source_role_sites_parsed(
    language: &str,
    source: &[u8],
    report: Option<&native::CapabilityReport>,
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<(RoleSites, Option<tree_sitter::Tree>)> {
    reserve(&mut || Ok(std::mem::size_of::<RoleSites>() as u64))?;
    let grammar: tree_sitter::Language = match language {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        _ => return Ok((RoleSites::default(), None)),
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar)?;
    let tree = parser
        .parse(source, None)
        .context("semantic role source parser failed")?;
    let mut sites = RoleSites::default();
    let mut visit = |node: tree_sitter::Node<'_>| {
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
            collect_role_leaf_spans_reserved(field, language, &mut sites.types, reserve)?;
        }
        if language == "rust" && kind == "impl_item" {
            for field in ["trait", "type"] {
                if let Some(child) = node.child_by_field_name(field) {
                    collect_role_leaf_spans_reserved(child, language, &mut sites.types, reserve)?;
                }
            }
        }
        if language == "java"
            && matches!(
                kind,
                "superclass" | "super_interfaces" | "extends_interfaces"
            )
        {
            collect_role_leaf_spans_reserved(node, language, &mut sites.types, reserve)?;
        }
        if language == "javascript" && kind == "class_heritage" {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                js_direct_class_span(child, &mut sites.types, reserve)?;
            }
        }
        if language == "python" && kind == "class_definition" {
            let mut cursor = node.walk();
            for child in node
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "argument_list")
            {
                collect_role_leaf_spans_reserved(child, language, &mut sites.types, reserve)?;
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
            js_direct_class_span(right, &mut sites.types, reserve)?;
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
                collect_role_leaf_spans_reserved(child, language, &mut sites.imports, reserve)?;
            }
        }
        Ok(true)
    };
    walk_named_reverse(tree.root_node(), &mut visit)?;
    if let Some(report) = report.filter(|report| !report.macro_expansions.is_empty())
        && let Ok(text) = std::str::from_utf8(source)
    {
        // Measured macro contents have syntax only in their expansions (#551). A site missing
        // here is not a conservative omission: its occurrence would join as a plain read.
        use crate::native_rust_macros::{Expansions, MeasuredMacros};
        let mut expansions = Expansions::new(text);
        expansions.expand_measured(tree.root_node(), &MeasuredMacros::of(report));
        for arguments in expansions.arguments() {
            walk_named_reverse(arguments, &mut visit)?;
        }
    }
    Ok((sites, Some(tree)))
}

fn js_direct_class_span(
    node: tree_sitter::Node<'_>,
    spans: &mut BTreeSet<(usize, usize)>,
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<()> {
    let span = match node.kind() {
        "identifier" => Some((node.start_byte(), node.end_byte())),
        "member_expression" => node
            .child_by_field_name("property")
            .map(|property| (property.start_byte(), property.end_byte())),
        _ => None,
    };
    if let Some(span) = span {
        reserve_role_span(spans, span, reserve)?;
    }
    Ok(())
}
fn collect_role_leaf_spans_reserved(
    node: tree_sitter::Node<'_>,
    language: &str,
    spans: &mut BTreeSet<(usize, usize)>,
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<()> {
    walk_named_reverse(node, &mut |node| {
        let leaf = match language {
            "java" | "rust" => matches!(node.kind(), "identifier" | "type_identifier"),
            "javascript" => matches!(
                node.kind(),
                "identifier" | "property_identifier" | "shorthand_property_identifier"
            ),
            "python" => node.kind() == "identifier",
            _ => false,
        };
        if leaf {
            reserve_role_span(spans, (node.start_byte(), node.end_byte()), reserve)?;
        }
        Ok(!leaf)
    })
}

fn direct_script_types(
    source: &[u8],
    language: tree_sitter::Language,
    owner_kind: &str,
    clause_kind: &str,
    parsed: Option<tree_sitter::Tree>,
    reserve: &mut impl FnMut(LazyBytes<'_>) -> Result<()>,
) -> Result<Vec<DirectBase>> {
    let tree = match parsed {
        Some(tree) => tree,
        None => {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&language)?;
            parser
                .parse(source, None)
                .context("semantic source parser failed")?
        }
    };
    collect_direct_bases(reserve, |emit| {
        walk_named_reverse(tree.root_node(), &mut |node| {
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
                    .filter(|child| child.kind() == clause_kind)
                {
                    let mut child_cursor = clause.walk();
                    for base in clause.named_children(&mut child_cursor) {
                        let name = match base.kind() {
                            "identifier" => Some(base),
                            "attribute" => base.named_child(u32::try_from(
                                base.named_child_count().saturating_sub(1),
                            )?),
                            _ => None,
                        };
                        if let Some(name) = name {
                            emit((
                                owner.clone(),
                                native::Range {
                                    start: name.start_byte(),
                                    end: name.end_byte(),
                                },
                                semantic::RelationshipKind::Extends,
                            ))?;
                        }
                    }
                }
            }
            Ok(true)
        })
    })
}

/// The original associations of one reused document, keyed by raw proof, from its admitted
/// tuple. `inventory` holds the document's claims. A claim that authored nothing was unmatched
/// or unsupported at admission and stays so against the same document version. `None` sends
/// the rejoin to FULL when a stored row cannot be tied to one claim.
pub(crate) fn original_document_associations(
    snapshot: &semantic::SemanticSnapshot,
    inventory: &crate::scip_raw::ProofInventory,
    producer_id: &str,
    document: &native::DocumentKey,
) -> Option<JoinedFacts> {
    let mut by_provenance = BTreeMap::new();
    let mut kinds = BTreeMap::new();
    // One raw proof can stand behind several evidence kinds: a definition's binding and its
    // reference. A proof is fused when a kind of it has no row.
    let mut unclaimed = BTreeSet::new();
    let proofs = inventory.proof_lookup();
    for row in snapshot
        .provenance
        .iter()
        .filter(|row| row.producer_id == producer_id)
    {
        let derived = row.derived_from.as_ref()?;
        let kind = serde_json::to_value(derived.record_kind).ok()?;
        let claim = proofs
            .lookup(&derived.provenance_id, kind.as_str()?, &derived.fact_digest)
            .ok()?;
        if row.document.language != claim.language || row.document.path != claim.path {
            return None;
        }
        by_provenance.insert(row.id.as_str(), derived.provenance_id.as_str());
        kinds.insert(row.id.as_str(), row.evidence_kind);
        unclaimed.insert((derived.provenance_id.as_str(), row.evidence_kind));
    }
    let mut facts = JoinedFacts::default();
    let mut claimed = |id: &str| {
        if let (Some(raw), Some(kind)) = (by_provenance.get(id), kinds.get(id)) {
            unclaimed.remove(&(*raw, *kind));
        }
    };
    for row in &snapshot.symbols {
        if let Some(raw) = by_provenance.get(row.provenance_id.as_str()) {
            let mut value = row.clone();
            value.provenance_id = (*raw).to_owned();
            claimed(&row.provenance_id);
            facts.symbols.push(value);
        }
    }
    for row in &snapshot.declaration_bindings {
        if let Some(raw) = by_provenance.get(row.provenance_id.as_str()) {
            let mut value = row.clone();
            value.provenance_id = (*raw).to_owned();
            claimed(&row.provenance_id);
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
            claimed(&row.provenance_id);
            facts
                .reference_claims
                .insert((*raw).to_owned(), value.clone());
            facts.references.push(value);
        }
    }
    for row in &snapshot.call_bindings {
        if let Some(raw) = by_provenance.get(row.provenance_id.as_str()) {
            if !facts.reference_claims.contains_key(*raw) {
                // The call's reference was fused into another proof's row at the call's span.
                let anchor = &row.join.anchor;
                let mut reference = facts
                    .references
                    .iter()
                    .find(|reference| {
                        reference.roles.contains(&semantic::Role::Call)
                            && (reference.range == anchor.range
                                || (anchor.kind == semantic::JoinKind::Invocation
                                    && anchor.range.start <= reference.range.start
                                    && reference.range.end <= anchor.range.end))
                    })?
                    .clone();
                facts.fused_calls.insert(reference.provenance_id.clone());
                facts.fused_calls.insert((*raw).to_owned());
                reference.provenance_id = (*raw).to_owned();
                facts.reference_claims.insert((*raw).to_owned(), reference);
            }
            let mut value = row.clone();
            value.provenance_id = (*raw).to_owned();
            claimed(&row.provenance_id);
            facts.call_bindings.push(value);
        }
    }
    for row in &snapshot.type_relationships {
        if let Some(raw) = by_provenance.get(row.provenance_id.as_str()) {
            let mut value = row.clone();
            value.provenance_id = (*raw).to_owned();
            claimed(&row.provenance_id);
            facts.type_relationships.push(value);
        }
    }
    for (raw, kind) in unclaimed {
        if kind != semantic::EvidenceKind::SemanticReference {
            return None;
        }
        facts.fused_proofs.insert(raw.to_owned());
    }
    if snapshot.coverage.iter().any(|row| {
        row.reasons
            .as_ref()
            .is_some_and(|reasons| reasons.captured_diagnostics)
    }) {
        facts.diagnostics.insert(document.clone(), BTreeSet::new());
    }
    Some(facts)
}

/// The internal targets `facts` name.
pub(crate) fn required_original_targets(
    facts: &JoinedFacts,
) -> BTreeSet<(native::DocumentKey, String)> {
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
    keys
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
    // A reused row is no destination classification of a target outside the reused documents:
    // the rejoin takes the captured one under its own rule, as a full rejoin would (#557).
    for row in &mut facts.call_bindings {
        if matches!(
            &row.declared_target,
            Some(semantic::Target::Internal { document, .. }) if !is_reused(document)
        ) {
            row.declared_target = None;
        }
    }
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
    let captured_store = MemoryJoinStore::new(&raw_index, &coordinates, &r1, captured_sources);
    let destination_store = MemoryJoinStore::new(&raw_index, &coordinates, r2, destination_sources);
    rejoin_with_stores(
        inventory,
        captured_native,
        destination_native,
        producer,
        basis,
        admitted_encoding,
        &captured_store,
        &destination_store,
        reusable,
    )
}

#[allow(clippy::too_many_arguments)]
fn rejoin_with_stores<S: IndexedJoinStore + NativeFacts>(
    inventory: &CapturedScipInventory,
    captured_native: &native::Artifact,
    destination_native: &native::Artifact,
    producer: semantic::Producer,
    basis: &BTreeMap<native::DocumentKey, semantic::SemanticBasis>,
    admitted_encoding: u8,
    captured_store: &S,
    destination_store: &S,
    reusable: Option<&BTreeSet<native::DocumentKey>>,
) -> Result<RejoinResult> {
    let captured = join_into_facts(admitted_encoding, captured_store)?;
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
    // Each document's unsupported claims by the kind of native gap that hid them (#520).
    let mut unsupported_documents: BTreeMap<native::DocumentKey, [u64; 4]> = BTreeMap::new();
    let mut failed = BTreeSet::new();
    let mut requested = BTreeSet::new();
    let unchanged_documents: BTreeSet<_> = captured_store
        .source_documents()
        .filter(|document| {
            destination_store.source_bytes(document) == captured_store.source_bytes(document)
        })
        .cloned()
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
    for claim in captured_store.claims() {
        let claim = claim?;
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
        captured_store
            .source_bytes(&doc)
            .context("missing captured claim source")?;
        if !unchanged_documents.contains(&doc) {
            failed.insert(doc);
            continue;
        }
        if claim.envelope.record_kind == "occurrence" {
            let range = captured_store.occurrence_range(&claim, admitted_encoding)?;
            if range.start == range.end {
                eligible.insert(doc);
                continue;
            }
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
            let mut measured_operation = measured.operation()?;
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
                unsupported_documents.entry(doc.clone()).or_default()
                    [semantic::CoverageReasons::gap_kind(first.diagnostic.as_deref())] += 1;
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
                let mut same_call = false;
                if let Some(id) = second.candidate_ids.first() {
                    let calls = match call.candidate_ids.first() {
                        Some(call_id) => {
                            measured.calls_with_id(&doc, call_id, &mut measured_operation)?
                        }
                        None => vec![],
                    };
                    same_call = measured
                        .references_with_id(&doc, id, &mut measured_operation)?
                        .iter()
                        .any(|reference| {
                            &reference.id == id
                                && calls.iter().any(|candidate| {
                                    call.candidate_ids == vec![candidate.id.clone()]
                                        && candidate.owner_syntax_id == reference.owner_syntax_id
                                        && candidate.range.start <= reference.range.start
                                        && reference.range.end <= candidate.range.end
                                        && candidate
                                            .callee_range
                                            .as_ref()
                                            .is_none_or(|callee| callee == &reference.range)
                                })
                        });
                }
                exact &= same_call;
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
            let joined = join_into_facts(admitted_encoding, &delta_store)?;
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
        let mut order = BTreeMap::new();
        for (ordinal, claim) in captured_store.claims().enumerate() {
            order.insert(claim?.captured_proof_id.clone(), ordinal);
        }
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
        join_into_facts(admitted_encoding, &filtered_store)?
    };
    let metadata = inventory.proof_identities();
    let filtered_metadata = filtered.proof_identities();
    let mut publication = empty_publication(&producer);
    let report = finish_rejoin(
        &metadata,
        captured_native,
        destination_native,
        producer,
        basis,
        Some(captured_store),
        destination_store,
        None,
        captured_store,
        &filtered_store,
        &filtered_metadata,
        &exclusions,
        captured,
        destination,
        requested,
        failed,
        unsupported_claims,
        unsupported_documents,
        &mut publication,
        None,
    )?;
    Ok(RejoinResult {
        publication,
        report,
    })
}

/// The provenance ID of a rejoined fact: a digest of the raw proof, the destination revision
/// and the evidence kind, so it needs no lookup table.
fn rejoin_provenance_id(raw_id: &str, revision_id: &str, kind: semantic::EvidenceKind) -> String {
    native_ids::digest(
        b"trellis.semantic-rejoin.v1\0",
        &native_ids::canonical(
            &serde_json::json!({"proof":raw_id,"revision":revision_id,"kind":kind}),
        ),
    )
}

/// Test-support edits to the destination's measured facts, applied only when the captured and
/// destination revisions differ. Both join backends take them as a value; the in-memory
/// destination applies them to the whole artifact, the staged one to each document it reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DestinationMutations {
    pub duplicate_b_target: bool,
    pub duplicate_a_reference: bool,
    pub missing_a_reference: bool,
    /// B's `target` declaration gets another syntax ID: a destination target that is not a
    /// captured one.
    pub renamed_b_target: bool,
}

impl DestinationMutations {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Reads the `TRELLIS_TEST_SEMANTIC_*` hook variables.
    #[cfg(feature = "test-support")]
    pub fn from_env() -> Self {
        Self {
            duplicate_b_target: std::env::var_os("TRELLIS_TEST_SEMANTIC_DUPLICATE_B_TARGET")
                .is_some(),
            duplicate_a_reference: std::env::var_os("TRELLIS_TEST_SEMANTIC_DUPLICATE_A_REFERENCE")
                .is_some(),
            missing_a_reference: std::env::var_os("TRELLIS_TEST_SEMANTIC_MISSING_A_REFERENCE")
                .is_some(),
            renamed_b_target: std::env::var_os("TRELLIS_TEST_SEMANTIC_RENAMED_B_TARGET").is_some(),
        }
    }

    /// Edits `declarations` and `references`, which may be the whole artifact's or one
    /// document's.
    pub fn apply(
        &self,
        declarations: &mut Vec<native::Declaration>,
        references: &mut Vec<native::Reference>,
    ) {
        if self.duplicate_b_target
            && let Some(declaration) = declarations
                .iter()
                .find(|d| d.document.path == "src/B.js" && d.name.as_deref() == Some("target"))
                .cloned()
        {
            declarations.push(declaration);
        }
        if self.duplicate_a_reference
            && let Some(reference) = references
                .iter()
                .find(|r| r.document.path == "src/A.js" && r.spelling == "target")
                .cloned()
        {
            let mut competing = reference;
            competing.id = format!("{}-candidate-only", competing.id);
            references.push(competing);
        }
        if self.renamed_b_target
            && let Some(declaration) = declarations
                .iter_mut()
                .find(|d| d.document.path == "src/B.js" && d.name.as_deref() == Some("target"))
        {
            // Another canonical syntax ID: flip the last hex digit.
            let last = declaration.syntax_id.pop().unwrap_or('0');
            declaration
                .syntax_id
                .push(if last == '0' { '1' } else { '0' });
        }
        if self.missing_a_reference {
            references.retain(|r| !(r.document.path == "src/A.js" && r.spelling == "target"));
        }
    }
}

/// The per-encoding coordinate maps of one document's occurrence claims.
fn document_coordinates<'c>(
    claims: impl IntoIterator<Item = &'c CapturedClaim>,
    source: &str,
    admitted_encoding: u8,
) -> Result<BTreeMap<u8, CoordinateMap>> {
    let mut positions: BTreeMap<u8, Vec<Position>> = BTreeMap::new();
    for claim in claims {
        if claim.envelope.record_kind != "occurrence" {
            continue;
        }
        for enclosing in [false, true] {
            if enclosing && claim.envelope.raw["enclosingRange"].is_null() {
                continue;
            }
            let raw = &claim.envelope.raw[if enclosing { "enclosingRange" } else { "range" }];
            let (encoding, endpoints) =
                range_endpoints(raw, claim.envelope.position_encoding, admitted_encoding)?;
            positions
                .entry(encoding)
                .or_default()
                .extend([endpoints.0, endpoints.1]);
        }
    }
    let mut maps = BTreeMap::new();
    for (encoding, mut positions) in positions {
        maps.insert(
            encoding,
            crate::scip_raw::source_byte_offsets(
                source,
                encoding,
                &mut positions,
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )?,
        );
    }
    Ok(maps)
}

pub(crate) fn empty_publication(producer: &semantic::Producer) -> semantic::SemanticPublication {
    semantic::SemanticPublication {
        producers: vec![producer.clone()],
        coverage: Vec::new(),
        provenance: Vec::new(),
        symbols: Vec::new(),
        declaration_bindings: Vec::new(),
        references: Vec::new(),
        call_bindings: Vec::new(),
        type_relationships: Vec::new(),
    }
}

struct ProofIdentityIndex<'a> {
    by_proof: BTreeMap<String, &'a crate::scip_raw::RawProofIdentity>,
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
        Ok(Self { by_proof })
    }
}

impl ProofIdentities for ProofIdentityIndex<'_> {
    fn proof(&self, proof_id: &str) -> Result<Option<ProofRef>> {
        Ok(self.by_proof.get(proof_id).map(|claim| ProofRef {
            language: claim.language.clone(),
            path: claim.path.clone(),
            record_kind: claim.record_kind.clone(),
            fact_digest: claim.fact_digest.clone(),
            artifact_hash: claim.artifact_hash.clone(),
        }))
    }

    fn check_proof(&self, proof_id: &str, kind: &str, digest: &str) -> Result<()> {
        // Proof IDs are unique (checked in `new`), so the index answers the inventory's lookup.
        match self.by_proof.get(proof_id) {
            Some(claim) if claim.record_kind == kind && claim.fact_digest == digest => Ok(()),
            _ => Err(crate::scip_raw::ScipCaptureError::MissingProof.into()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_rejoin<F: NativeFacts, P: ProofIdentities, Q: ProofIdentities, B: BasisFor>(
    inventory: &crate::scip_raw::ProofInventory,
    captured_native: &native::Artifact,
    destination_native: &native::Artifact,
    producer: semantic::Producer,
    basis: &B,
    r1: Option<&F>,
    r2: &F,
    target_projection: Option<
        &BTreeMap<(native::DocumentKey, String), semantic::TargetDeclaration>,
    >,
    raw_index: &P,
    filtered_index: &Q,
    filtered: &crate::scip_raw::ProofInventory,
    exclusions: &BTreeMap<(&str, &str), &str>,
    captured: JoinedFacts,
    destination: JoinedFacts,
    requested: BTreeSet<native::DocumentKey>,
    failed: BTreeSet<native::DocumentKey>,
    unsupported_claims: BTreeSet<String>,
    unsupported_documents: BTreeMap<native::DocumentKey, [u64; 4]>,
    sink: &mut impl PublicationSink,
    scope: Option<&WitnessScope>,
) -> Result<serde_json::Value> {
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
    let internal_symbols =
        scope.map_or(&captured_internal_symbols, |scope| &scope.internal_symbols);
    let prior_declarations = scope.map_or(&prior_declarations, |scope| &scope.declarations);
    let retarget = |target: &mut semantic::Target| -> bool {
        match target {
            semantic::Target::External { symbol } if internal_symbols.contains(&symbol.symbol) => {
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
                    if candidates.len() != 1 || r2.source_bytes(document).is_none() {
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
    for raw_id in &captured.fused_proofs {
        provenance_sources.insert((raw_id.clone(), semantic::EvidenceKind::SemanticReference));
    }
    let mut symbol_rows = Vec::new();
    for mut symbol in facts.symbols {
        let raw = filtered_index.proof(&symbol.provenance_id)?;
        if let Some(claim) = raw {
            let doc = native::DocumentKey {
                source_set_id: destination_native.revision.source_set_id.clone(),
                language: claim.language,
                path: claim.path,
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
        } else if let Some(claim) = filtered_index.proof(&symbol.provenance_id)? {
            omitted.insert(Some(native::DocumentKey {
                source_set_id: destination_native.revision.source_set_id.clone(),
                language: claim.language,
                path: claim.path,
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
    // The captured join's classification of each call and the target it was made against.
    let captured_dispatch: BTreeMap<_, _> = captured
        .call_bindings
        .iter()
        .map(|binding| {
            (
                binding.provenance_id.as_str(),
                (binding.dispatch, binding.declared_target.as_ref()),
            )
        })
        .collect();
    let mut calls = Vec::new();
    // Each destination call's join-time dispatch and the target it was classified against.
    let mut joined_dispatch = BTreeMap::new();
    for mut call in facts.call_bindings {
        let Some((resolution, target, candidates)) = old_targets.get(&call.provenance_id) else {
            continue;
        };
        call.resolution = *resolution;
        joined_dispatch.insert(
            call.provenance_id.clone(),
            (
                call.dispatch,
                std::mem::replace(&mut call.declared_target, target.clone()),
            ),
        );
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
    let target_is_method = |syntax_id: &str, document: &native::DocumentKey| {
        Ok(match target_projection {
            Some(projection) => projection
                .get(&(document.clone(), syntax_id.to_owned()))
                .is_some_and(|declaration| declaration.kind == "method"),
            None => r2
                .declarations_by_id(syntax_id)
                .iter()
                .any(|d| d.document == *document && d.kind == "method"),
        })
    };
    let mut rejoin_failure = None;
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
        let destination = joined_dispatch
            .get(&call.provenance_id)
            .map(|(dispatch, target)| (*dispatch, target.as_ref()));
        let original = captured_dispatch.get(call.provenance_id.as_str()).copied();
        match rejoined_dispatch(destination, original, call, target_is_method) {
            Ok(dispatch) => call.dispatch = dispatch,
            Err(error) => rejoin_failure = Some(error),
        }
        true
    });
    if let Some(error) = rejoin_failure {
        return Err(error);
    }
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
        } else if let Some(claim) = filtered_index.proof(&relationship.provenance_id)? {
            omitted.insert(Some(native::DocumentKey {
                source_set_id: destination_native.revision.source_set_id.clone(),
                language: claim.language,
                path: claim.path,
            }));
        }
    }
    let mut provenance = Vec::new();
    let destination_manifest_hash = scope.map_or_else(
        || manifest_hash(&destination_native.revision),
        |scope| scope.destination_manifest_hash.clone(),
    );
    let mut ids = BTreeMap::new();
    for (raw_id, kind) in provenance_sources {
        let claim = raw_index
            .proof(&raw_id)?
            .context("missing raw fact for joined provenance")?;
        raw_index.check_proof(&raw_id, &claim.record_kind, &claim.fact_digest)?;
        let doc = native::DocumentKey {
            source_set_id: destination_native.revision.source_set_id.clone(),
            language: claim.language.clone(),
            path: claim.path.clone(),
        };
        let captured_basis = &basis
            .basis_for(&doc)?
            .context("missing immutable captured basis")?;
        ensure!(
            captured_basis.revision_id == captured_native.revision.id
                && captured_basis.artifact_hash == claim.artifact_hash
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
        let id = rejoin_provenance_id(&raw_id, &destination_native.revision.id, kind);
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
        let capped = inventory
            .capped_documents
            .contains(&(doc.key.language.clone(), doc.key.path.clone()));
        let reasons = semantic::CoverageReasons::new(
            unsupported_documents
                .get(&doc.key)
                .copied()
                .unwrap_or_default(),
            capped,
            omitted.contains(&Some(doc.key.clone())),
            captured.diagnostics.contains_key(&doc.key),
        );
        let partial = excluded == Some(&"unsupported") || !reasons.is_empty();
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
            } else if reasons.native_gaps() > 0 {
                Some("unsupported_native_gap".into())
            } else if capped {
                Some(crate::scip_raw::OCCURRENCE_CAP_EXCEEDED.into())
            } else if partial {
                Some("unmeasurable_internal_target".into())
            } else {
                None
            },
            reasons: (state == semantic::CoverageState::Partial).then_some(reasons),
        });
    }
    let mut language_counts: BTreeMap<semantic::Language, [usize; 5]> = BTreeMap::new();
    let mut reasons: BTreeMap<semantic::Language, LanguageReasons> = BTreeMap::new();
    for row in &coverage {
        reasons.entry(row.language).or_default().add(row);
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
    }
    let languages: Vec<_> = language_counts
        .into_iter()
        .map(|(language, counts)| {
            serde_json::json!({
                "language":language,"requested":counts[0],"matched":counts[1],"skipped":counts[2],
                "ambiguous":counts[3],"unsupported":counts[4],
                "partialReasons":reasons.remove(&language).unwrap_or_default().json()
            })
        })
        .collect();
    #[allow(unused_mut)]
    let mut report = serde_json::json!({"producerId":producer.id,"sourceSetId":destination_native.revision.source_set_id,
        "capturedRevision":captured_native.revision.id,"destinationRevision":destination_native.revision.id,
        "languages":languages});
    #[cfg(feature = "test-support")]
    if let Ok(bytes) = std::env::var("TRELLIS_TEST_SEMANTIC_REPORT_PADDING")
        && let Ok(bytes) = bytes.parse::<usize>()
        && bytes <= 1024 * 1024
    {
        report["padding"] = serde_json::Value::String("x".repeat(bytes));
    }
    ensure!(
        serde_json::to_vec(&report)?.len() <= 1024 * 1024,
        "semantic import report exceeds response bound"
    );
    for row in coverage {
        sink.coverage(row)?;
    }
    for row in provenance {
        sink.provenance(row)?;
    }
    for row in symbol_rows {
        sink.symbol(row)?;
    }
    for row in bindings {
        sink.declaration_binding(row)?;
    }
    for row in references {
        sink.reference(row)?;
    }
    for row in calls {
        sink.call_binding(row)?;
    }
    for row in relationships {
        sink.type_relationship(row)?;
    }
    Ok(report)
}

/// What a one-document witnessed rejoin needs from the whole captured publication.
pub(crate) struct WitnessScope {
    /// Every symbol with a captured internal declaration: an external target naming one is an
    /// internal target that went absent, never a genuine external.
    pub internal_symbols: BTreeSet<String>,
    /// The captured measured declarations the rejoin has asked about, as `(document, syntax
    /// ID)`: a target's declaration must have been measured at the capture.
    pub declarations: BTreeSet<(native::DocumentKey, String)>,
    pub destination_manifest_hash: String,
}

/// One destination document of a witnessed rejoin.
pub(crate) struct WitnessedRejoin<'a> {
    pub document: &'a native::DocumentKey,
    /// The document's claims, exclusions and capped mark.
    pub inventory: &'a crate::scip_raw::ProofInventory,
    /// The captured and destination headers, each listing just this document (the captured one
    /// none when the document was not captured).
    pub captured_native: &'a native::Artifact,
    pub destination_native: &'a native::Artifact,
    pub producer: &'a semantic::Producer,
    pub basis: &'a BTreeMap<native::DocumentKey, semantic::SemanticBasis>,
    /// The document's original associations when it is reused; `None` when it changed.
    pub reused: Option<JoinedFacts>,
    /// The native gap counts its admission recorded.
    pub gaps: [u64; 4],
    pub current_targets: &'a BTreeMap<(native::DocumentKey, String), semantic::TargetDeclaration>,
    pub destination: &'a MeasuredFacts<'a>,
    pub scope: &'a WitnessScope,
}

/// One document's rejoin.
pub(crate) struct DocumentRejoin {
    pub publication: semantic::SemanticPublication,
    /// The reference proofs an absent target dropped, when the document holds fused proofs: a
    /// row fused from several claims keeps only its merged targets and roles, so dropping one
    /// claim of it cannot be told from here. The caller compares their spans.
    pub dropped: BTreeSet<String>,
    pub fused: BTreeSet<String>,
}

/// Rejoins one document from its admitted associations against the current targets. `None`
/// sends the rejoin to FULL: a target went absent under a fused call.
pub(crate) fn rejoin_document_with_witness(
    rejoin: WitnessedRejoin<'_>,
) -> Result<Option<DocumentRejoin>> {
    let WitnessedRejoin {
        document,
        inventory,
        captured_native,
        destination_native,
        producer,
        basis,
        reused,
        gaps,
        current_targets,
        destination,
        scope,
    } = rejoin;
    let is_reused = reused.is_some();
    let captured = reused.unwrap_or_default();
    let reusable = BTreeSet::from_iter(is_reused.then(|| document.clone()));
    let raw_index = ProofIdentityIndex::new(inventory)?;
    let mut filtered = inventory.clone();
    if !is_reused {
        filtered.claims.clear();
    }
    let filtered_index = ProofIdentityIndex::new(&filtered)?;
    let joined = reuse_captured_source_associations(
        &captured,
        &reusable,
        inventory,
        &destination_native.revision.source_set_id,
        &destination_native.revision.id,
    );
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
    if !captured_native.revision.documents.is_empty() {
        let reason = exclusions.get(&(document.language.as_str(), document.path.as_str()));
        if reason != Some(&"notRequested") {
            requested.insert(document.clone());
        }
        if reason == Some(&"failed") || (reason != Some(&"notRequested") && !is_reused) {
            failed.insert(document.clone());
        }
    }
    if !inventory.claims.is_empty() {
        requested.insert(document.clone());
    }
    let fused = captured.fused_proofs.clone();
    let fused_calls = captured.fused_calls.clone();
    let reference_proofs: BTreeSet<_> = captured.reference_claims.keys().cloned().collect();
    let unsupported_documents = (gaps != [0; 4])
        .then(|| (document.clone(), gaps))
        .into_iter()
        .collect();
    let mut publication = empty_publication(producer);
    finish_rejoin(
        inventory,
        captured_native,
        destination_native,
        producer.clone(),
        basis,
        None,
        destination,
        Some(current_targets),
        &raw_index,
        &filtered_index,
        &filtered,
        &exclusions,
        captured,
        joined,
        requested,
        failed,
        BTreeSet::new(),
        unsupported_documents,
        &mut publication,
        Some(scope),
    )?;
    let omitted = publication
        .coverage
        .iter()
        .any(|row| row.reasons.as_ref().is_some_and(|reasons| reasons.omitted));
    let mut dropped = BTreeSet::new();
    if omitted && !(fused.is_empty() && fused_calls.is_empty()) {
        let kept: BTreeSet<_> = publication
            .provenance
            .iter()
            .filter(|row| row.evidence_kind == semantic::EvidenceKind::SemanticReference)
            .filter_map(|row| row.derived_from.as_ref())
            .map(|derived| &derived.provenance_id)
            .collect();
        dropped.extend(
            reference_proofs
                .into_iter()
                .filter(|proof| !kept.contains(proof)),
        );
    }
    if dropped.iter().any(|proof| fused_calls.contains(proof)) {
        return Ok(None);
    }
    Ok(Some(DocumentRejoin {
        publication,
        dropped,
        fused,
    }))
}

pub(crate) fn manifest_hash(revision: &native::Revision) -> String {
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

#[cfg(test)]
mod walk_tests {
    use super::*;

    /// The former traversal, over `Node::parent` and `Node::prev_named_sibling`.
    fn reference_walk<'t>(
        root: tree_sitter::Node<'t>,
        visit: &mut impl FnMut(tree_sitter::Node<'t>) -> bool,
    ) {
        let mut node = root;
        loop {
            if visit(node)
                && let Some(child) = node
                    .named_child(u32::try_from(node.named_child_count().saturating_sub(1)).unwrap())
            {
                node = child;
                continue;
            }
            loop {
                if node == root {
                    return;
                }
                if let Some(sibling) = node.prev_named_sibling() {
                    node = sibling;
                    break;
                }
                node = node.parent().unwrap();
            }
        }
    }

    fn sources() -> Vec<(tree_sitter::Language, String)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = vec![];
        let mut pending = vec![root.join("tests/fixtures")];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        files.extend(
            ["src/semantic_import.rs", "src/scip_raw.rs"]
                .iter()
                .map(|path| root.join(path)),
        );
        files.sort();
        let mut sources = vec![];
        for path in files {
            let language: tree_sitter::Language = match path.extension().and_then(|e| e.to_str()) {
                Some("java") => tree_sitter_java::LANGUAGE.into(),
                Some("js") => tree_sitter_javascript::LANGUAGE.into(),
                Some("py") => tree_sitter_python::LANGUAGE.into(),
                Some("rs") => tree_sitter_rust::LANGUAGE.into(),
                _ => continue,
            };
            if let Ok(text) = std::fs::read_to_string(&path) {
                sources.push((language, text));
            }
        }
        // Malformed input parses with error and missing nodes.
        for (language, text) in [
            (
                tree_sitter_java::LANGUAGE.into(),
                "class A extends { void f( }",
            ),
            (tree_sitter_python::LANGUAGE.into(), "def f(:\n  return (\n"),
            (
                tree_sitter_javascript::LANGUAGE.into(),
                "class A extends B { f( } /*",
            ),
            (tree_sitter_rust::LANGUAGE.into(), "impl<T> X for { fn f( }"),
        ] {
            sources.push((language, text.to_owned()));
        }
        sources
    }

    #[test]
    fn cursor_walk_visits_exactly_the_former_traversal() {
        let sources = sources();
        assert!(sources.len() > 20, "{} sources", sources.len());
        for (language, source) in &sources {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(language).unwrap();
            let tree = parser.parse(source, None).unwrap();
            // Descend everywhere, and also prune by kind, as the role-site visitors do.
            for prune in [None, Some(0usize), Some(1)] {
                let descend = |node: tree_sitter::Node<'_>| match prune {
                    None => true,
                    Some(parity) => node.kind().len() % 2 != parity,
                };
                // The whole tree and a nested subtree, whose siblings must stay unvisited.
                let mut roots = vec![tree.root_node()];
                let mut cursor = tree.root_node().walk();
                if let Some(child) = tree.root_node().named_children(&mut cursor).nth(1) {
                    roots.push(child);
                }
                for root in roots {
                    let mut expected = vec![];
                    reference_walk(root, &mut |node| {
                        expected.push(node.id());
                        descend(node)
                    });
                    let mut actual = vec![];
                    walk_named_reverse(root, &mut |node| {
                        actual.push(node.id());
                        Ok(descend(node))
                    })
                    .unwrap();
                    assert_eq!(actual, expected);
                }
            }
        }
    }
}

#[cfg(test)]
mod language_reason_tests {
    use super::*;

    fn partial(path: String) -> semantic::Coverage {
        semantic::Coverage {
            producer_id: "p".into(),
            language: semantic::Language::Rust,
            source_set_id: "s".into(),
            document_path: path,
            revision_id: "r".into(),
            requested: true,
            selected: true,
            state: semantic::CoverageState::Partial,
            supported_roles: vec![],
            observed_roles: vec![],
            diagnostic: Some("unsupported_native_gap".into()),
            reasons: Some(semantic::CoverageReasons::new(
                [2, 1, 0, 0],
                false,
                false,
                false,
            )),
        }
    }

    /// The listed documents stop at the byte bound; every partial document is still counted.
    #[test]
    fn report_lists_partial_documents_up_to_its_byte_bound() {
        let mut reasons = LanguageReasons::default();
        for i in 0..2000 {
            reasons.add(&partial(format!("src/module_{i:04}.rs")));
        }
        let json = reasons.json();
        let listed = json["listed"].as_array().unwrap();
        assert!(listed.len() < 2000 && !listed.is_empty());
        assert!(json["listed"].to_string().len() <= REPORT_PARTIAL_DOCUMENT_BYTES);
        assert_eq!(json["unlisted"], 2000 - listed.len() as u64);
        assert_eq!(listed[0]["path"], "src/module_0000.rs");
        assert_eq!(json["documents"], 2000);
        assert_eq!(json["nativeGapsOnly"], 2000);
        assert_eq!(json["unsupportedOccurrences"]["macroInvocation"], 4000);
        assert_eq!(json["unsupportedOccurrences"]["attributeItem"], 2000);
    }

    /// The bound counts the array's brackets and commas: 316 entries whose separators and
    /// opening bracket total exactly the bound would serialize one byte over it with the
    /// closing bracket, so the last one is left unlisted.
    #[test]
    fn report_listing_bound_counts_array_framing() {
        let mut reasons = LanguageReasons::default();
        for i in 0..316 {
            let mut path = format!("src/module_{i:04}.rs");
            if i == 315 {
                path = format!("src/module_{i:04}{}.rs", "x".repeat(124));
            }
            reasons.add(&partial(path));
        }
        let json = reasons.json();
        assert!(json["listed"].to_string().len() <= REPORT_PARTIAL_DOCUMENT_BYTES);
        assert_eq!(json["listed"].as_array().unwrap().len(), 315);
        assert_eq!(json["unlisted"], 1);
    }
}
