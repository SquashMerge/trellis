//! The measured native facts of a join, their anchors, and the composed join.
#[cfg(test)]
use super::work::complexity_work;
#[cfg(feature = "test-support")]
use super::work::count_publication_work;
use crate::{native_evidence as native, native_ids, semantic_evidence as semantic};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

/// Per-document anchor candidates and capability families.
pub(crate) type AnchorParts = (
    BTreeMap<(usize, usize, semantic::JoinKind), BTreeSet<String>>,
    BTreeMap<semantic::JoinKind, native::MeasurementSupport>,
);

pub(super) type AnchorKey = (native::DocumentKey, usize, usize, semantic::JoinKind);
/// An `AnchorKey` whose document is its rank among the artifact's interned documents.
type RankedAnchor = (usize, usize, usize, semantic::JoinKind);
/// A symbol's join identity: its raw spelling and its scope document, which is the containing
/// document of a `local ` symbol, the crate root of a Cargo-target-scoped one (#394), and
/// `None` for a global one.
pub(super) type ScopedKey = (String, Option<native::DocumentKey>);

/// Import-local native indexes. Distinct native identities, not raw-claim counts,
/// determine exactness and ambiguity.
#[derive(Clone)]
pub struct MeasuredFacts<'a> {
    pub(super) native: &'a native::Artifact,
    pub(super) sources: BTreeMap<native::DocumentKey, &'a str>,
    pub(super) documents: BTreeMap<native::DocumentKey, &'a native::Document>,
    candidates: BTreeMap<AnchorKey, BTreeSet<String>>,
    pub(super) declarations: BTreeMap<String, Vec<&'a native::Declaration>>,
    pub(super) declaration_names:
        BTreeMap<(native::DocumentKey, usize, usize, String), Vec<&'a native::Declaration>>,
    pub(super) declaration_keys:
        BTreeMap<(native::DocumentKey, Vec<u8>), Vec<&'a native::Declaration>>,
    pub(super) by_document: BTreeMap<native::DocumentKey, Vec<&'a native::Declaration>>,
    pub(super) calls: BTreeMap<String, Vec<&'a native::Call>>,
    pub(super) references: BTreeMap<String, Vec<&'a native::Reference>>,
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
        // Bounded S counts its destination index where it builds one (#620); an in-memory
        // anchor table over a non-destination revision is original work.
        #[cfg(feature = "test-support")]
        count_publication_work(|work| {
            if work.destination_revision != native.revision.id {
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

    pub(super) fn declaration(&self, id: &str) -> Option<&'a native::Declaration> {
        self.declarations
            .get(id)
            .and_then(|rows| rows.first().copied())
    }

    pub(super) fn call(&self, id: &str) -> Option<&'a native::Call> {
        self.calls.get(id).and_then(|rows| rows.first().copied())
    }

    pub(super) fn reference(&self, id: &str) -> Option<&'a native::Reference> {
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
