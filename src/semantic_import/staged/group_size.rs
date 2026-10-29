//! Additional logical owners created by DocGroup::build, excluding moved native payload.
use super::*;
use sizes::{document, map_entries, sum};
use std::mem::size_of;

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
        assert!(live.reserve(1).is_err());
        drop(pin);
        drop(live.reserve(bytes).unwrap());
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
