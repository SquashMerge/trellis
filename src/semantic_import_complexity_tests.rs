//! Deterministic work checks on the production import/rejoin entrypoints.
use super::*;
use crate::{capture::Capture, indexer::IndexOptions};
use serde_json::json;
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
};

fn case(
    count: usize,
    multiline: bool,
    encoding: u8,
) -> (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
) {
    let root = tempfile::tempdir().unwrap();
    let physical = fs::canonicalize(root.path()).unwrap();
    let separator = if multiline { "\r\n" } else { " " };
    let source = format!(
        "function run(){{ /* 😀 */{separator}{}{separator}}}",
        format!("f();{separator}").repeat(count)
    );
    fs::write(physical.join("A.js"), &source).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(physical.clone()), &cancel, &|_| {}).unwrap();
    let identity =
        crate::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical).unwrap();
    let native = native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
    let document = native.revision.documents[0].key.clone();
    let sources = BTreeMap::from([(document.clone(), source.as_bytes().to_vec())]);
    let claims = source
        .match_indices("f()")
        .enumerate()
        .map(|(ordinal, (start, _))| {
            let prefix = &source[..start];
            let line = prefix.bytes().filter(|b| *b == b'\n').count();
            let line_prefix = prefix.rsplit('\n').next().unwrap();
            let column = match encoding {
                1 => line_prefix.len(),
                2 => line_prefix.encode_utf16().count(),
                3 => line_prefix.chars().count(),
                _ => unreachable!(),
            };
            let envelope = crate::scip_raw::CapturedScipFactV1 {
                format_version: 1,
                record_kind: "occurrence".into(),
                relative_path: document.path.clone(),
                position_encoding: encoding,
                raw: json!({"range":{"encoding":"legacy","values":[line,column,column+1]},
                "enclosingRange":{"encoding":"legacy","values":[line,column,column+3]},
                "symbol":"external pkg f.","symbolRoles":0,"syntaxKind":0}),
            };
            let canonical_bytes =
                native_ids::canonical(&json!({"formatVersion":1,"recordKind":"occurrence",
            "relativePath":envelope.relative_path,"positionEncoding":encoding,"raw":envelope.raw}));
            CapturedClaim {
                envelope,
                fact_digest: native_ids::digest(b"trellis.semantic-fact.v1\0", &canonical_bytes),
                captured_proof_id: native_ids::digest(
                    b"complexity-fixture",
                    ordinal.to_string().as_bytes(),
                ),
                canonical_bytes,
                language: document.language.clone(),
                artifact_hash: "0".repeat(64),
            }
        })
        .collect();
    let inventory = CapturedScipInventory {
        artifact_hashes: vec!["0".repeat(64)],
        decoder_version: crate::scip_raw::DECODER_VERSION.into(),
        source_manifest_hash: manifest_hash(&native.revision),
        completion: crate::scip_raw::Completion::Complete,
        exclusions: vec![],
        claims,
    };
    (root, native, sources, inventory)
}

fn reset_work() {
    COMPLEXITY_WORK.with(|work| *work.borrow_mut() = ComplexityWork::default());
    crate::scip_raw::take_position_work();
}

#[test]
fn import_and_rejoin_walk_coordinates_and_index_native_records_once_per_snapshot() {
    for multiline in [false, true] {
        for encoding in 1..=3 {
            for count in [32, 64] {
                let (_root, native, sources, inventory) = case(count, multiline, encoding);
                let bytes = sources.values().map(Vec::len).sum::<usize>();
                reset_work();
                let facts = join_inventory(&inventory, &native, &sources, encoding).unwrap();
                assert_eq!(facts.references.len(), count);
                assert_eq!(facts.call_bindings.len(), count);
                assert!(
                    facts
                        .references
                        .iter()
                        .all(|r| r.resolution == semantic::Resolution::External)
                );
                let work = COMPLEXITY_WORK.with(|work| *work.borrow());
                assert_eq!(work.coordinate_batches, 1);
                assert_eq!(work.raw_claims, count);
                assert_eq!(work.raw_lookups, count);
                assert_eq!(work.reference_map_lookups, 2 * count);
                assert_eq!(work.method_index_rows, native.declarations.len());
                assert_eq!(work.method_lookup_rows, 0);
                assert_eq!(work.java_type_parses, 0);
                assert!(work.native_candidates <= 5 * count + 8, "{work:?}");
                assert!(crate::scip_raw::take_position_work() <= 4 * bytes);

                let basis = native
                    .revision
                    .documents
                    .iter()
                    .map(|document| {
                        (
                            document.key.clone(),
                            semantic::SemanticBasis {
                                producer_id: "scip-typescript".into(),
                                producer_version: "0.0".into(),
                                producer_hash: "0".repeat(64),
                                artifact_hash: inventory.artifact_hashes[0].clone(),
                                language: semantic::Language::Javascript,
                                source_set_id: native.source_set.id.clone(),
                                revision_id: native.revision.id.clone(),
                                source_manifest_hash: inventory.source_manifest_hash.clone(),
                                toolchain_hash: native.revision.toolchain_hash.clone(),
                                config_hash: native.revision.config_hash.clone(),
                                dependency_hash: native.revision.dependency_hash.clone(),
                                lookup_dependencies: vec![],
                            },
                        )
                    })
                    .collect();
                let producer = semantic::Producer {
                    id: "scip-typescript".into(),
                    version: "0.0".into(),
                    executable_hash: "0".repeat(64),
                    kind: semantic::SemanticKind::Semantic,
                    languages: vec![semantic::Language::Javascript],
                    position_encoding: match encoding {
                        1 => semantic::PositionEncoding::Utf8,
                        2 => semantic::PositionEncoding::Utf16,
                        3 => semantic::PositionEncoding::UnicodeScalar,
                        _ => unreachable!(),
                    },
                };
                reset_work();
                let result = rejoin_inventory(
                    &inventory, &native, &sources, &native, &sources, producer, &basis, encoding,
                )
                .unwrap();
                assert_eq!(result.publication.references.len(), count);
                assert_eq!(result.publication.call_bindings.len(), count);
                let work = COMPLEXITY_WORK.with(|work| *work.borrow());
                assert_eq!(
                    work.coordinate_batches, 1,
                    "coordinates must survive both joins and eligibility"
                );
                assert_eq!(work.raw_claims, 2 * count);
                assert_eq!(work.reference_map_lookups, 4 * count);
                assert_eq!(work.method_index_rows, 2 * native.declarations.len());
                assert_eq!(work.method_lookup_rows, 0);
                assert_eq!(work.java_type_parses, 0);
                assert!(work.raw_lookups <= 4 * count);
                assert!(work.native_candidates <= 4 * (5 * count + 8), "{work:?}");
                assert!(crate::scip_raw::take_position_work() <= 4 * bytes);
            }
        }
    }
}

#[test]
fn indexed_raw_lookup_keeps_duplicate_proof_fail_closed() {
    let (_root, native, sources, mut inventory) = case(2, false, 2);
    inventory.claims.push(inventory.claims[0].clone());
    let error = join_inventory(&inventory, &native, &sources, 2).unwrap_err();
    assert_eq!(
        error.downcast_ref::<crate::scip_raw::ScipCaptureError>(),
        Some(&crate::scip_raw::ScipCaptureError::NonUniqueProof)
    );
}

#[test]
fn native_gap_index_visits_records_once_and_queries_by_binary_search() {
    for count in [32, 64] {
        let (_root, mut native, sources, _inventory) = case(count, false, 2);
        let document = native.revision.documents[0].key.clone();
        let source = std::str::from_utf8(&sources[&document]).unwrap();
        let gaps = source
            .match_indices(';')
            .map(|(start, _)| native::MeasurementGap {
                range: native::Range {
                    start,
                    end: start + 1,
                },
                diagnostic: "fixture_gap".into(),
            })
            .collect();
        native.capability_reports[0]
            .families
            .iter_mut()
            .find(|f| f.kind == native::MeasurementKind::Invocation)
            .unwrap()
            .gaps = gaps;
        let gap_count: usize = native
            .capability_reports
            .iter()
            .flat_map(|report| &report.families)
            .map(|family| family.gaps.len())
            .sum();
        reset_work();
        MeasuredFacts::new(&native, &sources).unwrap();
        let work = COMPLEXITY_WORK.with(|work| *work.borrow());
        assert!(work.native_gap_index_records > 0);
        assert!(
            work.native_gap_index_records <= 2 * work.native_candidates,
            "{work:?}"
        );
        assert_eq!(work.native_gap_queries, gap_count);
        let anchor_count_bound = work.native_gap_index_records / 2;
        let per_query_bound = (anchor_count_bound + 1).ilog2() as usize + 2;
        assert!(
            work.native_gap_query_records <= gap_count * per_query_bound,
            "{work:?}"
        );
    }
}

#[test]
fn nested_raw_range_contradiction_is_not_hidden_by_a_longer_first_range() {
    let (_root, mut native, sources, _inventory) = case(2, false, 2);
    let first = native
        .calls
        .iter()
        .min_by_key(|call| call.range.start)
        .unwrap()
        .clone();
    let end = native
        .calls
        .iter()
        .map(|call| call.range.end)
        .max()
        .unwrap();
    let mut outer = first.clone();
    outer.id.push_str("-outer");
    outer.range = native::Range {
        start: first.range.start - 1,
        end,
    };
    outer.callee_range = None;
    let gap = native::Range {
        start: outer.range.start,
        end: first.range.end,
    };
    native.calls.push(outer);
    MeasuredFacts::new(&native, &sources).unwrap();
    native.capability_reports[0]
        .families
        .iter_mut()
        .find(|f| f.kind == native::MeasurementKind::Invocation)
        .unwrap()
        .gaps
        .push(native::MeasurementGap {
            range: gap,
            diagnostic: "fixture_gap".into(),
        });
    assert!(MeasuredFacts::new(&native, &sources).is_err());
}

#[test]
fn raw_gap_contradiction_survives_spelling_filter_and_unavailable_stays_query_local() {
    let (_root, mut native, sources, _inventory) = case(2, false, 2);
    let reference = native
        .references
        .iter()
        .find(|r| r.spelling == "f")
        .unwrap()
        .clone();
    let kind = semantic::JoinKind::Reference;
    let absent = native::Range { start: 0, end: 1 };
    assert_eq!(
        MeasuredFacts::new(&native, &sources)
            .unwrap()
            .join(&reference.document, absent.clone(), kind)
            .unwrap()
            .status,
        semantic::JoinStatus::Unmatched
    );
    let family = native.capability_reports[0]
        .families
        .iter_mut()
        .find(|f| f.kind == native::MeasurementKind::Reference)
        .unwrap();
    family.available = false;
    family.diagnostic = Some("fixture_unavailable".into());
    let measured = MeasuredFacts::new(&native, &sources).unwrap();
    assert_eq!(
        measured
            .join(&reference.document, absent.clone(), kind)
            .unwrap()
            .status,
        semantic::JoinStatus::Unsupported
    );
    assert!(
        measured
            .join(&reference.document, reference.range.clone(), kind)
            .is_err()
    );
    for row in &mut native.references {
        if row.document == reference.document && row.range == reference.range {
            row.spelling = "incompatible_fixture_spelling".into();
        }
    }
    assert_eq!(
        MeasuredFacts::new(&native, &sources)
            .unwrap()
            .join(&reference.document, reference.range.clone(), kind)
            .unwrap()
            .status,
        semantic::JoinStatus::Unsupported
    );
    let family = native.capability_reports[0]
        .families
        .iter_mut()
        .find(|f| f.kind == native::MeasurementKind::Reference)
        .unwrap();
    family.gaps.push(native::MeasurementGap {
        range: absent.clone(),
        diagnostic: "fixture_gap".into(),
    });
    assert!(
        MeasuredFacts::new(&native, &sources)
            .unwrap()
            .join(&reference.document, absent, kind)
            .is_err()
    );
    let family = native.capability_reports[0]
        .families
        .iter_mut()
        .find(|f| f.kind == native::MeasurementKind::Reference)
        .unwrap();
    family.available = true;
    family.diagnostic = None;
    family.gaps = vec![native::MeasurementGap {
        range: reference.range,
        diagnostic: "fixture_gap".into(),
    }];
    assert!(MeasuredFacts::new(&native, &sources).is_err());
}

#[test]
fn java_local_method_in_other_document_does_not_make_override_endpoint_ambiguous() {
    let root = tempfile::tempdir().unwrap();
    let physical = fs::canonicalize(root.path()).unwrap();
    let a = "class Base { void f() {} } class Child extends Base { void f() {} }";
    let b = "class Other { void f() {} }";
    fs::write(physical.join("A.java"), a).unwrap();
    fs::write(physical.join("B.java"), b).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(physical.clone()), &cancel, &|_| {}).unwrap();
    let identity =
        crate::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical).unwrap();
    let native = native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
    let sources: BTreeMap<_, _> = capture
        .files
        .iter()
        .map(|file| {
            (
                native::DocumentKey {
                    source_set_id: native.revision.source_set_id.clone(),
                    language: file.language.clone(),
                    path: file.path.clone(),
                },
                file.text.as_bytes().to_vec(),
            )
        })
        .collect();
    let mut claims = Vec::new();
    let mut push_claim = |path: &str, kind: &str, raw: serde_json::Value| {
        let envelope = crate::scip_raw::CapturedScipFactV1 {
            format_version: 1,
            record_kind: kind.into(),
            relative_path: path.into(),
            position_encoding: 1,
            raw,
        };
        let canonical_bytes = native_ids::canonical(&json!({
            "formatVersion": 1, "recordKind": kind, "relativePath": path,
            "positionEncoding": 1, "raw": envelope.raw,
        }));
        claims.push(CapturedClaim {
            fact_digest: native_ids::digest(b"trellis.semantic-fact.v1\0", &canonical_bytes),
            captured_proof_id: native_ids::digest(
                b"java-local-method-test",
                claims.len().to_string().as_bytes(),
            ),
            canonical_bytes,
            envelope,
            language: "java".into(),
            artifact_hash: "0".repeat(64),
        });
    };
    let derived = "scip-java pkg Child#f().";
    for declaration in native.declarations.iter().filter(|d| d.kind == "method") {
        let range = declaration.name_range.as_ref().unwrap();
        let symbol =
            if declaration.document.path == "A.java" && range.start == a.rfind("f()").unwrap() {
                derived
            } else {
                "local 7"
            };
        push_claim(
            &declaration.document.path,
            "occurrence",
            json!({
                "range": {"encoding": "legacy", "values": [0, range.start, range.end]},
                "enclosingRange": null, "symbol": symbol, "symbolRoles": 1, "syntaxKind": 0,
            }),
        );
    }
    for (path, symbol) in [
        ("A.java", derived),
        ("A.java", "local 7"),
        ("B.java", "local 7"),
    ] {
        push_claim(
            path,
            "symbolInformation",
            json!({"symbol": symbol, "kind": 26}),
        );
    }
    push_claim(
        "A.java",
        "relationship",
        json!({
            "sourceSymbol": derived, "targetSymbol": "local 7",
        }),
    );
    let inventory = CapturedScipInventory {
        artifact_hashes: vec!["0".repeat(64)],
        decoder_version: crate::scip_raw::DECODER_VERSION.into(),
        source_manifest_hash: manifest_hash(&native.revision),
        completion: crate::scip_raw::Completion::Complete,
        exclusions: vec![],
        claims,
    };
    let facts = join_inventory(&inventory, &native, &sources, 1).unwrap();
    assert_eq!(facts.declaration_bindings.len(), 3);
    assert!(
        facts
            .declaration_bindings
            .iter()
            .all(|b| b.join.status == semantic::JoinStatus::Exact)
    );
    // A local ID has no owner descriptor, so it cannot establish an override. The
    // same ID in B.java must not create an ambiguous endpoint in A.java either.
    assert!(facts.type_relationships.is_empty());
    assert!(facts.diagnostics.values().all(BTreeSet::is_empty));
}

#[test]
fn java_relationship_indexes_visit_each_snapshot_once_and_preserve_method_multiplicity() {
    for count in [32, 64] {
        let root = tempfile::tempdir().unwrap();
        let physical = fs::canonicalize(root.path()).unwrap();
        for ordinal in 0..count {
            fs::write(
                physical.join(format!("A{ordinal}.java")),
                format!("class Base{ordinal} {{ void f(int x) {{}} }} class Child{ordinal} extends Base{ordinal} {{ void f(int x) {{}} }}"),
            ).unwrap();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let capture =
            Capture::admit(&IndexOptions::new(physical.clone()), &cancel, &|_| {}).unwrap();
        let identity =
            crate::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical)
                .unwrap();
        let mut native =
            native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
        let sources: BTreeMap<_, _> = capture
            .files
            .iter()
            .map(|file| {
                (
                    native::DocumentKey {
                        source_set_id: native.revision.source_set_id.clone(),
                        language: file.language.clone(),
                        path: file.path.clone(),
                    },
                    file.text.as_bytes().to_vec(),
                )
            })
            .collect();
        let mut claims = Vec::new();
        let mut push_claim = |path: &str, kind: &str, raw: serde_json::Value| {
            let envelope = crate::scip_raw::CapturedScipFactV1 {
                format_version: 1,
                record_kind: kind.into(),
                relative_path: path.into(),
                position_encoding: 1,
                raw,
            };
            let canonical_bytes = native_ids::canonical(&json!({
                "formatVersion": 1, "recordKind": kind, "relativePath": path,
                "positionEncoding": 1, "raw": envelope.raw,
            }));
            claims.push(CapturedClaim {
                fact_digest: native_ids::digest(b"trellis.semantic-fact.v1\0", &canonical_bytes),
                captured_proof_id: native_ids::digest(
                    b"java-method-complexity",
                    claims.len().to_string().as_bytes(),
                ),
                canonical_bytes,
                envelope,
                language: "java".into(),
                artifact_hash: "0".repeat(64),
            });
        };
        for declaration in native
            .declarations
            .iter()
            .filter(|d| matches!(d.kind.as_str(), "type" | "method"))
        {
            let range = declaration.name_range.as_ref().unwrap();
            let (symbol, kind) = if declaration.kind == "type" {
                (
                    format!("scip-java pkg {}#", declaration.name.as_deref().unwrap()),
                    7,
                )
            } else {
                (
                    format!(
                        "scip-java pkg {}#f(I).",
                        declaration
                            .ancestors
                            .last()
                            .unwrap()
                            .name
                            .as_deref()
                            .unwrap()
                    ),
                    26,
                )
            };
            push_claim(
                &declaration.document.path,
                "occurrence",
                json!({
                    "range": {"encoding": "legacy", "values": [0, range.start, range.end]},
                    "enclosingRange": null, "symbol": symbol, "symbolRoles": 1, "syntaxKind": 0,
                }),
            );
            push_claim(
                &declaration.document.path,
                "symbolInformation",
                json!({"symbol": symbol, "kind": kind}),
            );
        }
        for (document, bytes) in &sources {
            let ordinal = document
                .path
                .strip_prefix('A')
                .unwrap()
                .strip_suffix(".java")
                .unwrap();
            let text = std::str::from_utf8(bytes).unwrap();
            let base = format!("Base{ordinal}");
            let start = text.rfind(&base).unwrap();
            push_claim(
                &document.path,
                "occurrence",
                json!({
                    "range": {"encoding": "legacy", "values": [0, start, start + base.len()]},
                    "enclosingRange": null, "symbol": format!("scip-java pkg {base}#"), "symbolRoles": 8, "syntaxKind": 0,
                }),
            );
            // Distinct proofs for the same endpoint pair must each survive indexing.
            for _ in 0..2 {
                push_claim(
                    &document.path,
                    "relationship",
                    json!({
                        "sourceSymbol": format!("scip-java pkg Child{ordinal}#f(I)."),
                        "targetSymbol": format!("scip-java pkg Base{ordinal}#f(I)."),
                    }),
                );
            }
        }
        let inventory = CapturedScipInventory {
            artifact_hashes: vec!["0".repeat(64)],
            decoder_version: crate::scip_raw::DECODER_VERSION.into(),
            source_manifest_hash: manifest_hash(&native.revision),
            completion: crate::scip_raw::Completion::Complete,
            exclusions: vec![],
            claims,
        };
        reset_work();
        let facts = join_inventory(&inventory, &native, &sources, 1).unwrap();
        let work = COMPLEXITY_WORK.with(|work| *work.borrow());
        assert_eq!(work.method_index_rows, native.declarations.len());
        assert_eq!(work.method_lookup_rows, 4 * count);
        assert_eq!(work.java_type_parses, count);
        assert_eq!(facts.type_relationships.len(), 2 * count);
        assert!(
            facts
                .type_relationships
                .iter()
                .all(|r| r.kind == semantic::RelationshipKind::Overrides)
        );
        assert_eq!(
            facts
                .type_relationships
                .iter()
                .map(|r| &r.provenance_id)
                .collect::<BTreeSet<_>>()
                .len(),
            2 * count
        );
        assert!(facts.diagnostics.values().all(BTreeSet::is_empty));

        // A duplicate native method is still two candidates, never one indexed identity.
        let duplicate = native
            .declarations
            .iter()
            .find(|d| {
                d.document.path == "A0.java"
                    && d.kind == "method"
                    && d.ancestors.last().and_then(|key| key.name.as_deref()) == Some("Base0")
            })
            .unwrap()
            .clone();
        let ambiguous_document = duplicate.document.clone();
        native.declarations.push(duplicate);
        reset_work();
        let facts = join_inventory(&inventory, &native, &sources, 1).unwrap();
        let work = COMPLEXITY_WORK.with(|work| *work.borrow());
        assert_eq!(work.method_index_rows, native.declarations.len());
        assert_eq!(work.method_lookup_rows, 4 * count + 2);
        assert_eq!(work.java_type_parses, count);
        assert_eq!(facts.type_relationships.len(), 2 * count - 2);
        assert!(
            facts
                .diagnostics
                .get(&ambiguous_document)
                .is_some_and(|d| d.contains("unmeasurable_override_endpoint"))
        );
    }
}

#[test]
fn java_override_uses_global_method_owner_from_another_language() {
    let root = tempfile::tempdir().unwrap();
    let physical = fs::canonicalize(root.path()).unwrap();
    let child = "class Child extends Base { void f() {} }";
    let base = "class Base { f() {} }";
    fs::write(physical.join("A.java"), child).unwrap();
    fs::write(physical.join("B.js"), base).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(physical.clone()), &cancel, &|_| {}).unwrap();
    let identity =
        crate::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical).unwrap();
    let native = native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
    let sources: BTreeMap<_, _> = capture
        .files
        .iter()
        .map(|file| {
            (
                native::DocumentKey {
                    source_set_id: native.revision.source_set_id.clone(),
                    language: file.language.clone(),
                    path: file.path.clone(),
                },
                file.text.as_bytes().to_vec(),
            )
        })
        .collect();
    let mut claims = Vec::new();
    let mut push = |document: &native::DocumentKey, kind: &str, raw: serde_json::Value| {
        let envelope = crate::scip_raw::CapturedScipFactV1 {
            format_version: 1,
            record_kind: kind.into(),
            relative_path: document.path.clone(),
            position_encoding: 1,
            raw,
        };
        let canonical_bytes = native_ids::canonical(&json!({
            "formatVersion": 1, "recordKind": kind, "relativePath": document.path,
            "positionEncoding": 1, "raw": envelope.raw,
        }));
        claims.push(CapturedClaim {
            fact_digest: native_ids::digest(b"trellis.semantic-fact.v1\0", &canonical_bytes),
            captured_proof_id: native_ids::digest(b"foreign-method-owner", &canonical_bytes),
            canonical_bytes,
            envelope,
            language: document.language.clone(),
            artifact_hash: "0".repeat(64),
        });
    };
    for declaration in native
        .declarations
        .iter()
        .filter(|d| matches!(d.kind.as_str(), "type" | "method"))
    {
        let range = declaration.name_range.as_ref().unwrap();
        let owner = if declaration.document.path == "A.java" {
            "Child"
        } else {
            "Base"
        };
        let (symbol, kind) = if declaration.kind == "type" {
            (format!("scip-java pkg {owner}#"), 7)
        } else {
            (format!("scip-java pkg {owner}#f()."), 26)
        };
        push(
            &declaration.document,
            "occurrence",
            json!({
                "range": {"encoding": "legacy", "values": [0, range.start, range.end]},
                "enclosingRange": null, "symbol": symbol, "symbolRoles": 1, "syntaxKind": 0,
            }),
        );
        push(
            &declaration.document,
            "symbolInformation",
            json!({"symbol": symbol, "kind": kind}),
        );
    }
    let document = sources.keys().find(|d| d.path == "A.java").unwrap();
    let start = child.find("Base").unwrap();
    push(
        document,
        "occurrence",
        json!({
            "range": {"encoding": "legacy", "values": [0, start, start + 4]},
            "enclosingRange": null, "symbol": "scip-java pkg Base#", "symbolRoles": 8, "syntaxKind": 0,
        }),
    );
    push(
        document,
        "relationship",
        json!({
            "sourceSymbol": "scip-java pkg Child#f().", "targetSymbol": "scip-java pkg Base#f().",
        }),
    );
    let inventory = CapturedScipInventory {
        artifact_hashes: vec!["0".repeat(64)],
        decoder_version: crate::scip_raw::DECODER_VERSION.into(),
        source_manifest_hash: manifest_hash(&native.revision),
        completion: crate::scip_raw::Completion::Complete,
        exclusions: vec![],
        claims,
    };
    let facts = join_inventory(&inventory, &native, &sources, 1).unwrap();
    let overrides: Vec<_> = facts
        .type_relationships
        .iter()
        .filter(|r| r.kind == semantic::RelationshipKind::Overrides)
        .collect();
    assert_eq!(
        overrides.len(),
        1,
        "foreign-language method must retain its owner predicate"
    );
    assert!(
        matches!(&overrides[0].target, semantic::Target::Internal { document, .. } if document.path == "B.js" && document.language == "javascript")
    );
}

#[test]
fn direct_base_rows_preserve_order_and_admit_output_before_build() {
    let source = b"interface I extends First, pkg.Second<T> {} class A extends pkg.Base<T> implements X, Y {}";
    let mut bytes = 0;
    let rows = direct_java_types(source, &mut |amount| {
        bytes += amount;
        Ok(())
    })
    .unwrap();
    let names: Vec<_> = rows
        .iter()
        .map(|(_, range, _)| std::str::from_utf8(&source[range.start..range.end]).unwrap())
        .collect();
    assert_eq!(names, ["Base", "Y", "X", "Second", "First"]);
    assert_eq!(
        bytes,
        std::mem::size_of::<Vec<DirectBase>>() as u64
            + (rows.len() * std::mem::size_of::<DirectBase>()) as u64
    );
    let mut passes = 0;
    let denied = collect_direct_bases(
        &mut |_| {
            Err(crate::scip_raw::ScipCaptureError::LimitExceeded("unit expansion".into()).into())
        },
        |emit| {
            passes += 1;
            emit((
                native::Range { start: 0, end: 1 },
                native::Range { start: 2, end: 3 },
                semantic::RelationshipKind::Extends,
            ))
        },
    )
    .unwrap_err();
    assert!(
        matches!(denied.downcast_ref::<crate::scip_raw::ScipCaptureError>(),Some(crate::scip_raw::ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
    );
    assert_eq!(
        passes, 1,
        "denied capacity must not reach output-building pass"
    );
    let script = b"class A(pkg.Base, Other): pass";
    let rows = direct_script_types(
        script,
        tree_sitter_python::LANGUAGE.into(),
        "class_definition",
        "argument_list",
        &mut |_| Ok(()),
    )
    .unwrap();
    let names: Vec<_> = rows
        .iter()
        .map(|(_, range, _)| std::str::from_utf8(&script[range.start..range.end]).unwrap())
        .collect();
    assert_eq!(names, ["Base", "Other"]);
}

#[test]
fn reserved_canonical_key_matches_existing_encoding() {
    use super::join_store::JoinFacts;
    let (_root, native, sources, inventory) = case(2, false, 1);
    let raw = InventoryIndex::new(&inventory).unwrap();
    let coordinates = CoordinateMaps::new(&inventory, &native, &sources, 1).unwrap();
    let measured = MeasuredFacts::new(&native, &sources).unwrap();
    let store = MemoryJoinStore::new(&raw, &coordinates, &measured, &sources);
    store.operation().unwrap();
    for number in [0, usize::MAX] {
        for parameter_types in [vec![], vec!["é\n\\\"🙂".into()]] {
            let key = native::Key {
                kind: "function".into(),
                name: Some("name\t".into()),
                signature: Some(native::Signature {
                    parameter_types,
                    type_parameter_count: number,
                    variadic: false,
                }),
                ordinal: number,
            };
            let bytes = reserved_key_bytes(&store, &key, &mut ()).unwrap();
            assert_eq!(
                bytes,
                native_ids::canonical(&serde_json::to_value(&key).unwrap())
            );
        }
    }
}
