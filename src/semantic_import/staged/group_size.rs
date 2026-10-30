//! Additional logical owners created by DocGroup::build, excluding moved native payload.
use super::*;
use sizes::{document, sum};
use std::mem::size_of;

/// The build's map model: the logical per-entry estimate, and never less than the one leaf
/// node a small map really allocates.
fn map_entries<K, V>(count: usize) -> Result<u64> {
    Ok(sizes::map_entries::<K, V>(count)?.max(super::heap::btree::<K, V>(count.min(11))))
}

fn slots<T>(count: usize) -> Result<u64> {
    (count as u64)
        .checked_mul(size_of::<T>() as u64)
        .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()).into())
}
fn pushed<T>(count: usize) -> Result<u64> {
    if count == 0 {
        return Ok(0);
    }
    slots::<T>(
        count
            .checked_mul(2)
            .map(|n| n.max(4))
            .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?,
    )
}
fn grouped<T>(count: usize, groups: usize) -> Result<u64> {
    slots::<T>(
        count
            .checked_mul(2)
            .and_then(|n| {
                count
                    .min(groups)
                    .checked_mul(4)
                    .and_then(|small| n.checked_add(small))
            })
            .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?,
    )
}
fn collected<T>(count: usize) -> Result<u64> {
    sum(&[pushed::<T>(count)?, slots::<T>(count)?])
}
fn copies(bytes: u64, count: usize) -> Result<u64> {
    bytes
        .checked_mul(count as u64)
        .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()).into())
}
fn strings(values: &[String]) -> Result<u64> {
    values
        .iter()
        .try_fold(slots::<String>(values.len())?, |total, s| {
            sum(&[total, s.len() as u64])
        })
}
pub(super) fn quoted(value: &str) -> Result<u64> {
    value.chars().try_fold(2, |total, ch| {
        sum(&[
            total,
            match ch {
                '"' | '\\' => 2,
                '\u{0}'..='\u{1f}' => 6,
                _ => ch.len_utf8() as u64,
            },
        ])
    })
}
fn digits(mut value: usize) -> u64 {
    let mut count = 1;
    while value >= 10 {
        value /= 10;
        count += 1;
    }
    count
}
fn bytes_capacity(length: u64) -> Result<u64> {
    Ok(sum(&[length, length])?.max(8))
}

/// Exact canonical output length and field-derived temporary Value/sort/format owners.
pub(in crate::semantic_import) fn key_bytes(key: &native::Key) -> Result<(u64, u64)> {
    let (signature_len, signature_dom) = if let Some(signature) = &key.signature {
        let array = signature.parameter_types.iter().try_fold(
            sum(&[2, signature.parameter_types.len().saturating_sub(1) as u64])?,
            |total, parameter| sum(&[total, quoted(parameter)?]),
        )?;
        let length = sum(&[
            2,
            2,
            3,
            quoted("parameterTypes")?,
            quoted("typeParameterCount")?,
            quoted("variadic")?,
            array,
            digits(signature.type_parameter_count),
            if signature.variadic { 4 } else { 5 },
        ])?;
        let payload = signature
            .parameter_types
            .iter()
            .try_fold(0, |total, p| sum(&[total, p.len() as u64]))?;
        let dom = sum(&[
            map_entries::<String, serde_json::Value>(3)?,
            "parameterTypestypeParameterCountvariadic".len() as u64,
            slots::<serde_json::Value>(signature.parameter_types.len())?,
            payload,
            digits(signature.type_parameter_count),
            slots::<&String>(4)?,
        ])?;
        (length, dom)
    } else {
        (4, 0)
    };
    let encoded = sum(&[
        2,
        3,
        4,
        quoted("kind")?,
        quoted("name")?,
        quoted("signature")?,
        quoted("ordinal")?,
        quoted(&key.kind)?,
        key.name.as_deref().map_or(Ok(4), quoted)?,
        signature_len,
        digits(key.ordinal),
    ])?;
    let temporary = sum(&[
        size_of::<serde_json::Value>() as u64,
        map_entries::<String, serde_json::Value>(4)?,
        "kindnamesignatureordinal".len() as u64,
        key.kind.len() as u64,
        key.name.as_ref().map_or(0, |s| s.len() as u64),
        digits(key.ordinal),
        signature_dom,
        slots::<&String>(4)?,
        // Number::to_string/control escape formatting, one at a time.
        size_of::<String>() as u64,
        bytes_capacity(
            digits(key.ordinal)
                .max(
                    key.signature
                        .as_ref()
                        .map_or(0, |s| digits(s.type_parameter_count)),
                )
                .max(6),
        )?,
    ])?;
    Ok((encoded, temporary))
}

/// Scalar decomposition uses ICU's pinned inline buffer; admit whole-input spill/sort
/// buffers before constructing its iterator. No raw-source expansion multiplier.
fn lookup_bytes(language: &str, spelling: &str, live: &Rc<LiveBudget>) -> Result<u64> {
    match language {
        "rust" | "python" => {
            use icu_normalizer::{ComposingNormalizerBorrowed, DecomposingNormalizerBorrowed};
            let input = if language == "rust" {
                spelling.strip_prefix("r#").unwrap_or(spelling)
            } else {
                spelling
            };
            let decomposer = if language == "rust" {
                DecomposingNormalizerBorrowed::new_nfd()
            } else {
                DecomposingNormalizerBorrowed::new_nfkd()
            };
            let mut decomposed = 0usize;
            for ch in input.chars() {
                for _ in decomposer.normalize_iter(std::iter::once(ch)) {
                    decomposed = decomposed
                        .checked_add(1)
                        .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
                }
            }
            // Pinned ICU 2.3.0 uses 17 inline CharacterAndClass(u32) slots.
            let scratch = if decomposed <= 17 {
                0
            } else {
                sum(&[
                    slots::<u32>(decomposed.checked_next_power_of_two().ok_or_else(|| {
                        ScipCaptureError::LimitExceeded("unit expansion".into())
                    })?)?,
                    slots::<u32>(decomposed)?,
                ])?
            };
            let _scratch = live.reserve(scratch)?;
            let normalizer = if language == "rust" {
                ComposingNormalizerBorrowed::new_nfc()
            } else {
                ComposingNormalizerBorrowed::new_nfkc()
            };
            let output = normalizer
                .normalize_iter(input.chars())
                .try_fold(0, |total, ch| sum(&[total, ch.len_utf8() as u64]))?;
            sum(&[
                scratch,
                size_of::<String>() as u64,
                bytes_capacity(output.max(input.len() as u64))?,
            ])
        }
        "java" => sum(&[size_of::<String>() as u64, spelling.len() as u64]),
        "javascript" => sum(&[
            size_of::<String>() as u64,
            if spelling.contains('\\') {
                // Identifier escape decoding only shrinks UTF-8 input; four hex digits
                // temporarily collect into their own String. C parser storage is separate.
                sum(&[
                    bytes_capacity(spelling.len() as u64)?,
                    size_of::<String>() as u64,
                    8,
                ])?
            } else {
                spelling.len() as u64
            },
        ]),
        _ => anyhow::bail!("unsupported native language"),
    }
}

pub(super) fn group_build(
    header: &NativeHeader,
    doc: &native::Document,
    rows: &crate::store::NativeDocumentRows,
    live: &Rc<LiveBudget>,
) -> Result<u64> {
    let key = document(&doc.key)?;
    let producer = &header.producer;
    let source_set = &header.source_set;
    let revision = &header.revision;
    let declarations = rows.declarations.len();
    let calls = rows.calls.len();
    let references = rows.references.len();
    let named = rows
        .declarations
        .iter()
        .filter(|d| d.name_range.is_some() && d.name.is_some())
        .count();
    let declaration_anchors = rows
        .declarations
        .iter()
        .filter(|d| d.name_range.is_some())
        .count();
    let callees = rows
        .calls
        .iter()
        .filter(|c| c.callee_range.is_some())
        .count();
    let anchors = declaration_anchors
        .checked_add(calls)
        .and_then(|n| n.checked_add(callees))
        .and_then(|n| n.checked_add(references))
        .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?;
    let families = rows.capability_report.families.len();
    // Header/document copies; source/native DTO payload moves and is charged by the caller.
    let mut total = sum(&[
        size_of::<DocGroup>() as u64,
        2 * size_of::<usize>() as u64,
        2 * size_of::<usize>() as u64, // Rc<str> control metadata (source overlap paid by caller).
        size_of::<native::Artifact>() as u64,
        size_of::<super::super::MeasuredFacts<'_>>() as u64,
        key,
        map_entries::<native::DocumentKey, Vec<u8>>(1)?,
        key,
        producer.id.len() as u64,
        producer.version.len() as u64,
        producer.executable_hash.len() as u64,
        producer.kind.len() as u64,
        producer.position_encoding.len() as u64,
        strings(&producer.languages)?,
        source_set.id.len() as u64,
        source_set.root_id.len() as u64,
        strings(&source_set.languages)?,
        strings(&source_set.dependencies)?,
        revision.id.len() as u64,
        revision.source_set_id.len() as u64,
        revision.toolchain_hash.len() as u64,
        revision.config_hash.len() as u64,
        revision.dependency_hash.len() as u64,
        slots::<native::Document>(1)?,
        key,
        doc.revision_id.len() as u64,
        doc.content_hash.len() as u64,
        slots::<native::Coverage>(1)?,
        slots::<native::CapabilityReport>(1)?,
        // MeasuredFacts source/doc maps and integrity digest.
        map_entries::<native::DocumentKey, &native::Document>(1)?,
        key,
        map_entries::<native::DocumentKey, &str>(1)?,
        key,
        64,
        map_entries::<String, Vec<&native::Declaration>>(declarations)?,
        grouped::<&native::Declaration>(declarations, declarations)?,
        map_entries::<native::DocumentKey, Vec<&native::Declaration>>(usize::from(
            declarations > 0,
        ))?,
        copies(key, usize::from(declarations > 0))?,
        pushed::<&native::Declaration>(declarations)?,
        map_entries::<(native::DocumentKey, Vec<u8>), Vec<&native::Declaration>>(declarations)?,
        grouped::<&native::Declaration>(declarations, declarations)?,
        copies(key, declarations)?,
        map_entries::<(native::DocumentKey, usize, usize, String), Vec<&native::Declaration>>(
            named,
        )?,
        grouped::<&native::Declaration>(named, named)?,
        copies(key, named)?,
        map_entries::<String, Vec<&native::Call>>(calls)?,
        grouped::<&native::Call>(calls, calls)?,
        map_entries::<String, Vec<&native::Reference>>(references)?,
        grouped::<&native::Reference>(references, references)?,
        // Both initial anchors and compatible candidates own ID sets; projection moves them.
        copies(
            sum(&[
                map_entries::<super::super::AnchorKey, BTreeSet<String>>(anchors)?,
                map_entries::<String, ()>(anchors)?,
                copies(key, anchors)?,
            ])?,
            2,
        )?,
        grouped::<String>(anchors, anchors)?,
        slots::<String>(anchors)?,
        map_entries::<(native::DocumentKey, semantic::JoinKind), Vec<(usize, usize)>>(
            anchors.min(4),
        )?,
        copies(key, anchors.min(4))?,
        grouped::<(usize, usize)>(anchors, 4)?,
        map_entries::<native::DocumentKey, ()>(1)?,
        key,
        map_entries::<(native::DocumentKey, semantic::JoinKind), &native::MeasurementSupport>(
            families,
        )?,
        copies(key, families)?,
        key,
        map_entries::<(usize, usize, semantic::JoinKind), BTreeSet<String>>(anchors)?,
        collected::<((usize, usize, semantic::JoinKind), BTreeSet<String>)>(anchors)?,
        map_entries::<semantic::JoinKind, native::MeasurementSupport>(families)?,
        collected::<(semantic::JoinKind, native::MeasurementSupport)>(families)?,
        // Retained DocGroup indexes, separate from temporary MeasuredFacts indexes.
        map_entries::<String, Vec<usize>>(declarations)?,
        grouped::<usize>(declarations, declarations)?,
        map_entries::<Vec<u8>, Vec<usize>>(declarations)?,
        grouped::<usize>(declarations, declarations)?,
        map_entries::<(usize, usize, String), ()>(named)?,
        map_entries::<String, Vec<usize>>(calls)?,
        grouped::<usize>(calls, calls)?,
        map_entries::<String, Vec<usize>>(references)?,
        grouped::<usize>(references, references)?,
    ])?;
    let mut canonical_scratch = 0;
    for declaration in &rows.declarations {
        let (encoded, scratch) = key_bytes(&declaration.key)?;
        // One canonical Vec may reallocate while its old buffer is still live.
        canonical_scratch = canonical_scratch.max(sum(&[scratch, bytes_capacity(encoded)?])?);
        total = sum(&[
            total,
            copies(declaration.syntax_id.len() as u64, 2)?,
            copies(bytes_capacity(encoded)?, 2)?,
        ])?;
        if declaration.name_range.is_some() {
            total = sum(&[total, copies(declaration.syntax_id.len() as u64, 2)?])?;
            if let Some(name) = &declaration.name {
                total = sum(&[total, copies(name.len() as u64, 2)?])?;
            }
        }
    }
    for call in &rows.calls {
        total = sum(&[
            total,
            copies(call.id.len() as u64, 2)?,
            copies(
                call.id.len() as u64,
                if call.callee_range.is_some() { 4 } else { 2 },
            )?,
        ])?;
    }
    let source = std::str::from_utf8(&rows.source).context("non-UTF8 native source")?;
    let mut lookup_scratch = 0;
    for reference in &rows.references {
        total = sum(&[total, copies(reference.id.len() as u64, 4)?])?;
        if let Some(spelling) = source.get(reference.range.start..reference.range.end) {
            lookup_scratch = lookup_scratch.max(lookup_bytes(&doc.key.language, spelling, live)?);
        }
    }
    for family in &rows.capability_report.families {
        total = sum(&[
            total,
            family.diagnostic.as_ref().map_or(0, |s| s.len() as u64),
            slots::<native::MeasurementGap>(family.gaps.len())?,
        ])?;
        for gap in &family.gaps {
            total = sum(&[total, gap.diagnostic.len() as u64])?;
        }
    }
    // The retained-reader header currently has no documents. Include actual base-clone
    // owners nevertheless, since struct-update evaluates that clone before replacement.
    total = sum(&[total, slots::<native::Document>(revision.documents.len())?])?;
    for document in &revision.documents {
        total = sum(&[
            total,
            sizes::document(&document.key)?,
            document.revision_id.len() as u64,
            document.content_hash.len() as u64,
        ])?;
    }
    sum(&[total, canonical_scratch, lookup_scratch])
}

mod retained {
    //! What a built `DocGroup` keeps, from the real capacities of its owners.
    use super::super::heap::{btree, buffer, optional, string, strings, value};
    use super::*;

    fn key(value: &native::DocumentKey) -> u64 {
        string(&value.source_set_id) + string(&value.language) + string(&value.path)
    }
    fn document(value: &native::Document) -> u64 {
        key(&value.key) + string(&value.revision_id) + string(&value.content_hash)
    }
    fn native_key(value: &native::Key) -> u64 {
        string(&value.kind)
            + optional(&value.name)
            + value
                .signature
                .as_ref()
                .map_or(0, |signature| strings(&signature.parameter_types))
    }
    fn declaration(value: &native::Declaration) -> u64 {
        let header = &value.header;
        let parameters = buffer(&header.parameters)
            + header
                .parameters
                .iter()
                .map(|p| optional(&p.name) + optional(&p.type_name))
                .sum::<u64>();
        string(&value.syntax_id)
            + key(&value.document)
            + string(&value.revision_id)
            + string(&value.kind)
            + optional(&value.name)
            + optional(&value.lookup_key)
            + buffer(&value.ancestors)
            + value.ancestors.iter().map(native_key).sum::<u64>()
            + native_key(&value.key)
            + string(&header.kind)
            + optional(&header.name)
            + strings(&header.modifiers)
            + strings(&header.type_parameters)
            + parameters
            + optional(&header.result_type)
            + strings(&header.bases)
            + string(&value.provenance_id)
    }
    fn call(value: &native::Call) -> u64 {
        string(&value.id)
            + string(&value.owner_syntax_id)
            + key(&value.document)
            + string(&value.revision_id)
            + optional(&value.spelling)
            + strings(&value.region_ids)
            + string(&value.provenance_id)
    }
    fn reference(value: &native::Reference) -> u64 {
        string(&value.id)
            + string(&value.owner_syntax_id)
            + key(&value.document)
            + string(&value.revision_id)
            + string(&value.spelling)
            + string(&value.lookup_key)
            + string(&value.provenance_id)
    }
    fn coverage(value: &native::Coverage) -> u64 {
        string(&value.producer_id)
            + string(&value.language)
            + string(&value.source_set_id)
            + string(&value.document_path)
            + string(&value.revision_id)
            + string(&value.state)
            + strings(&value.supported_roles)
            + strings(&value.observed_roles)
            + optional(&value.diagnostic)
    }
    fn support(value: &native::MeasurementSupport) -> u64 {
        optional(&value.diagnostic)
            + buffer(&value.gaps)
            + value
                .gaps
                .iter()
                .map(|g| string(&g.diagnostic))
                .sum::<u64>()
    }
    fn artifact(value: &native::Artifact) -> u64 {
        let producer = &value.producer;
        let source_set = &value.source_set;
        let revision = &value.revision;
        string(&producer.id)
            + string(&producer.version)
            + string(&producer.executable_hash)
            + string(&producer.kind)
            + strings(&producer.languages)
            + string(&producer.position_encoding)
            + string(&source_set.id)
            + string(&source_set.root_id)
            + strings(&source_set.languages)
            + strings(&source_set.dependencies)
            + string(&revision.id)
            + string(&revision.source_set_id)
            + buffer(&revision.documents)
            + revision.documents.iter().map(document).sum::<u64>()
            + string(&revision.toolchain_hash)
            + string(&revision.config_hash)
            + string(&revision.dependency_hash)
            + buffer(&value.coverage)
            + value.coverage.iter().map(coverage).sum::<u64>()
            + buffer(&value.provenance)
            + buffer(&value.declarations)
            + value.declarations.iter().map(declaration).sum::<u64>()
            + buffer(&value.calls)
            + value.calls.iter().map(call).sum::<u64>()
            + buffer(&value.control_regions)
            + buffer(&value.references)
            + value.references.iter().map(reference).sum::<u64>()
            + buffer(&value.capability_reports)
            + value
                .capability_reports
                .iter()
                .map(|report| {
                    key(&report.document)
                        + string(&report.revision_id)
                        + buffer(&report.families)
                        + report.families.iter().map(support).sum::<u64>()
                })
                .sum::<u64>()
            + buffer(&value.search_cards)
    }
    fn ids(value: &BTreeMap<String, Vec<usize>>) -> u64 {
        btree::<String, Vec<usize>>(value.len())
            + value
                .iter()
                .map(|(id, rows)| string(id) + buffer(rows))
                .sum::<u64>()
    }

    /// The canonical declaration-key index `DocGroup::declaration_keys` builds on first use,
    /// prepaid because it is built later without a charge of its own: the map, each key's
    /// canonical bytes at their growth capacity, and its index Vec (at most two slots per row
    /// plus four per key), with the largest one build's scratch.
    fn declaration_keys(declarations: &[native::Declaration]) -> Result<u64> {
        let mut total = btree::<Vec<u8>, Vec<usize>>(declarations.len());
        let mut scratch = 0;
        for declaration in declarations {
            let (encoded, temporary) = key_bytes(&declaration.key)?;
            total = sum(&[total, bytes_capacity(encoded)?])?;
            scratch = scratch.max(sum(&[temporary, bytes_capacity(encoded)?])?);
        }
        sum(&[
            total,
            slots::<usize>(declarations.len().saturating_mul(6))?,
            scratch,
        ])
    }

    /// The bytes a loaded document's native rows keep, or `None` when they hold search cards,
    /// which a join load never reads and this model does not count.
    pub(in crate::semantic_import::staged) fn rows(
        rows: &crate::store::NativeDocumentRows,
    ) -> Option<u64> {
        if !rows.search_cards.is_empty() {
            return None;
        }
        let provenance = &rows.provenance;
        let report = &rows.capability_report;
        let regions = rows
            .control_regions
            .iter()
            .map(|region| {
                string(&region.id)
                    + string(&region.owner_syntax_id)
                    + key(&region.document)
                    + string(&region.revision_id)
                    + string(&region.kind)
                    + optional(&region.parent_id)
                    + optional(&region.arm)
                    + string(&region.provenance_id)
            })
            .sum::<u64>();
        Some(
            buffer(&rows.source)
                + coverage(&rows.coverage)
                + string(&provenance.id)
                + string(&provenance.producer_id)
                + key(&provenance.document)
                + string(&provenance.revision_id)
                + string(&provenance.content_hash)
                + string(&provenance.evidence_kind)
                + provenance.basis.as_ref().map_or(0, value)
                + string(&provenance.freshness)
                + provenance.derived_from.as_ref().map_or(0, value)
                + buffer(&rows.declarations)
                + rows.declarations.iter().map(declaration).sum::<u64>()
                + buffer(&rows.calls)
                + rows.calls.iter().map(call).sum::<u64>()
                + buffer(&rows.control_regions)
                + regions
                + buffer(&rows.references)
                + rows.references.iter().map(reference).sum::<u64>()
                + key(&report.document)
                + string(&report.revision_id)
                + buffer(&report.families)
                + report.families.iter().map(support).sum::<u64>(),
        )
    }

    /// The bytes a built group keeps: its `Rc` allocation, document, source text, native rows
    /// and indexes, and the prepaid lazy key index.
    pub(in crate::semantic_import::staged) fn group(group: &DocGroup) -> Result<u64> {
        let candidates =
            btree::<(usize, usize, semantic::JoinKind), BTreeSet<String>>(group.candidates.len())
                + group
                    .candidates
                    .values()
                    .map(|ids| btree::<String, ()>(ids.len()) + ids.iter().map(string).sum::<u64>())
                    .sum::<u64>();
        let names = btree::<(usize, usize, String), ()>(group.declaration_names.len())
            + group
                .declaration_names
                .iter()
                .map(|(_, _, name)| string(name))
                .sum::<u64>();
        let support = btree::<semantic::JoinKind, native::MeasurementSupport>(group.support.len())
            + group.support.values().map(support).sum::<u64>();
        sum(&[
            // Rc strong and weak counts beside the group.
            (size_of::<DocGroup>() + 2 * size_of::<usize>()) as u64,
            document(&group.document),
            (group.text.len() + 2 * size_of::<usize>()) as u64,
            artifact(&group.native),
            ids(&group.declaration_ids),
            declaration_keys(&group.native.declarations)?,
            names,
            ids(&group.call_ids),
            ids(&group.reference_ids),
            candidates,
            support,
        ])
    }
}
pub(super) use retained::{group as retained_group, rows as retained_rows};

#[cfg(test)]
mod tests {
    use super::*;
    fn empty_group_input() -> (
        NativeHeader,
        native::Document,
        crate::store::NativeDocumentRows,
    ) {
        let key = native::DocumentKey {
            source_set_id: "source".into(),
            language: "java".into(),
            path: "A.java".into(),
        };
        let document = native::Document {
            key: key.clone(),
            revision_id: "revision".into(),
            content_hash: crate::native_ids::digest(b"", b""),
            byte_length: 0,
        };
        let header = NativeHeader {
            source_set: native::SourceSet {
                id: "source".into(),
                root_id: "root".into(),
                languages: vec!["java".into()],
                dependencies: vec![],
            },
            revision: native::Revision {
                id: "revision".into(),
                source_set_id: "source".into(),
                documents: vec![],
                toolchain_hash: "tool".into(),
                config_hash: "config".into(),
                dependency_hash: "dependencies".into(),
            },
            producer: native::Producer {
                id: "producer".into(),
                version: "1".into(),
                executable_hash: "executable".into(),
                kind: "native".into(),
                languages: vec!["java".into()],
                position_encoding: "utf8".into(),
            },
        };
        let rows = crate::store::NativeDocumentRows {
            source: vec![],
            declarations: vec![],
            calls: vec![],
            control_regions: vec![],
            references: vec![],
            search_cards: vec![],
            coverage: native::Coverage {
                producer_id: "producer".into(),
                language: "java".into(),
                source_set_id: "source".into(),
                document_path: "A.java".into(),
                revision_id: "revision".into(),
                requested: true,
                selected: true,
                state: "complete".into(),
                supported_roles: vec![],
                observed_roles: vec![],
                diagnostic: None,
            },
            provenance: native::Provenance {
                id: "provenance".into(),
                producer_id: "producer".into(),
                document: key.clone(),
                revision_id: "revision".into(),
                content_hash: document.content_hash.clone(),
                evidence_kind: "native".into(),
                basis: None,
                freshness: "current".into(),
                derived_from: None,
            },
            capability_report: native::CapabilityReport {
                document: key,
                revision_id: "revision".into(),
                families: vec![],
            },
        };
        (header, document, rows)
    }

    #[test]
    fn group_denial_precedes_build_and_last_pin_retains_reservation() {
        let (header, mut document, rows) = empty_group_input();
        document.content_hash = "bad hash".into();
        let measure = LiveBudget::new(BatchLimits::PRODUCTION.unit_expansion_bytes);
        let bytes = group_build(&header, &document, &rows, &measure).unwrap();
        let denied = LiveBudget::new(bytes - 1);
        let error = DocGroup::build(&header, document, rows, denied.reserve(0).unwrap(), &denied)
            .err()
            .unwrap();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name == "unit expansion")
        );
        drop(denied.reserve(bytes - 1).unwrap());
        let (header, mut document, rows) = empty_group_input();
        document.content_hash = "bad hash".into();
        let error = DocGroup::build(
            &header,
            document,
            rows,
            measure.reserve(0).unwrap(),
            &measure,
        )
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "native source hash mismatch");
        let (header, document, rows) = empty_group_input();
        let bytes = group_build(&header, &document, &rows, &measure).unwrap();
        let live = LiveBudget::new(bytes);
        let group = Rc::new(
            DocGroup::build(&header, document, rows, live.reserve(0).unwrap(), &live).unwrap(),
        );
        let pin = group.clone();
        drop(group);
        let source = pin.text.as_bytes();
        assert!(source.is_empty());
        // After the build the group holds its retained bytes, not the build's peak, until
        // its last pin drops.
        let kept = retained_group(&pin).unwrap();
        assert_eq!(pin.bytes, kept);
        assert!(kept < bytes);
        assert!(live.reserve(bytes - kept + 1).is_err());
        drop(live.reserve(bytes - kept).unwrap());
        drop(pin);
        drop(live.reserve(bytes).unwrap());
    }

    /// A document with `calls` call sites of one declared function, as native rows.
    fn call_group_input(
        calls: usize,
    ) -> (
        NativeHeader,
        native::Document,
        crate::store::NativeDocumentRows,
    ) {
        let (header, mut document, mut rows) = empty_group_input();
        let declaration = "function target() {}\n";
        let mut source = declaration.to_owned();
        let key = document.key.clone();
        let revision = document.revision_id.clone();
        rows.declarations.push(native::Declaration {
            role: native::DeclarationRole::Production,
            syntax_id: "syntax:v1:declaration-target".into(),
            document: key.clone(),
            revision_id: revision.clone(),
            kind: "function".into(),
            name: Some("target".into()),
            lookup_key: Some("target".into()),
            ancestors: vec![],
            key: native::Key {
                kind: "function".into(),
                name: Some("target".into()),
                signature: None,
                ordinal: 0,
            },
            range: native::Range {
                start: 0,
                end: declaration.len() - 1,
            },
            name_range: Some(native::Range { start: 9, end: 15 }),
            header: native::Header {
                kind: "function".into(),
                name: Some("target".into()),
                modifiers: vec![],
                type_parameters: vec![],
                parameters: vec![],
                result_type: None,
                bases: vec![],
            },
            provenance_id: "provenance".into(),
        });
        for n in 0..calls {
            let start = source.len();
            source.push_str("target();\n");
            let callee = native::Range {
                start,
                end: start + 6,
            };
            rows.calls.push(native::Call {
                id: format!("call:v1:{n:08}"),
                owner_syntax_id: "syntax:v1:declaration-target".into(),
                ordinal: n,
                document: key.clone(),
                revision_id: revision.clone(),
                range: native::Range {
                    start,
                    end: start + 8,
                },
                callee_range: Some(callee.clone()),
                spelling: Some("target".into()),
                region_ids: vec![],
                provenance_id: "provenance".into(),
            });
            rows.references.push(native::Reference {
                id: format!("reference:v1:{n:08}"),
                owner_syntax_id: "syntax:v1:declaration-target".into(),
                ordinal: n,
                document: key.clone(),
                revision_id: revision.clone(),
                range: callee,
                spelling: "target".into(),
                lookup_key: "target".into(),
                provenance_id: "provenance".into(),
            });
        }
        document.content_hash = crate::native_ids::digest(b"", source.as_bytes());
        document.byte_length = source.len();
        rows.source = source.into_bytes();
        (header, document, rows)
    }

    /// The group's long-lived charge covers what the built group really keeps (requested
    /// bytes, counted by the test allocator) and stays within 1.6x of it; the build's peak is
    /// within its admitted load and build charge.
    #[test]
    fn retained_group_charge_tracks_real_storage() {
        for calls in [0, 1, 10, 1_000, 5_000] {
            let (header, document, rows) = call_group_input(calls);
            let measure = LiveBudget::new(BatchLimits::PRODUCTION.unit_expansion_bytes);
            let build = group_build(&header, &document, &rows, &measure).unwrap();
            let rows_bytes = rows_heap(&rows);
            // The loaded-rows model is the rows' storage exactly.
            let (copy, copied) = crate::test_alloc::measure(|| crate::store::NativeDocumentRows {
                source: rows.source.clone(),
                coverage: rows.coverage.clone(),
                provenance: rows.provenance.clone(),
                declarations: rows.declarations.clone(),
                calls: rows.calls.clone(),
                control_regions: rows.control_regions.clone(),
                references: rows.references.clone(),
                capability_report: rows.capability_report.clone(),
                search_cards: vec![],
            });
            assert_eq!(retained_rows(&copy), Some(copied), "{calls}");
            let live = LiveBudget::new(BatchLimits::PRODUCTION.unit_expansion_bytes);
            let (group, kept, peak) = crate::test_alloc::measure_peak(|| {
                Rc::new(
                    DocGroup::build(&header, document, rows, live.reserve(0).unwrap(), &live)
                        .unwrap(),
                )
            });
            // The rows were allocated before the build and move into the group.
            let real = kept + rows_bytes;
            assert!(
                group.bytes >= real,
                "{calls}: charge {} < real {real}",
                group.bytes
            );
            assert!(
                group.bytes * 5 <= real * 8,
                "{calls}: charge {} vs real {real}",
                group.bytes
            );
            assert!(
                peak <= build,
                "{calls}: build peak {peak} > admitted {build}"
            );
        }
    }

    /// The heap storage of rows built by `call_group_input`, which the build moves.
    fn rows_heap(rows: &crate::store::NativeDocumentRows) -> u64 {
        let strings = |values: &[&String]| values.iter().map(|s| s.capacity() as u64).sum::<u64>();
        let key = |k: &native::DocumentKey| strings(&[&k.source_set_id, &k.language, &k.path]);
        let mut total = rows.source.capacity() as u64
            + (rows.declarations.capacity() * size_of::<native::Declaration>()) as u64
            + (rows.calls.capacity() * size_of::<native::Call>()) as u64
            + (rows.references.capacity() * size_of::<native::Reference>()) as u64;
        for d in &rows.declarations {
            total += strings(&[&d.syntax_id, &d.revision_id, &d.kind, &d.provenance_id])
                + key(&d.document)
                + [&d.name, &d.lookup_key, &d.key.name, &d.header.name]
                    .iter()
                    .map(|s| s.as_ref().map_or(0, |s| s.capacity() as u64))
                    .sum::<u64>()
                + strings(&[&d.key.kind, &d.header.kind]);
        }
        for c in &rows.calls {
            total += strings(&[&c.id, &c.owner_syntax_id, &c.revision_id, &c.provenance_id])
                + key(&c.document)
                + c.spelling.as_ref().map_or(0, |s| s.capacity() as u64);
        }
        for r in &rows.references {
            total += strings(&[&r.id, &r.owner_syntax_id, &r.revision_id, &r.spelling])
                + strings(&[&r.lookup_key, &r.provenance_id])
                + key(&r.document);
        }
        let c = &rows.coverage;
        total += strings(&[
            &c.producer_id,
            &c.language,
            &c.source_set_id,
            &c.document_path,
        ]) + strings(&[&c.revision_id, &c.state]);
        total += key(&rows.capability_report.document)
            + rows.capability_report.revision_id.capacity() as u64;
        total
    }

    #[test]
    fn normalized_lookup_preflight_covers_output_and_denies_spill_before_iterator() {
        for (language, input) in [
            ("rust", "r#e\u{301}"),
            ("python", "\u{fdfa}"),
            ("python", "é"),
            ("java", "name"),
            ("javascript", r"na\u006de"),
        ] {
            let live = LiveBudget::new(BatchLimits::PRODUCTION.unit_expansion_bytes);
            let bytes = lookup_bytes(language, input, &live).unwrap();
            let _guard = live.reserve(bytes).unwrap();
            let actual = native::lookup(language, input).unwrap();
            assert!(bytes >= size_of::<String>() as u64 + actual.len() as u64);
        }
        let spelling = format!("a{}", "\u{301}".repeat(20));
        let denied = LiveBudget::new(0);
        let error = lookup_bytes("python", &spelling, &denied).unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name == "unit expansion")
        );
        let live = LiveBudget::new(BatchLimits::PRODUCTION.unit_expansion_bytes);
        let bytes = lookup_bytes("python", &spelling, &live).unwrap();
        let guard = live.reserve(bytes).unwrap();
        assert!(bytes >= native::lookup("python", &spelling).unwrap().len() as u64);
        drop(guard);
        drop(live.reserve(bytes).unwrap());
    }

    #[test]
    fn key_prepass_matches_canonical_encoding_and_counts_scratch() {
        for name in [None, Some("é\n\\\"🙂".to_owned())] {
            for signature in [
                None,
                Some(native::Signature {
                    parameter_types: vec!["A\t".into(), "B".into()],
                    type_parameter_count: 12,
                    variadic: true,
                }),
            ] {
                let key = native::Key {
                    kind: "function".into(),
                    name: name.clone(),
                    signature,
                    ordinal: 123,
                };
                let (encoded, scratch) = key_bytes(&key).unwrap();
                assert_eq!(
                    encoded,
                    crate::native_ids::canonical(&serde_json::to_value(&key).unwrap()).len() as u64
                );
                assert!(scratch > 0);
            }
        }
    }
}
