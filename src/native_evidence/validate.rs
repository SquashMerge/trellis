//! The structural checks behind `Artifact::validate`: `validate_structure`, its sections
//! and the `StructureCheck` pass over calls, references, capability reports and regions.
use super::{
    MINT_CHUNK, declaration_lookup, in_macro_expansion, language_order, native_extraction_context,
    occurrence_input, safe_path, span, text, valid_hash,
};
use crate::model::SourceFile;
use crate::native_ids::{IdentityRegistry, canonical, digest};
use crate::native_records::{
    Artifact, Call, ControlRegion, Declaration, DocumentKey, LANGUAGES, MeasurementKind,
    NATIVE_VERSION, PRODUCER, Range, lookup,
};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

impl Artifact {
    /// The structural checks, one section at a time. The sections run in this
    /// order and each stops at its first failure, so the order decides which
    /// error an artifact with several faults reports.
    pub(super) fn validate_structure(&self, files: &[SourceFile]) -> Result<()> {
        self.check_header(files)?;
        let docs = self.index_documents(files)?;
        self.check_coverage(&docs)?;
        let proofs = self.index_proofs(&docs)?;
        // The pure part of every ID (input checks, canonical bytes, digest) is
        // minted on workers, a chunk at a time. Each row's checks still run
        // here in order, and the minted ID is admitted at the point where it
        // was computed before, so errors and their order are unchanged. A
        // one-document view is already running on a document worker, so it
        // mints inline instead of starting threads of its own.
        let workers = if files.len() == 1 {
            1
        } else {
            crate::parallel::default_workers()
        };
        let declarations = self.check_declarations(&docs, &proofs, workers)?;
        self.check_ordinal_sections()?;
        let check = StructureCheck {
            artifact: self,
            docs: &docs,
            proofs: &proofs,
            declarations: &declarations,
            contexts: self.extraction_contexts(&docs),
            workers,
        };
        let mut occurrence = IdentityRegistry::default();
        check.check_calls(&mut occurrence)?;
        check.check_references(&mut occurrence)?;
        check.check_capability_reports(&self.measured_candidates())?;
        let regions = check.check_control_regions(&mut occurrence)?;
        self.check_region_parents(&regions)?;
        self.check_region_links(&regions)
    }
    /// Section 1: producer, source set, revision and the per-document tuple counts.
    fn check_header(&self, files: &[SourceFile]) -> Result<()> {
        ensure!(
            self.producer.id == PRODUCER
                && self.producer.kind == "native"
                && self.producer.version == NATIVE_VERSION
                && self.producer.position_encoding == "utf8"
                && valid_hash(&self.producer.executable_hash),
            "invalid native producer"
        );
        ensure!(
            self.producer.languages == LANGUAGES.map(str::to_owned)
                && self.source_set.languages == self.producer.languages
                && self.source_set.dependencies.is_empty()
                && self.source_set.id == format!("source-set:v1:{}", self.source_set.root_id),
            "invalid native source set"
        );
        text(&self.source_set.root_id)?;
        ensure!(
            self.revision.source_set_id == self.source_set.id
                && self.revision.id.starts_with("revision:v2:")
                && valid_hash(&self.revision.id[12..])
                && valid_hash(&self.revision.toolchain_hash)
                && valid_hash(&self.revision.config_hash)
                && valid_hash(&self.revision.dependency_hash),
            "invalid native revision"
        );
        ensure!(
            self.revision.documents.len() == files.len()
                && self.coverage.len() == files.len()
                && self.provenance.len() == files.len()
                && self.capability_reports.len() == files.len(),
            "native document tuple cardinality"
        );
        Ok(())
    }
    /// Section 2: every revision document, sorted and matching its captured file.
    fn index_documents<'a>(&self, files: &'a [SourceFile]) -> Result<Docs<'a>> {
        let mut docs = BTreeMap::new();
        let captured_files: BTreeMap<_, _> = files
            .iter()
            .map(|f| ((f.language.as_str(), f.path.as_str()), f))
            .collect();
        ensure!(
            captured_files.len() == files.len(),
            "duplicate captured native path"
        );
        let mut last: Option<(usize, &str)> = None;
        for d in &self.revision.documents {
            safe_path(&d.key.path)?;
            let current = (language_order(&d.key.language)?, d.key.path.as_str());
            ensure!(
                last.is_none_or(|prev| prev < current),
                "unsorted or duplicate native documents"
            );
            last = Some(current);
            let file = captured_files
                .get(&(d.key.language.as_str(), d.key.path.as_str()))
                .context("native document not captured")?;
            ensure!(
                d.key.source_set_id == self.source_set.id
                    && d.revision_id == self.revision.id
                    && d.byte_length == file.text.len()
                    && d.content_hash == file.hash
                    && valid_hash(&d.content_hash),
                "native document bytes/hash mismatch"
            );
            docs.insert(d.key.clone(), *file);
        }
        Ok(docs)
    }
    /// Section 3: one coverage row per document.
    fn check_coverage(&self, docs: &Docs<'_>) -> Result<()> {
        let mut covers = BTreeSet::new();
        for c in &self.coverage {
            let key = DocumentKey {
                source_set_id: c.source_set_id.clone(),
                language: c.language.clone(),
                path: c.document_path.clone(),
            };
            ensure!(
                docs.contains_key(&key)
                    && covers.insert(key)
                    && c.producer_id == PRODUCER
                    && c.revision_id == self.revision.id
                    && c.requested
                    && c.selected
                    && matches!(c.state.as_str(), "complete" | "partial" | "failed")
                    && c.supported_roles == ["definition", "call"]
                    && c.observed_roles
                        .iter()
                        .all(|role| c.supported_roles.contains(role))
                    && c.observed_roles.windows(2).all(|window| c
                        .supported_roles
                        .iter()
                        .position(|r| r == &window[0])
                        < c.supported_roles.iter().position(|r| r == &window[1])),
                "invalid native coverage"
            );
            ensure!(
                (c.state == "complete") == c.diagnostic.is_none(),
                "native diagnostic mismatch"
            );
            if let Some(d) = &c.diagnostic {
                text(d)?;
            }
        }
        Ok(())
    }
    /// Section 4: one provenance proof per document, with its digest.
    fn index_proofs(&self, docs: &Docs<'_>) -> Result<Proofs<'_>> {
        let mut proofs = BTreeMap::new();
        for proof in &self.provenance {
            let f = docs
                .get(&proof.document)
                .context("native proof dangling document")?;
            ensure!(
                proofs.insert(proof.id.as_str(), &proof.document).is_none()
                    && proof.producer_id == PRODUCER
                    && proof.revision_id == self.revision.id
                    && proof.content_hash == f.hash
                    && proof.evidence_kind == "measuredSyntax"
                    && proof.basis.is_none()
                    && proof.derived_from.is_none()
                    && proof.freshness == "fresh",
                "invalid native provenance"
            );
            let expected = format!(
                "native-proof:v1:{}",
                digest(
                    b"trellis.native-proof.v1\0",
                    &canonical(
                        &json!({"producerId":PRODUCER,"document":proof.document,"revisionId":self.revision.id})
                    )
                )
            );
            ensure!(proof.id == expected, "native proof digest mismatch");
        }
        Ok(proofs)
    }
    /// Section 5: every declaration row, then its stable ID.
    fn check_declarations(
        &self,
        docs: &Docs<'_>,
        proofs: &Proofs<'_>,
        workers: usize,
    ) -> Result<BTreeMap<String, &Declaration>> {
        let never = std::sync::atomic::AtomicBool::new(false);
        let stable_input = |d: &Declaration| {
            crate::native_ids::mint_stable(
                &json!({"sourceSet":self.source_set.id,"path":d.document.path,"language":d.document.language,"ancestors":d.ancestors,"declaration":d.key}),
            )
        };
        let mut declarations = BTreeMap::new();
        let mut identity = IdentityRegistry::default();
        for chunk in self.declarations.chunks(MINT_CHUNK) {
            let mut minted =
                crate::parallel::map_ordered(chunk, workers, &never, |_, d| stable_input(d))
                    .into_iter();
            for d in chunk {
                let file = docs
                    .get(&d.document)
                    .context("native declaration dangling document")?;
                ensure!(
                    d.revision_id == self.revision.id
                        && proofs.get(d.provenance_id.as_str()) == Some(&&d.document)
                        && matches!(
                            d.kind.as_str(),
                            "module"
                                | "namespace"
                                | "type"
                                | "implementation"
                                | "function"
                                | "method"
                                | "constructor"
                                | "field"
                                | "variable"
                                | "parameter"
                                | "typeParameter"
                                | "alias"
                                | "anonymousFunction"
                                | "variant"
                        )
                        && d.kind == d.key.kind
                        && d.name == d.key.name
                        && d.header.kind == d.kind
                        && d.header.name == d.name,
                    "native declaration mismatch"
                );
                ensure!(
                    d.name.is_some() == d.lookup_key.is_some()
                        && d.name.is_some() == d.name_range.is_some(),
                    "native name nullability"
                );
                span(
                    &file.text,
                    &d.range,
                    d.kind == "module" && file.text.is_empty(),
                )?;
                if let Some(r) = &d.name_range {
                    span(&file.text, r, false)?;
                    ensure!(
                        r.start >= d.range.start
                            && r.end <= d.range.end
                            && d.name.as_deref() == file.text.get(r.start..r.end),
                        "native name not source-measured"
                    );
                }
                if let Some(n) = &d.name {
                    ensure!(
                        d.lookup_key.as_deref()
                            == Some(declaration_lookup(&d.document.language, n)?.as_str()),
                        "native lookup normalization mismatch"
                    );
                }
                if d.document.language != "java"
                    || !matches!(d.kind.as_str(), "method" | "constructor")
                {
                    ensure!(d.key.signature.is_none(), "unexpected native signature");
                }
                if d.kind == "module" {
                    ensure!(
                        d.name.is_none() && d.ancestors.is_empty(),
                        "invalid native module"
                    );
                }
                let expected =
                    identity.admit(minted.next().flatten().unwrap_or_else(|| stable_input(d))?)?;
                ensure!(
                    d.syntax_id == expected
                        && declarations.insert(d.syntax_id.clone(), d).is_none(),
                    "native stable ID collision/duplicate"
                );
            }
        }
        Ok(declarations)
    }
    /// Sections 6 to 9: sibling, call, reference and control-region ordinals, in that order.
    fn check_ordinal_sections(&self) -> Result<()> {
        // Facts inside a document's measured macro expansions take the owner's later ordinals.
        let expansions: BTreeMap<&DocumentKey, &[Range]> = self
            .capability_reports
            .iter()
            .map(|report| (&report.document, report.macro_expansions.as_slice()))
            .collect();
        let late = |document: &DocumentKey, range: &Range| {
            expansions
                .get(document)
                .is_some_and(|expansions| in_macro_expansion(expansions, range))
        };
        check_ordinals(
            &self.declarations,
            |d| {
                canonical(
                    &json!({"document":d.document,"ancestors":d.ancestors,"kind":d.kind,"name":d.name,"signature":d.key.signature}),
                )
            },
            late,
            |d| (&d.document, &d.range, d.key.ordinal),
            "native sibling ordinal mismatch",
            "duplicate sibling range",
        )?;
        check_ordinals(
            &self.calls,
            |c| (c.document.clone(), c.owner_syntax_id.clone()),
            late,
            |c| (&c.document, &c.range, c.ordinal),
            "native call ordinal mismatch",
            "duplicate call range",
        )?;
        check_ordinals(
            &self.references,
            |r| (&r.document, r.owner_syntax_id.as_str()),
            late,
            |r| (&r.document, &r.range, r.ordinal),
            "native reference ordinal mismatch",
            "duplicate reference range",
        )?;
        check_ordinals(
            &self.control_regions,
            |r| (r.document.clone(), r.owner_syntax_id.clone()),
            late,
            |r| (&r.document, &r.range, r.ordinal),
            "native control ordinal mismatch",
            "duplicate control range",
        )
    }
    // Keep authentication results local to this producer/revision validation.
    // Failures are surfaced at the existing occurrence-mint boundary.
    fn extraction_contexts<'a>(
        &self,
        docs: &'a Docs<'_>,
    ) -> BTreeMap<&'a DocumentKey, Result<String>> {
        docs.keys()
            .map(|document| {
                (
                    document,
                    native_extraction_context(&self.producer, &document.language, &self.revision),
                )
            })
            .collect()
    }
    /// The ranges each capability family measured, per document, sorted for
    /// the gap check of section 12.
    fn measured_candidates(&self) -> BTreeMap<(DocumentKey, MeasurementKind), Vec<&Range>> {
        let mut candidates: BTreeMap<(DocumentKey, MeasurementKind), Vec<&Range>> = BTreeMap::new();
        for d in &self.declarations {
            if let Some(range) = &d.name_range {
                candidates
                    .entry((d.document.clone(), MeasurementKind::DeclarationName))
                    .or_default()
                    .push(range);
            }
        }
        for c in &self.calls {
            candidates
                .entry((c.document.clone(), MeasurementKind::Invocation))
                .or_default()
                .push(&c.range);
            if let Some(range) = &c.callee_range {
                candidates
                    .entry((c.document.clone(), MeasurementKind::Callee))
                    .or_default()
                    .push(range);
            }
        }
        for r in &self.references {
            candidates
                .entry((r.document.clone(), MeasurementKind::Reference))
                .or_default()
                .push(&r.range);
        }
        for ranges in candidates.values_mut() {
            ranges.sort_by_key(|range| (range.start, range.end));
        }
        candidates
    }
    /// Section 14: a control region's parent has the same owner and encloses it.
    fn check_region_parents(&self, regions: &BTreeMap<String, &ControlRegion>) -> Result<()> {
        for region in &self.control_regions {
            if let Some(parent) = &region.parent_id {
                let parent = regions
                    .get(parent)
                    .context("native control dangling parent")?;
                ensure!(
                    parent.owner_syntax_id == region.owner_syntax_id
                        && parent.document == region.document
                        && parent.range.start <= region.range.start
                        && region.range.end <= parent.range.end
                        && parent.id != region.id,
                    "invalid native control parent"
                );
            }
        }
        Ok(())
    }
    /// Section 15: a call's regions have the same owner and enclose it.
    fn check_region_links(&self, regions: &BTreeMap<String, &ControlRegion>) -> Result<()> {
        for c in &self.calls {
            let mut seen_regions = BTreeSet::new();
            for id in &c.region_ids {
                ensure!(seen_regions.insert(id), "duplicate native call region");
                let r = regions.get(id).context("native call dangling region")?;
                ensure!(
                    r.owner_syntax_id == c.owner_syntax_id
                        && r.document == c.document
                        && r.range.start <= c.range.start
                        && c.range.end <= r.range.end,
                    "native call region mismatch"
                );
            }
        }
        Ok(())
    }
}

type Docs<'a> = BTreeMap<DocumentKey, &'a SourceFile>;
type Proofs<'a> = BTreeMap<&'a str, &'a DocumentKey>;
/// One ordinal section of `validate_structure`. Groups `facts` by `group_of`
/// and visits the groups in key order. Each group is ordered by
/// `(late, start, end)` of `place`'s document and range; its facts must then
/// carry the ordinals 0, 1, 2, … and no two neighbours the same range. Per
/// fact the ordinal is checked before the range, and the first failure ends
/// the section.
fn check_ordinals<'a, T, K: Ord>(
    facts: &'a [T],
    group_of: impl Fn(&'a T) -> K,
    late: impl Fn(&DocumentKey, &Range) -> bool,
    place: impl Fn(&T) -> (&DocumentKey, &Range, usize),
    mismatch: &'static str,
    duplicate: &'static str,
) -> Result<()> {
    let mut groups: BTreeMap<K, Vec<&T>> = BTreeMap::new();
    for fact in facts {
        groups.entry(group_of(fact)).or_default().push(fact);
    }
    for group in groups.values_mut() {
        group.sort_by_key(|fact| {
            let (document, range, _) = place(fact);
            (late(document, range), range.start, range.end)
        });
        for (ordinal, fact) in group.iter().enumerate() {
            let (_, range, own) = place(fact);
            ensure!(own == ordinal, mismatch);
            if ordinal != 0 {
                ensure!(place(group[ordinal - 1]).1 != range, duplicate);
            }
        }
    }
    Ok(())
}
/// What the fact sections of `validate_structure` share: the artifact and the
/// indexes the earlier sections built from it.
struct StructureCheck<'a> {
    artifact: &'a Artifact,
    docs: &'a Docs<'a>,
    proofs: &'a Proofs<'a>,
    declarations: &'a BTreeMap<String, &'a Declaration>,
    contexts: BTreeMap<&'a DocumentKey, Result<String>>,
    workers: usize,
}
impl<'a> StructureCheck<'a> {
    fn occurrence_of(
        &self,
        document: &DocumentKey,
        owner: &str,
        kind: &str,
        ordinal: usize,
    ) -> Result<crate::native_ids::Minted> {
        let Self {
            artifact,
            docs,
            contexts,
            ..
        } = self;
        let file = docs
            .get(document)
            .context("native occurrence dangling document")?;
        let context = contexts
            .get(document)
            .context("native occurrence dangling context")?
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        crate::native_ids::mint_occurrence_ref(&occurrence_input(
            &file.hash,
            context,
            &artifact.producer,
            owner,
            kind,
            ordinal,
        ))
    }
    fn call_occurrence(&self, c: &Call) -> Result<crate::native_ids::Minted> {
        self.occurrence_of(&c.document, &c.owner_syntax_id, "call", c.ordinal)
    }
    fn region_occurrence(&self, r: &ControlRegion) -> Result<crate::native_ids::Minted> {
        self.occurrence_of(&r.document, &r.owner_syntax_id, "control", r.ordinal)
    }
    /// Section 10: every call row, then its occurrence ID.
    fn check_calls(&self, occurrence: &mut IdentityRegistry) -> Result<()> {
        let Self {
            artifact,
            docs,
            proofs,
            declarations,
            workers,
            ..
        } = *self;
        let never = std::sync::atomic::AtomicBool::new(false);
        let mut calls = BTreeSet::new();
        for chunk in artifact.calls.chunks(MINT_CHUNK) {
            let mut minted = crate::parallel::map_ordered(chunk, workers, &never, |_, c| {
                self.call_occurrence(c)
            })
            .into_iter();
            for c in chunk {
                let file = docs
                    .get(&c.document)
                    .context("native call dangling document")?;
                let owner = declarations
                    .get(&c.owner_syntax_id)
                    .context("native call dangling owner")?;
                ensure!(
                    owner.document == c.document
                        && c.revision_id == artifact.revision.id
                        && proofs.get(c.provenance_id.as_str()) == Some(&&c.document)
                        && calls.insert(c.id.clone()),
                    "native call ownership/duplicate"
                );
                span(&file.text, &c.range, false)?;
                if let Some(r) = &c.callee_range {
                    span(&file.text, r, false)?;
                    ensure!(
                        r.start >= c.range.start
                            && r.end <= c.range.end
                            && c.spelling.as_deref() == file.text.get(r.start..r.end),
                        "callee not measured in call"
                    );
                }
                if let Some(s) = &c.spelling {
                    text(s)?;
                }
                let minted = minted
                    .next()
                    .flatten()
                    .unwrap_or_else(|| self.call_occurrence(c))?;
                ensure!(
                    c.id == occurrence.admit(minted)?,
                    "native call occurrence mismatch"
                );
            }
        }
        Ok(())
    }
    /// Section 11: every reference row, then its occurrence ID.
    fn check_references(&self, occurrence: &mut IdentityRegistry) -> Result<()> {
        let Self {
            artifact,
            docs,
            proofs,
            declarations,
            ..
        } = *self;
        let mut references: BTreeSet<&str> = BTreeSet::new();
        for r in &artifact.references {
            let file = docs
                .get(&r.document)
                .context("native reference dangling document")?;
            let owner = declarations
                .get(&r.owner_syntax_id)
                .context("native reference dangling owner")?;
            ensure!(
                owner.document == r.document
                    && r.revision_id == artifact.revision.id
                    && proofs.get(r.provenance_id.as_str()) == Some(&&r.document)
                    && references.insert(r.id.as_str()),
                "native reference ownership/duplicate"
            );
            span(&file.text, &r.range, false)?;
            ensure!(
                file.text.get(r.range.start..r.range.end) == Some(r.spelling.as_str())
                    && r.lookup_key == lookup(&r.document.language, &r.spelling)?,
                "native reference not source-measured"
            );
            ensure!(
                r.id == occurrence.admit(self.occurrence_of(
                    &r.document,
                    &r.owner_syntax_id,
                    "reference",
                    r.ordinal
                )?)?,
                "native reference occurrence mismatch"
            );
        }
        Ok(())
    }
    /// Section 12: one capability report per document: macro expansions,
    /// families and their gaps.
    fn check_capability_reports(
        &self,
        candidates: &BTreeMap<(DocumentKey, MeasurementKind), Vec<&Range>>,
    ) -> Result<()> {
        let Self { artifact, docs, .. } = *self;
        let mut reports = BTreeSet::new();
        for report in &artifact.capability_reports {
            let file = docs
                .get(&report.document)
                .context("native capability dangling document")?;
            ensure!(
                reports.insert(report.document.clone())
                    && report.revision_id == artifact.revision.id,
                "native capability document/revision mismatch"
            );
            let mut previous_end = None;
            for range in &report.macro_expansions {
                span(&file.text, range, false)?;
                ensure!(
                    report.document.language == "rust"
                        && previous_end.is_none_or(|end| end <= range.start),
                    "invalid native macro expansions"
                );
                previous_end = Some(range.end);
            }
            let mut families = BTreeSet::new();
            for family in &report.families {
                ensure!(
                    families.insert(family.kind) && family.available && family.diagnostic.is_none(),
                    "invalid native capability convention"
                );
                let mut last = None;
                for gap in &family.gaps {
                    span(&file.text, &gap.range, false)?;
                    text(&gap.diagnostic)?;
                    let range_key = (gap.range.start, gap.range.end);
                    ensure!(
                        last.is_none_or(|previous| previous < range_key),
                        "duplicate or unsorted native capability gaps"
                    );
                    last = Some(range_key);
                    let ranges = candidates
                        .get(&(report.document.clone(), family.kind))
                        .map(Vec::as_slice)
                        .unwrap_or_default();
                    let first = ranges.partition_point(|range| range.start < gap.range.start);
                    let contradiction = ranges[first..]
                        .iter()
                        .take_while(|range| range.start < gap.range.end)
                        .any(|range| range.end <= gap.range.end);
                    ensure!(
                        !contradiction,
                        "native capability gap contradicts measured candidate"
                    );
                }
            }
            ensure!(
                families == MeasurementKind::ALL.into_iter().collect(),
                "native capability family cardinality"
            );
        }
        Ok(())
    }
    /// Section 13: every control-region row, then its occurrence ID.
    fn check_control_regions(
        &self,
        occurrence: &mut IdentityRegistry,
    ) -> Result<BTreeMap<String, &'a ControlRegion>> {
        let Self {
            artifact,
            docs,
            proofs,
            declarations,
            workers,
            ..
        } = *self;
        let never = std::sync::atomic::AtomicBool::new(false);
        let mut regions = BTreeMap::new();
        for chunk in artifact.control_regions.chunks(MINT_CHUNK) {
            let mut minted = crate::parallel::map_ordered(chunk, workers, &never, |_, r| {
                self.region_occurrence(r)
            })
            .into_iter();
            for region in chunk {
                let file = docs
                    .get(&region.document)
                    .context("native control dangling document")?;
                let owner = declarations
                    .get(&region.owner_syntax_id)
                    .context("native control dangling owner")?;
                ensure!(
                    owner.document == region.document
                        && region.revision_id == artifact.revision.id
                        && proofs.get(region.provenance_id.as_str()) == Some(&&region.document),
                    "native control ownership"
                );
                span(&file.text, &region.range, false)?;
                let minted = minted
                    .next()
                    .flatten()
                    .unwrap_or_else(|| self.region_occurrence(region))?;
                ensure!(
                    region.id == occurrence.admit(minted)?
                        && regions.insert(region.id.clone(), region).is_none(),
                    "native control occurrence mismatch"
                );
            }
        }
        Ok(regions)
    }
}
