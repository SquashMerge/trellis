use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    capture::Capture,
    indexer::IndexOptions,
    native_evidence as native, native_ids,
    scip_admission::{
        CapturedScipInventory, ReferenceProfileAdmission, TrustedSelection,
        decode_reference_profile,
    },
    semantic_evidence as semantic,
    semantic_import::rejoin_inventory,
};

const WIRE: &str = "122e0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a090a0241232811320166";

fn fixture() -> (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
) {
    fixture_with_extra_occurrences(false, false)
}

fn fixture_with_extra_occurrences(
    unmatched: bool,
    reference_as_definition: bool,
) -> (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
) {
    use protobuf::Message;
    let root = tempfile::tempdir().unwrap();
    let physical = fs::canonicalize(root.path()).unwrap();
    fs::create_dir(physical.join("src")).unwrap();
    let text = "function f(){g();}\n";
    fs::write(physical.join("src/A.js"), text).unwrap();
    let mut bytes = hex::decode(WIRE).unwrap();
    if unmatched || reference_as_definition {
        let mut index = scip::types::Index::parse_from_bytes(&bytes).unwrap();
        if reference_as_definition {
            let mut occurrence = scip::types::Occurrence::new();
            occurrence.range = vec![0, 13, 14];
            occurrence.symbol = "pkg g.".into();
            occurrence.symbol_roles = 1;
            index.documents[0].occurrences.push(occurrence);
        }
        for roles in [0, 1].into_iter().filter(|_| unmatched) {
            let mut occurrence = scip::types::Occurrence::new();
            occurrence.range = vec![0, 0, 8];
            occurrence.symbol = format!("unmatched{roles}#");
            occurrence.symbol_roles = roles;
            index.documents[0].occurrences.push(occurrence);
        }
        bytes = index.write_to_bytes().unwrap();
    }
    fs::write(physical.join("artifact.scip"), &bytes).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(physical.clone());
    let first = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical).unwrap();
    let first_native =
        native::from_capture(&first, &physical, &identity.record_id, &cancel).unwrap();
    let (set, rev) = (first_native.source_set, first_native.revision);
    let file = &first.files[0];
    let doc = json!({"language":"javascript","relativePath":"src/A.js","contentHash":file.hash});
    let source_manifest_hash = native_ids::digest(
        b"",
        &native_ids::canonical(
            &json!([{"document":{"sourceSetId":set.id,"language":"javascript","path":"src/A.js"},"contentHash":file.hash}]),
        ),
    );
    let profile: serde_json::Value = serde_json::from_str(include_str!(
        "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
    ))
    .unwrap();
    let producer = profile["semanticProducers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "scip-typescript")
        .unwrap();
    let uri = format!("file://{}", physical.display());
    let manifest = json!({"formatVersion":1,"trustOrigin":"referenceProfile","completion":"complete","decoderVersion":trellis::scip_raw::DECODER_VERSION,"profileId":profile["profileId"],
        "producer":{"name":producer["name"],"version":producer["version"],"executableHash":producer["executableHash"],"positionEncoding":2},
        "sourceSet":{"id":set.id,"rootId":set.root_id,"languages":set.languages,"dependencies":set.dependencies},
        "sourceRevision":rev.id,"sourceManifestHash":source_manifest_hash,
        "components":{"toolchainHash":rev.toolchain_hash,"configHash":rev.config_hash,"dependencyHash":rev.dependency_hash},
        "metadata":{"protocolVersion":0,"projectRoot":uri,"textDocumentEncoding":1},
        "sourceDocuments":[doc],"documents":[doc],"exclusions":[],
        "artifacts":[{"kind":"scipBinary","path":"artifact.scip","hash":native_ids::digest(b"", &bytes),"language":"javascript","documents":["src/A.js"]}]});
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(physical.join("manifest.json"), &manifest_bytes).unwrap();
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let selection = TrustedSelection {
        expected_manifest_hash: native_ids::digest(b"", &manifest_bytes),
        profile_id: "mac-mini-m5-pro-v1".into(),
        producer_name: "scip-typescript".into(),
    };
    let admission = ReferenceProfileAdmission::from_trusted_selection(
        &capture,
        &physical.join("artifact.scip"),
        &bytes,
        &physical.join("manifest.json"),
        &manifest_bytes,
        &selection,
        &cancel,
    )
    .unwrap();
    let inventory = decode_reference_profile(&admission, &cancel).unwrap();
    let native = native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
    let sources = capture
        .files
        .iter()
        .map(|f| {
            (
                native::DocumentKey {
                    source_set_id: native.revision.source_set_id.clone(),
                    language: f.language.clone(),
                    path: f.path.clone(),
                },
                f.text.as_bytes().to_vec(),
            )
        })
        .collect();
    (root, native, sources, inventory)
}

fn two_document_fixture(
    conflicting_target: bool,
    plain_reference: bool,
) -> (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
) {
    two_document_fixture_with_claims(conflicting_target, plain_reference, false, None, false)
}

fn two_document_fixture_with_claims(
    conflicting_target: bool,
    plain_reference: bool,
    relationship: bool,
    repeated_reference_roles: Option<i32>,
    base_info_in_a: bool,
) -> (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
) {
    two_document_fixture_for_language(
        "javascript",
        conflicting_target,
        plain_reference,
        relationship,
        repeated_reference_roles,
        base_info_in_a,
    )
}

fn two_document_fixture_for_language(
    language: &str,
    conflicting_target: bool,
    plain_reference: bool,
    relationship: bool,
    repeated_reference_roles: Option<i32>,
    base_info_in_a: bool,
) -> (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
) {
    use protobuf::{EnumOrUnknown, Message};
    let root = tempfile::tempdir().unwrap();
    let physical = fs::canonicalize(root.path()).unwrap();
    fs::create_dir(physical.join("src")).unwrap();
    let (a_path, b_path, producer_name) = match language {
        "javascript" => ("src/A.js", "src/B.js", "scip-typescript"),
        "java" => ("src/A.java", "src/B.java", "scip-java"),
        _ => panic!("unsupported test language"),
    };
    let (a, b) = if language == "java" {
        assert!(relationship);
        (
            "class Child extends Base {}\n",
            "class Base { int method(){ return 1; } }\n",
        )
    } else if relationship {
        (
            "class Child extends Base {}\n",
            "class Base { method(){ return 1; } }\n",
        )
    } else {
        (
            "function run(){ target(); }\n",
            "function target(){ return 1; }\n",
        )
    };
    fs::write(physical.join(a_path), a).unwrap();
    fs::write(physical.join(b_path), b).unwrap();
    let mut binary = scip::types::Index::new();
    for (path, text) in [(a_path, a), (b_path, b)] {
        let mut doc = scip::types::Document::new();
        doc.relative_path = path.into();
        doc.language = language.into();
        doc.position_encoding = EnumOrUnknown::from_i32(2);
        let is_a = path == a_path;
        let (name, symbol) = if relationship {
            ("Base", "pkg Base#")
        } else {
            ("target", "pkg target.")
        };
        let start = text.find(name).unwrap();
        let roles = if is_a { 0 } else { 1 };
        let mut occurrence = scip::types::Occurrence::new();
        occurrence.range = vec![0, start as i32, (start + name.len()) as i32];
        occurrence.symbol = symbol.into();
        occurrence.symbol_roles = roles;
        occurrence.syntax_kind = EnumOrUnknown::from_i32(0);
        if is_a && !plain_reference && !relationship {
            occurrence.enclosing_range = vec![0, start as i32, (start + 8) as i32];
        }
        doc.occurrences.push(occurrence.clone());
        if is_a && conflicting_target {
            let mut conflict = occurrence.clone();
            conflict.symbol = "external pkg other.".into();
            doc.occurrences.push(conflict);
        }
        if is_a {
            if let Some(roles) = repeated_reference_roles {
                let mut repeated = occurrence;
                repeated.symbol_roles = roles;
                repeated.syntax_kind = EnumOrUnknown::from_i32(1);
                doc.occurrences.push(repeated);
            }
            let (owner_name, owner_symbol) = if relationship {
                ("Child", "pkg Child#")
            } else {
                ("run", "pkg run.")
            };
            let start = text.find(owner_name).unwrap();
            let mut independent = scip::types::Occurrence::new();
            independent.range = vec![0, start as i32, (start + owner_name.len()) as i32];
            independent.symbol = owner_symbol.into();
            independent.symbol_roles = 1;
            independent.syntax_kind = EnumOrUnknown::from_i32(0);
            doc.occurrences.push(independent);
            let mut info = scip::types::SymbolInformation::new();
            info.symbol = owner_symbol.into();
            info.kind = EnumOrUnknown::from_i32(if relationship { 7 } else { 26 });
            info.display_name = owner_name.into();
            if relationship {
                let mut edge = scip::types::Relationship::new();
                edge.symbol = symbol.into();
                edge.is_reference = true;
                edge.is_implementation = true;
                info.relationships.push(edge);
            }
            doc.symbols.push(info);
        }
        if !is_a || base_info_in_a {
            let mut info = scip::types::SymbolInformation::new();
            info.symbol = symbol.into();
            info.kind = EnumOrUnknown::from_i32(if relationship { 7 } else { 26 });
            info.display_name = name.into();
            doc.symbols.push(info);
        }
        binary.documents.push(doc);
    }
    let bytes = binary.write_to_bytes().unwrap();
    fs::write(physical.join("artifact.scip"), &bytes).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(physical.clone());
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical).unwrap();
    let native = native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
    let docs: Vec<_> = capture
        .files
        .iter()
        .map(|f| json!({"language":f.language,"relativePath":f.path,"contentHash":f.hash}))
        .collect();
    let manifest_hash = native_ids::digest(
        b"",
        &native_ids::canonical(&json!(
            native
                .revision
                .documents
                .iter()
                .map(|d| json!({"document":d.key,"contentHash":d.content_hash}))
                .collect::<Vec<_>>()
        )),
    );
    let profile: serde_json::Value = serde_json::from_str(include_str!(
        "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
    ))
    .unwrap();
    let pinned = profile["semanticProducers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == producer_name)
        .unwrap();
    let manifest = json!({"formatVersion":1,"trustOrigin":"referenceProfile","completion":"complete","decoderVersion":trellis::scip_raw::DECODER_VERSION,"profileId":profile["profileId"],
        "producer":{"name":pinned["name"],"version":pinned["version"],"executableHash":pinned["executableHash"],"positionEncoding":2},
        "sourceSet":{"id":native.source_set.id,"rootId":native.source_set.root_id,"languages":native.source_set.languages,"dependencies":native.source_set.dependencies},
        "sourceRevision":native.revision.id,"sourceManifestHash":manifest_hash,
        "components":{"toolchainHash":native.revision.toolchain_hash,"configHash":native.revision.config_hash,"dependencyHash":native.revision.dependency_hash},
        "metadata":{"protocolVersion":0,"projectRoot":format!("file://{}",physical.display()),"textDocumentEncoding":1},
        "sourceDocuments":docs,"documents":docs,"exclusions":[],
        "artifacts":[{"kind":"scipBinary","path":"artifact.scip","hash":native_ids::digest(b"",&bytes),"language":language,"documents":[a_path,b_path]}]});
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(physical.join("manifest.json"), &manifest_bytes).unwrap();
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let admission = ReferenceProfileAdmission::from_trusted_selection(
        &capture,
        &physical.join("artifact.scip"),
        &bytes,
        &physical.join("manifest.json"),
        &manifest_bytes,
        &TrustedSelection {
            expected_manifest_hash: native_ids::digest(b"", &manifest_bytes),
            profile_id: "mac-mini-m5-pro-v1".into(),
            producer_name: producer_name.into(),
        },
        &cancel,
    )
    .unwrap();
    let inventory = decode_reference_profile(&admission, &cancel).unwrap();
    let sources = capture
        .files
        .iter()
        .map(|f| {
            (
                native::DocumentKey {
                    source_set_id: native.revision.source_set_id.clone(),
                    language: f.language.clone(),
                    path: f.path.clone(),
                },
                f.text.as_bytes().to_vec(),
            )
        })
        .collect();
    (root, native, sources, inventory)
}

fn basis(
    native: &native::Artifact,
    inventory: &CapturedScipInventory,
) -> BTreeMap<native::DocumentKey, semantic::SemanticBasis> {
    native
        .revision
        .documents
        .iter()
        .map(|doc| {
            (
                doc.key.clone(),
                semantic::SemanticBasis {
                    producer_id: "scip-typescript".into(),
                    producer_version: "0.0".into(),
                    producer_hash: "0".repeat(64),
                    artifact_hash: inventory.artifact_hash.clone(),
                    language: semantic::Language::Javascript,
                    source_set_id: doc.key.source_set_id.clone(),
                    revision_id: native.revision.id.clone(),
                    source_manifest_hash: inventory.source_manifest_hash.clone(),
                    toolchain_hash: native.revision.toolchain_hash.clone(),
                    config_hash: native.revision.config_hash.clone(),
                    dependency_hash: native.revision.dependency_hash.clone(),
                    lookup_dependencies: vec![],
                },
            )
        })
        .collect()
}
fn producer() -> semantic::Producer {
    semantic::Producer {
        id: "scip-typescript".into(),
        version: "0.0".into(),
        executable_hash: "0".repeat(64),
        kind: semantic::SemanticKind::Semantic,
        languages: vec![semantic::Language::Javascript],
        position_encoding: semantic::PositionEncoding::Utf16,
    }
}
#[test]
fn unchanged_caller_revalidates_destination_and_mints_raw_linked_proofs() {
    let (_root, captured, sources, inventory) = fixture();
    let mut destination = captured.clone();
    destination.revision.id = "new-native-revision".into();
    for d in &mut destination.revision.documents {
        d.revision_id = destination.revision.id.clone();
    }
    for r in &mut destination.references {
        r.revision_id = destination.revision.id.clone();
    }
    for c in &mut destination.calls {
        c.revision_id = destination.revision.id.clone();
    }
    for d in &mut destination.declarations {
        d.revision_id = destination.revision.id.clone();
    }
    for r in &mut destination.capability_reports {
        r.revision_id = destination.revision.id.clone();
    }
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &destination,
        &sources,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    assert_eq!(
        result.publication.coverage[0].state,
        semantic::CoverageState::Complete
    );
    assert!(!result.publication.declaration_bindings.is_empty());
    for p in &result.publication.provenance {
        assert_eq!(p.revision_id, destination.revision.id);
        assert_eq!(p.freshness, semantic::Freshness::PossiblyStale);
        let raw = p.derived_from.as_ref().unwrap();
        let claim = inventory
            .lookup(
                &raw.provenance_id,
                serde_json::to_value(raw.record_kind)
                    .unwrap()
                    .as_str()
                    .unwrap(),
                &raw.fact_digest,
            )
            .unwrap();
        assert_eq!(claim.envelope.relative_path, p.document.path);
        assert_ne!(p.id, raw.provenance_id);
        assert_eq!(p.basis.as_ref().unwrap().revision_id, captured.revision.id);
    }
    assert!(
        result
            .publication
            .call_bindings
            .iter()
            .all(|c| c.stale_target != Some(true))
    );
}
#[test]
fn two_distinct_raw_claims_on_one_declaration_rejoin_as_separate_proofs() {
    use protobuf::Message;
    let (root, captured, sources, _) = fixture();
    let physical = fs::canonicalize(root.path()).unwrap();
    let artifact_path = physical.join("artifact.scip");
    let mut binary =
        scip::types::Index::parse_from_bytes(&fs::read(&artifact_path).unwrap()).unwrap();
    let first = binary.documents[0]
        .occurrences
        .iter()
        .find(|o| o.symbol_roles & 1 != 0)
        .unwrap()
        .clone();
    let mut second = first.clone();
    second.symbol = "pkg other.f".into();
    binary.documents[0].occurrences.push(second);
    let bytes = binary.write_to_bytes().unwrap();
    fs::write(&artifact_path, &bytes).unwrap();
    let manifest_path = physical.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["artifacts"][0]["hash"] = json!(native_ids::digest(b"", &bytes));
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(&manifest_path, &manifest_bytes).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(physical.clone());
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let admission = ReferenceProfileAdmission::from_trusted_selection(
        &capture,
        &physical.join("artifact.scip"),
        &bytes,
        &physical.join("manifest.json"),
        &manifest_bytes,
        &TrustedSelection {
            expected_manifest_hash: native_ids::digest(b"", &manifest_bytes),
            profile_id: "mac-mini-m5-pro-v1".into(),
            producer_name: "scip-typescript".into(),
        },
        &cancel,
    )
    .unwrap();
    let inventory = decode_reference_profile(&admission, &cancel).unwrap();
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &captured,
        &sources,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    let bindings: Vec<_> = result
        .publication
        .declaration_bindings
        .iter()
        .filter(|b| b.join.status == semantic::JoinStatus::Exact)
        .collect();
    assert_eq!(bindings.len(), 2);
    assert_eq!(bindings[0].syntax_id, bindings[1].syntax_id);
    let proofs: std::collections::BTreeSet<_> = bindings
        .iter()
        .map(|b| {
            result
                .publication
                .provenance
                .iter()
                .find(|p| p.id == b.provenance_id)
                .unwrap()
                .derived_from
                .as_ref()
                .unwrap()
                .provenance_id
                .as_str()
        })
        .collect();
    assert_eq!(proofs.len(), 2);
}

#[test]
fn authored_reference_does_not_require_an_unused_declaration_family() {
    let (_root, captured, sources, inventory) = fixture_with_extra_occurrences(false, true);
    let claim = inventory
        .claims
        .iter()
        .find(|claim| claim.envelope.raw["symbol"].as_str() == Some("pkg g."))
        .unwrap();
    let joined =
        trellis::semantic_import::join_inventory(&inventory, &captured, &sources, 2).unwrap();
    assert!(
        joined
            .reference_claims
            .contains_key(&claim.captured_proof_id)
    );
    assert!(joined.declaration_bindings.iter().any(|binding| {
        binding.provenance_id == claim.captured_proof_id && binding.syntax_id.is_none()
    }));
    let reference = joined.reference_claims[&claim.captured_proof_id].clone();
    for case in [
        "surviving_reference",
        "missing_reference",
        "ambiguous_reference",
    ] {
        let mut destination = captured.clone();
        match case {
            "surviving_reference" => {}
            "missing_reference" => {
                destination.references.retain(|row| row.id != reference.id);
            }
            "ambiguous_reference" => {
                let mut duplicate = captured
                    .references
                    .iter()
                    .find(|row| row.id == reference.id)
                    .unwrap()
                    .clone();
                duplicate.id = captured.calls[0].id.clone();
                destination.references.push(duplicate);
            }
            _ => unreachable!(),
        }
        for measured in [&captured, &destination] {
            let facts = trellis::semantic_import::MeasuredFacts::new(measured, &sources).unwrap();
            let declaration = facts
                .join(
                    &reference.document,
                    reference.range.clone(),
                    semantic::JoinKind::DeclarationName,
                )
                .unwrap();
            assert_eq!(
                declaration.status,
                semantic::JoinStatus::Unmatched,
                "{case}"
            );
        }
        let result = rejoin_inventory(
            &inventory,
            &captured,
            &sources,
            &destination,
            &sources,
            producer(),
            &basis(&captured, &inventory),
            2,
        )
        .unwrap();
        if case == "surviving_reference" {
            assert_eq!(
                result.publication.coverage[0].state,
                semantic::CoverageState::Complete
            );
            assert_eq!(result.publication.declaration_bindings.len(), 1);
            assert!(
                result
                    .publication
                    .references
                    .iter()
                    .any(|row| row.id == reference.id)
            );
            assert_eq!(result.report["documents"][0]["matched"], 2);
            assert_eq!(result.report["documents"][0]["skipped"], 0);
        } else {
            assert_eq!(
                result.publication.coverage[0].state,
                semantic::CoverageState::Failed,
                "{case}"
            );
            assert!(result.publication.provenance.is_empty(), "{case}");
            assert!(result.publication.symbols.is_empty(), "{case}");
            assert!(result.publication.declaration_bindings.is_empty(), "{case}");
            assert!(result.publication.references.is_empty(), "{case}");
            assert!(result.publication.call_bindings.is_empty(), "{case}");
            assert!(result.publication.type_relationships.is_empty(), "{case}");
            assert_eq!(result.report["documents"][0]["matched"], 0, "{case}");
            assert_eq!(result.report["documents"][0]["skipped"], 2, "{case}");
        }
    }
}

#[test]
fn authenticated_unmatched_occurrences_stay_skipped_without_failing_independent_facts() {
    let (_root, captured, sources, inventory) = fixture_with_extra_occurrences(true, false);
    let unmatched: Vec<_> = inventory
        .claims
        .iter()
        .filter(|claim| {
            claim.envelope.record_kind == "occurrence"
                && claim.envelope.raw["symbol"]
                    .as_str()
                    .is_some_and(|symbol| symbol.starts_with("unmatched"))
        })
        .collect();
    assert_eq!(unmatched.len(), 2);
    let joined =
        trellis::semantic_import::join_inventory(&inventory, &captured, &sources, 2).unwrap();
    assert!(joined.declaration_bindings.iter().any(|binding| {
        binding.syntax_id.is_none()
            && unmatched
                .iter()
                .any(|claim| claim.captured_proof_id == binding.provenance_id)
    }));
    let mut destination = captured.clone();
    destination.revision.id = "new-native-revision".into();
    for document in &mut destination.revision.documents {
        document.revision_id = destination.revision.id.clone();
    }
    for declaration in &mut destination.declarations {
        declaration.revision_id = destination.revision.id.clone();
    }
    for reference in &mut destination.references {
        reference.revision_id = destination.revision.id.clone();
    }
    for call in &mut destination.calls {
        call.revision_id = destination.revision.id.clone();
    }
    for report in &mut destination.capability_reports {
        report.revision_id = destination.revision.id.clone();
    }
    for measured in [&captured, &destination] {
        let facts = trellis::semantic_import::MeasuredFacts::new(measured, &sources).unwrap();
        for kind in [
            semantic::JoinKind::DeclarationName,
            semantic::JoinKind::Reference,
        ] {
            let join = facts
                .join(
                    &measured.revision.documents[0].key,
                    native::Range { start: 0, end: 8 },
                    kind,
                )
                .unwrap();
            assert_eq!(join.status, semantic::JoinStatus::Unmatched);
        }
        let result = rejoin_inventory(
            &inventory,
            &captured,
            &sources,
            measured,
            &sources,
            producer(),
            &basis(&captured, &inventory),
            2,
        )
        .unwrap();
        assert_eq!(
            result.publication.coverage[0].state,
            semantic::CoverageState::Complete
        );
        assert_eq!(result.publication.declaration_bindings.len(), 1);
        assert!(
            result.publication.declaration_bindings[0]
                .syntax_id
                .is_some()
        );
        assert!(result.publication.provenance.iter().all(|proof| {
            unmatched.iter().all(|claim| {
                proof.derived_from.as_ref().unwrap().provenance_id != claim.captured_proof_id
            })
        }));
        assert_eq!(result.report["documents"][0]["requested"], 3);
        assert_eq!(result.report["documents"][0]["matched"], 1);
        assert_eq!(result.report["documents"][0]["skipped"], 2);
        assert_eq!(result.report["documents"][0]["ambiguous"], 0);
        assert_eq!(result.report["documents"][0]["unsupported"], 0);
    }
}

#[test]
fn ambiguous_destination_declaration_fails_the_whole_caller_tuple() {
    let (_root, captured, sources, inventory) = fixture();
    let joined =
        trellis::semantic_import::join_inventory(&inventory, &captured, &sources, 2).unwrap();
    assert!(joined.declaration_bindings.iter().any(|binding| {
        binding.syntax_id.is_some() && binding.join.status == semantic::JoinStatus::Exact
    }));
    let mut destination = captured.clone();
    let mut duplicate = destination
        .declarations
        .iter()
        .find(|d| d.name.as_deref() == Some("f"))
        .unwrap()
        .clone();
    duplicate.syntax_id = destination
        .declarations
        .iter()
        .find(|d| d.name.as_deref() == Some("g"))
        .map(|d| d.syntax_id.clone())
        .unwrap_or_else(|| destination.calls[0].id.clone());
    destination.declarations.push(duplicate);
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &destination,
        &sources,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    assert_eq!(
        result.publication.coverage[0].state,
        semantic::CoverageState::Failed
    );
    assert!(result.publication.provenance.is_empty());
    assert!(result.publication.symbols.is_empty());
    assert!(result.publication.declaration_bindings.is_empty());
    assert!(result.publication.references.is_empty());
    assert!(result.publication.call_bindings.is_empty());
    assert!(result.publication.type_relationships.is_empty());
    assert_eq!(result.report["documents"][0]["matched"], 0);
    assert_eq!(
        result.report["documents"][0]["skipped"],
        result.report["documents"][0]["requested"]
    );
}

#[test]
fn changed_caller_has_failed_coverage_and_no_facts() {
    let (_root, captured, sources, inventory) = fixture();
    let mut destination = captured.clone();
    let mut changed = sources.clone();
    let key = destination.revision.documents[0].key.clone();
    changed.get_mut(&key).unwrap().push(b' ');
    destination.revision.documents[0].content_hash =
        native_ids::digest(b"", changed.get(&key).unwrap());
    destination.revision.documents[0].byte_length = changed[&key].len();
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &destination,
        &changed,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    assert_eq!(
        result.publication.coverage[0].state,
        semantic::CoverageState::Failed
    );
    assert!(
        result.publication.provenance.is_empty()
            && result.publication.declaration_bindings.is_empty()
            && result.publication.call_bindings.is_empty()
    );
}

#[test]
fn missing_destination_source_anchor_fails_even_when_bytes_and_occurrence_ids_match() {
    let (_root, captured, sources, inventory) = fixture();
    let joined =
        trellis::semantic_import::join_inventory(&inventory, &captured, &sources, 2).unwrap();
    assert!(joined.declaration_bindings.iter().any(|binding| {
        binding.syntax_id.is_some() && binding.join.status == semantic::JoinStatus::Exact
    }));
    let mut destination = captured.clone();
    let declaration = captured
        .declarations
        .iter()
        .find(|d| d.name.as_deref() == Some("f"))
        .unwrap();
    destination
        .declarations
        .retain(|d| d.syntax_id != declaration.syntax_id);
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &destination,
        &sources,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    assert_eq!(
        result.publication.coverage[0].state,
        semantic::CoverageState::Failed
    );
    assert!(result.publication.provenance.is_empty());
    assert!(result.publication.symbols.is_empty());
    assert!(result.publication.declaration_bindings.is_empty());
    assert!(result.publication.references.is_empty());
    assert!(result.publication.call_bindings.is_empty());
    assert!(result.publication.type_relationships.is_empty());
    assert_eq!(result.report["documents"][0]["matched"], 0);
    assert_eq!(
        result.report["documents"][0]["skipped"],
        result.report["documents"][0]["requested"]
    );
}
#[test]
fn call_failed_target_drops_only_its_raw_claim_and_matches_reference_resolution() {
    let (_root, captured, sources, inventory) = two_document_fixture(true, false);
    let mut destination = captured.clone();
    destination
        .declarations
        .retain(|d| d.document.path != "src/B.js");
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &destination,
        &sources,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    let calls: Vec<_> = result
        .publication
        .call_bindings
        .iter()
        .filter(|c| c.join.anchor.document.path == "src/A.js")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].resolution, semantic::Resolution::External);
    assert!(calls[0].candidates.is_empty());
    let reference = result
        .publication
        .references
        .iter()
        .find(|r| r.document.path == "src/A.js" && r.spelling == "target")
        .unwrap();
    assert_eq!(calls[0].resolution, reference.resolution);
    assert_eq!(calls[0].declared_target, reference.declared_target);
    for claim in inventory.claims.iter().filter(|c| {
        c.envelope.record_kind == "occurrence"
            && c.envelope.relative_path == "src/A.js"
            && c.envelope.raw["symbolRoles"] == 0
    }) {
        let admitted = result.publication.provenance.iter().any(|p| {
            p.evidence_kind == semantic::EvidenceKind::SemanticReference
                && p.derived_from
                    .as_ref()
                    .is_some_and(|raw| raw.provenance_id == claim.captured_proof_id)
        });
        assert_eq!(
            admitted,
            claim.envelope.raw["symbol"] == "external pkg other."
        );
    }
    assert_eq!(result.report["documents"][0]["state"], "partial");
    assert_eq!(result.report["documents"][0]["matched"], 2);
    assert_eq!(result.report["documents"][0]["skipped"], 1);
    assert_eq!(result.report["documents"][0]["ambiguous"], 0);
}

#[test]
fn same_anchor_use_and_definition_claims_have_one_declaration_site_in_both_orders() {
    use protobuf::Message;
    for definition_first in [false, true] {
        let (root, captured, sources, _) = fixture();
        let physical = fs::canonicalize(root.path()).unwrap();
        let artifact_path = physical.join("artifact.scip");
        let mut binary =
            scip::types::Index::parse_from_bytes(&fs::read(&artifact_path).unwrap()).unwrap();
        let definition = binary.documents[0]
            .occurrences
            .iter()
            .find(|o| o.symbol_roles & 1 != 0)
            .unwrap()
            .clone();
        let mut use_claim = definition.clone();
        use_claim.symbol_roles = 0;
        binary.documents[0].occurrences.push(use_claim);
        let bytes = binary.write_to_bytes().unwrap();
        fs::write(&artifact_path, &bytes).unwrap();
        let manifest_path = physical.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["artifacts"][0]["hash"] = json!(native_ids::digest(b"", &bytes));
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        fs::write(&manifest_path, &manifest_bytes).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let capture =
            Capture::admit(&IndexOptions::new(physical.clone()), &cancel, &|_| {}).unwrap();
        let admission = ReferenceProfileAdmission::from_trusted_selection(
            &capture,
            &artifact_path,
            &bytes,
            &manifest_path,
            &manifest_bytes,
            &TrustedSelection {
                expected_manifest_hash: native_ids::digest(b"", &manifest_bytes),
                profile_id: "mac-mini-m5-pro-v1".into(),
                producer_name: "scip-typescript".into(),
            },
            &cancel,
        )
        .unwrap();
        let mut inventory = decode_reference_profile(&admission, &cancel).unwrap();
        inventory.claims.sort_by_key(|claim| {
            let definition = claim.envelope.raw["symbolRoles"]
                .as_u64()
                .is_some_and(|roles| roles & 1 != 0);
            (
                definition != definition_first,
                claim.captured_proof_id.clone(),
            )
        });
        let joined =
            trellis::semantic_import::join_inventory(&inventory, &captured, &sources, 2).unwrap();
        let references: Vec<_> = joined
            .references
            .iter()
            .filter(|r| r.spelling == "f")
            .collect();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].site, semantic::ReferenceSite::Declaration);
        assert!(references[0].roles.contains(&semantic::Role::Definition));
        assert!(references[0].roles.contains(&semantic::Role::Read));
        let result = rejoin_inventory(
            &inventory,
            &captured,
            &sources,
            &captured,
            &sources,
            producer(),
            &basis(&captured, &inventory),
            2,
        )
        .unwrap();
        let references: Vec<_> = result
            .publication
            .references
            .iter()
            .filter(|r| r.spelling == "f")
            .collect();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].site, semantic::ReferenceSite::Declaration);
        assert!(references[0].roles.contains(&semantic::Role::Definition));
        assert!(references[0].roles.contains(&semantic::Role::Read));
    }
}

#[test]
fn contradictory_exact_targets_remain_ambiguous_with_all_raw_links() {
    let (_root, captured, sources, inventory) = two_document_fixture(true, false);
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &captured,
        &sources,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    let a = captured
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == "src/A.js")
        .unwrap()
        .key
        .clone();
    let calls: Vec<_> = result
        .publication
        .call_bindings
        .iter()
        .filter(|c| c.join.anchor.document == a)
        .collect();
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|c| c.resolution == semantic::Resolution::Ambiguous && c.candidates.len() == 2)
    );
    let proofs: std::collections::BTreeSet<_> = calls
        .iter()
        .map(|c| {
            result
                .publication
                .provenance
                .iter()
                .find(|p| p.id == c.provenance_id)
                .unwrap()
                .derived_from
                .as_ref()
                .unwrap()
                .provenance_id
                .as_str()
        })
        .collect();
    assert_eq!(proofs.len(), 2);
    assert_eq!(result.report["documents"][0]["requested"], 3);
    assert_eq!(result.report["documents"][0]["matched"], 1);
    assert_eq!(result.report["documents"][0]["ambiguous"], 2);
}

#[test]
fn plain_reference_failed_target_does_not_admit_its_raw_proof() {
    let (_root, captured, sources, inventory) = two_document_fixture(true, true);
    let mut destination = captured.clone();
    destination
        .declarations
        .retain(|d| d.document.path != "src/B.js");
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &destination,
        &sources,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    let references: Vec<_> = result
        .publication
        .references
        .iter()
        .filter(|r| r.document.path == "src/A.js" && r.spelling == "target")
        .collect();
    assert_eq!(references.len(), 1);
    assert_eq!(references[0].resolution, semantic::Resolution::External);
    assert!(references[0].candidates.is_empty());
    for claim in inventory.claims.iter().filter(|c| {
        c.envelope.record_kind == "occurrence"
            && c.envelope.relative_path == "src/A.js"
            && c.envelope.raw["symbolRoles"] == 0
    }) {
        let admitted = result.publication.provenance.iter().any(|p| {
            p.evidence_kind == semantic::EvidenceKind::SemanticReference
                && p.derived_from
                    .as_ref()
                    .is_some_and(|raw| raw.provenance_id == claim.captured_proof_id)
        });
        assert_eq!(
            admitted,
            claim.envelope.raw["symbol"] == "external pkg other."
        );
    }
    assert_eq!(result.report["documents"][0]["state"], "partial");
    assert_eq!(result.report["documents"][0]["matched"], 2);
    assert_eq!(result.report["documents"][0]["skipped"], 1);
    assert_eq!(result.report["documents"][0]["ambiguous"], 0);
}

#[test]
fn unresolved_claim_does_not_make_one_exact_internal_target_ambiguous() {
    use protobuf::Message;
    let (root, captured, sources, _) = two_document_fixture(true, true);
    let physical = fs::canonicalize(root.path()).unwrap();
    let artifact_path = physical.join("artifact.scip");
    let mut binary =
        scip::types::Index::parse_from_bytes(&fs::read(&artifact_path).unwrap()).unwrap();
    let document = binary
        .documents
        .iter_mut()
        .find(|d| d.relative_path == "src/A.js")
        .unwrap();
    document
        .occurrences
        .iter_mut()
        .find(|o| o.symbol == "external pkg other.")
        .unwrap()
        .symbol = "pkg unavailable.".into();
    // Symbol information without a measured declaration is unresolved, not external.
    let mut info = scip::types::SymbolInformation::new();
    info.symbol = "pkg unavailable.".into();
    document.symbols.push(info);
    let bytes = binary.write_to_bytes().unwrap();
    fs::write(&artifact_path, &bytes).unwrap();
    let manifest_path = physical.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["artifacts"][0]["hash"] = json!(native_ids::digest(b"", &bytes));
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(&manifest_path, &manifest_bytes).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(physical.clone()), &cancel, &|_| {}).unwrap();
    let admission = ReferenceProfileAdmission::from_trusted_selection(
        &capture,
        &artifact_path,
        &bytes,
        &manifest_path,
        &manifest_bytes,
        &TrustedSelection {
            expected_manifest_hash: native_ids::digest(b"", &manifest_bytes),
            profile_id: "mac-mini-m5-pro-v1".into(),
            producer_name: "scip-typescript".into(),
        },
        &cancel,
    )
    .unwrap();
    let inventory = decode_reference_profile(&admission, &cancel).unwrap();
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &captured,
        &sources,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    let references: Vec<_> = result
        .publication
        .references
        .iter()
        .filter(|r| r.document.path == "src/A.js" && r.spelling == "target")
        .collect();
    assert_eq!(references.len(), 1);
    assert_eq!(references[0].resolution, semantic::Resolution::Resolved);
    assert!(references[0].candidates.is_empty());
    assert!(matches!(&references[0].declared_target,
        Some(semantic::Target::Internal { document, .. }) if document.path == "src/B.js"));
    for claim in inventory.claims.iter().filter(|c| {
        c.envelope.record_kind == "occurrence"
            && c.envelope.relative_path == "src/A.js"
            && c.envelope.raw["symbolRoles"] == 0
    }) {
        let proofs: Vec<_> = result
            .publication
            .provenance
            .iter()
            .filter(|p| {
                p.evidence_kind == semantic::EvidenceKind::SemanticReference
                    && p.derived_from
                        .as_ref()
                        .is_some_and(|raw| raw.provenance_id == claim.captured_proof_id)
            })
            .collect();
        assert_eq!(proofs.len(), 1);
        let raw = proofs[0].derived_from.as_ref().unwrap();
        inventory
            .lookup(&raw.provenance_id, "occurrence", &raw.fact_digest)
            .unwrap();
    }
    assert_eq!(result.report["documents"][0]["state"], "complete");
    assert_eq!(result.report["documents"][0]["matched"], 3);
    assert_eq!(result.report["documents"][0]["skipped"], 0);
    assert_eq!(result.report["documents"][0]["ambiguous"], 0);
}

#[test]
fn contradictory_plain_references_keep_both_raw_proofs_on_one_r2_reference() {
    let (_root, captured, sources, inventory) = two_document_fixture(true, true);
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &captured,
        &sources,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    let a = captured
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == "src/A.js")
        .unwrap()
        .key
        .clone();
    let reference = result
        .publication
        .references
        .iter()
        .find(|r| r.document == a && r.spelling == "target")
        .unwrap();
    assert_eq!(reference.resolution, semantic::Resolution::Ambiguous);
    assert_eq!(reference.candidates.len(), 2);
    let raw_ids: std::collections::BTreeSet<_> = inventory
        .claims
        .iter()
        .filter(|c| {
            c.envelope.record_kind == "occurrence"
                && c.envelope.relative_path == a.path
                && c.envelope.raw["symbolRoles"] == 0
        })
        .map(|c| c.captured_proof_id.clone())
        .collect();
    assert_eq!(raw_ids.len(), 2);
    let proofs: Vec<_> = result
        .publication
        .provenance
        .iter()
        .filter(|p| p.document == a && p.evidence_kind == semantic::EvidenceKind::SemanticReference)
        .collect();
    for raw_id in &raw_ids {
        let raw = inventory
            .claims
            .iter()
            .find(|c| &c.captured_proof_id == raw_id)
            .unwrap();
        assert!(
            proofs.iter().any(|p| p.revision_id == captured.revision.id
                && p.derived_from
                    .as_ref()
                    .is_some_and(|d| d.provenance_id == *raw_id
                        && d.record_kind == semantic::RecordKind::Occurrence
                        && d.fact_digest == raw.fact_digest)),
            "raw={raw_id} raw_ids={raw_ids:?} proofs={proofs:?} reference={reference:?}"
        );
    }
    assert_eq!(result.report["documents"][0]["ambiguous"], 2);
    assert_eq!(result.report["documents"][0]["skipped"], 0);
}

#[test]
fn required_call_invocation_and_reference_must_both_be_unique_at_r2() {
    let (_root, captured, sources, inventory) = two_document_fixture(false, false);
    let a = captured
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == "src/A.js")
        .unwrap()
        .key
        .clone();
    let baseline = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &captured,
        &sources,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    assert!(
        baseline
            .publication
            .call_bindings
            .iter()
            .any(|c| c.join.anchor.document == a)
    );
    assert_eq!(baseline.report["documents"][0]["state"], "complete");
    for failure in [
        "missing_invocation",
        "missing_reference",
        "ambiguous_invocation",
        "ambiguous_reference",
    ] {
        let mut destination = captured.clone();
        match failure {
            "missing_invocation" => {
                destination.calls.retain(|call| call.document != a);
                assert!(destination.references.iter().any(|r| r.document == a));
            }
            "missing_reference" => {
                destination
                    .references
                    .retain(|reference| reference.document != a);
                assert!(destination.calls.iter().any(|c| c.document == a));
            }
            "ambiguous_invocation" => {
                let mut other = destination
                    .calls
                    .iter()
                    .find(|call| call.document == a)
                    .unwrap()
                    .clone();
                other.id = destination
                    .references
                    .iter()
                    .find(|r| r.document == a)
                    .unwrap()
                    .id
                    .clone();
                destination.calls.push(other);
            }
            "ambiguous_reference" => {
                let mut other = destination
                    .references
                    .iter()
                    .find(|r| r.document == a && r.spelling == "target")
                    .unwrap()
                    .clone();
                other.id = destination
                    .calls
                    .iter()
                    .find(|call| call.document == a)
                    .unwrap()
                    .id
                    .clone();
                destination.references.push(other);
            }
            _ => unreachable!(),
        }
        let result = rejoin_inventory(
            &inventory,
            &captured,
            &sources,
            &destination,
            &sources,
            producer(),
            &basis(&captured, &inventory),
            2,
        )
        .unwrap();
        assert_eq!(
            result
                .publication
                .coverage
                .iter()
                .find(|c| c.document_path == a.path)
                .unwrap()
                .state,
            semantic::CoverageState::Failed,
            "{failure}"
        );
        assert!(
            result
                .publication
                .provenance
                .iter()
                .all(|p| p.document != a),
            "{failure}"
        );
        assert!(
            result
                .publication
                .declaration_bindings
                .iter()
                .all(|binding| binding.join.anchor.document != a),
            "{failure}"
        );
        assert!(
            result
                .publication
                .references
                .iter()
                .all(|reference| reference.document != a),
            "{failure}"
        );
        assert!(result.publication.call_bindings.is_empty(), "{failure}");
        for proof_id in result
            .publication
            .symbols
            .iter()
            .map(|symbol| &symbol.provenance_id)
            .chain(
                result
                    .publication
                    .type_relationships
                    .iter()
                    .map(|relationship| &relationship.provenance_id),
            )
        {
            assert!(
                result
                    .publication
                    .provenance
                    .iter()
                    .any(|proof| &proof.id == proof_id && proof.document != a),
                "{failure}"
            );
        }
        assert_eq!(result.report["documents"][0]["matched"], 0, "{failure}");
        assert_eq!(
            result.report["documents"][0]["skipped"], result.report["documents"][0]["requested"],
            "{failure}"
        );
    }
}

#[test]
fn enclosing_range_without_captured_call_witness_cannot_gain_a_call_at_r2() {
    let (_root, measured, sources, inventory) = two_document_fixture(false, false);
    let mut captured = measured.clone();
    captured.calls.retain(|c| c.document.path != "src/A.js");
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &measured,
        &sources,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    assert!(result.publication.call_bindings.is_empty());
    assert!(
        result
            .publication
            .references
            .iter()
            .any(|r| r.spelling == "target")
    );
}

#[test]
fn plain_reference_needs_reference_even_with_exact_callee() {
    let (_root, captured, sources, inventory) = two_document_fixture(false, true);
    let a = captured
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == "src/A.js")
        .unwrap()
        .key
        .clone();
    let mut destination = captured.clone();
    destination.references.retain(|r| r.document != a);
    assert!(destination.calls.iter().any(|c| c.document == a));
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &destination,
        &sources,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    assert_eq!(
        result
            .publication
            .coverage
            .iter()
            .find(|c| c.document_path == a.path)
            .unwrap()
            .state,
        semantic::CoverageState::Failed
    );
    assert!(
        result
            .publication
            .provenance
            .iter()
            .all(|p| p.document != a)
    );
    assert_eq!(
        result.report["documents"][0]["skipped"],
        result.report["documents"][0]["requested"]
    );
}

#[test]
fn changed_target_rejoins_only_with_one_measured_stable_id() {
    let (root, captured, sources, inventory) = two_document_fixture(false, false);
    let a = captured
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == "src/A.js")
        .unwrap()
        .key
        .clone();
    let b = captured
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == "src/B.js")
        .unwrap()
        .key
        .clone();
    let first = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &captured,
        &sources,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    assert!(first.publication.call_bindings.iter().any(|c|c.join.anchor.document==a && matches!(c.declared_target,Some(semantic::Target::Internal {ref document,..}) if document==&b)));
    let physical = fs::canonicalize(root.path()).unwrap();
    fs::write(
        physical.join("src/B.js"),
        "function target(){ return 2; }\n",
    )
    .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(physical.clone());
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical).unwrap();
    let newer = native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
    let changed: BTreeMap<_, _> = capture
        .files
        .iter()
        .map(|f| {
            (
                native::DocumentKey {
                    source_set_id: newer.revision.source_set_id.clone(),
                    language: f.language.clone(),
                    path: f.path.clone(),
                },
                f.text.as_bytes().to_vec(),
            )
        })
        .collect();
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &newer,
        &changed,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    assert_eq!(
        result
            .publication
            .coverage
            .iter()
            .find(|c| c.document_path == b.path)
            .unwrap()
            .state,
        semantic::CoverageState::Failed
    );
    assert!(result.publication.call_bindings.iter().any(|c|c.join.anchor.document==a && matches!(c.declared_target,Some(semantic::Target::Internal {ref document,ref revision_id,..}) if document==&b && revision_id==&newer.revision.id)));
    assert!(
        result
            .publication
            .provenance
            .iter()
            .all(|p| p.document != b)
    );
    assert!(
        result
            .publication
            .provenance
            .iter()
            .all(|p| p.freshness == semantic::Freshness::PossiblyStale)
    );
    let mut absent = newer.clone();
    absent.declarations.retain(|d| d.document != b);
    let omitted = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &absent,
        &changed,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    assert_eq!(
        omitted
            .publication
            .coverage
            .iter()
            .find(|c| c.document_path == a.path)
            .unwrap()
            .state,
        semantic::CoverageState::Partial
    );
    assert!(omitted.publication.call_bindings.is_empty());
    assert!(
        omitted
            .publication
            .declaration_bindings
            .iter()
            .any(|row| row.join.anchor.document == a)
    );
    assert_eq!(
        omitted
            .publication
            .coverage
            .iter()
            .find(|c| c.document_path == a.path)
            .unwrap()
            .observed_roles,
        vec![semantic::Role::Definition]
    );
    assert!(
        omitted
            .publication
            .provenance
            .iter()
            .all(|p| p.document != b)
    );
    assert_eq!(omitted.report["documents"][0]["path"], "src/A.js");
    assert_eq!(omitted.report["documents"][0]["requested"], 2);
    assert_eq!(omitted.report["documents"][0]["matched"], 1);
    assert_eq!(omitted.report["documents"][0]["skipped"], 1);
    assert_eq!(omitted.report["documents"][1]["requested"], 1);
    assert_eq!(omitted.report["documents"][1]["skipped"], 1);
    assert_eq!(omitted.report["languages"][0]["requested"], 3);
    assert_eq!(omitted.report["languages"][0]["matched"], 1);
    assert_eq!(omitted.report["languages"][0]["skipped"], 2);
    let mut nonunique = newer.clone();
    let duplicate = nonunique
        .declarations
        .iter()
        .find(|d| d.document == b && d.name.as_deref() == Some("target"))
        .unwrap()
        .clone();
    nonunique.declarations.push(duplicate);
    let ambiguous = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &nonunique,
        &changed,
        producer(),
        &basis(&captured, &inventory),
        2,
    )
    .unwrap();
    assert_eq!(
        ambiguous
            .publication
            .coverage
            .iter()
            .find(|c| c.document_path == a.path)
            .unwrap()
            .state,
        semantic::CoverageState::Partial
    );
    assert!(ambiguous.publication.call_bindings.is_empty());
}

#[test]
fn same_target_plain_reference_claims_keep_roles_and_every_raw_proof() {
    for repeated_roles in [0, 4] {
        let (root, captured, sources, inventory) =
            two_document_fixture_with_claims(false, true, false, Some(repeated_roles), false);
        let physical = fs::canonicalize(root.path()).unwrap();
        fs::write(
            physical.join("src/B.js"),
            "function target(){ return 2; }\n",
        )
        .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let capture =
            Capture::admit(&IndexOptions::new(physical.clone()), &cancel, &|_| {}).unwrap();
        let identity =
            trellis::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical)
                .unwrap();
        let newer =
            native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
        let changed: BTreeMap<_, _> = capture
            .files
            .iter()
            .map(|f| {
                (
                    native::DocumentKey {
                        source_set_id: newer.revision.source_set_id.clone(),
                        language: f.language.clone(),
                        path: f.path.clone(),
                    },
                    f.text.as_bytes().to_vec(),
                )
            })
            .collect();
        let result = rejoin_inventory(
            &inventory,
            &captured,
            &sources,
            &newer,
            &changed,
            producer(),
            &basis(&captured, &inventory),
            2,
        )
        .unwrap();
        let references: Vec<_> = result
            .publication
            .references
            .iter()
            .filter(|r| r.document.path == "src/A.js" && r.spelling == "target")
            .collect();
        assert_eq!(references.len(), 1);
        let reference = references[0];
        assert_eq!(reference.resolution, semantic::Resolution::Resolved);
        assert!(reference.candidates.is_empty());
        let expected_roles = if repeated_roles == 4 {
            vec![semantic::Role::Read, semantic::Role::Write]
        } else {
            vec![semantic::Role::Read]
        };
        assert_eq!(reference.roles, expected_roles);
        let coverage = result
            .publication
            .coverage
            .iter()
            .find(|c| c.document_path == "src/A.js")
            .unwrap();
        assert_eq!(coverage.state, semantic::CoverageState::Complete);
        for role in &expected_roles {
            assert!(coverage.observed_roles.contains(role));
        }
        let raw_claims: Vec<_> = inventory
            .claims
            .iter()
            .filter(|c| {
                c.envelope.record_kind == "occurrence"
                    && c.envelope.relative_path == "src/A.js"
                    && c.envelope.raw["symbol"].as_str() == Some("pkg target.")
            })
            .collect();
        assert_eq!(raw_claims.len(), 2);
        assert_ne!(
            raw_claims[0].captured_proof_id,
            raw_claims[1].captured_proof_id
        );
        for claim in raw_claims {
            let proofs: Vec<_> = result
                .publication
                .provenance
                .iter()
                .filter(|p| {
                    p.evidence_kind == semantic::EvidenceKind::SemanticReference
                        && p.derived_from
                            .as_ref()
                            .is_some_and(|raw| raw.provenance_id == claim.captured_proof_id)
                })
                .collect();
            assert_eq!(proofs.len(), 1);
            let proof = proofs[0];
            assert_eq!(proof.revision_id, newer.revision.id);
            assert_eq!(
                proof.basis.as_ref().unwrap().revision_id,
                captured.revision.id
            );
            assert_ne!(proof.id, claim.captured_proof_id);
            let raw = proof.derived_from.as_ref().unwrap();
            assert_eq!(raw.fact_digest, claim.fact_digest);
            assert_eq!(raw.record_kind, semantic::RecordKind::Occurrence);
            inventory
                .lookup(&raw.provenance_id, "occurrence", &raw.fact_digest)
                .unwrap();
        }
        assert_eq!(result.report["documents"][0]["requested"], 3);
        assert_eq!(result.report["documents"][0]["matched"], 3);
        assert_eq!(result.report["documents"][0]["skipped"], 0);
        assert_eq!(result.report["documents"][0]["ambiguous"], 0);
    }
}

#[test]
fn java_changed_base_body_does_not_invent_a_filtered_join_diagnostic() {
    let (root, captured, sources, inventory) =
        two_document_fixture_for_language("java", false, true, true, None, true);
    let mut java_producer = producer();
    java_producer.id = "scip-java".into();
    java_producer.languages = vec![semantic::Language::Java];
    let mut java_basis = basis(&captured, &inventory);
    for basis in java_basis.values_mut() {
        basis.producer_id = java_producer.id.clone();
        basis.language = semantic::Language::Java;
    }
    let baseline = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &captured,
        &sources,
        java_producer.clone(),
        &java_basis,
        2,
    )
    .unwrap();
    assert_eq!(baseline.report["documents"][0]["state"], "complete");
    assert_eq!(baseline.publication.type_relationships.len(), 1);
    let physical = fs::canonicalize(root.path()).unwrap();
    fs::write(
        physical.join("src/B.java"),
        "class Base { int method(){ return 2; } }\n",
    )
    .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(physical.clone()), &cancel, &|_| {}).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical).unwrap();
    let newer = native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
    let changed: BTreeMap<_, _> = capture
        .files
        .iter()
        .map(|f| {
            (
                native::DocumentKey {
                    source_set_id: newer.revision.source_set_id.clone(),
                    language: f.language.clone(),
                    path: f.path.clone(),
                },
                f.text.as_bytes().to_vec(),
            )
        })
        .collect();
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &newer,
        &changed,
        java_producer.clone(),
        &java_basis,
        2,
    )
    .unwrap();
    assert_eq!(result.publication.type_relationships.len(), 1);
    assert_eq!(result.report["documents"][0]["state"], "complete");
    assert!(result.report["documents"][0]["diagnostic"].is_null());
    assert_eq!(result.report["documents"][1]["state"], "failed");
    let base = captured
        .declarations
        .iter()
        .find(|d| d.document.path == "src/B.java" && d.name.as_deref() == Some("Base"))
        .unwrap();
    for nonunique in [false, true] {
        let mut invalid = newer.clone();
        if nonunique {
            let duplicate = invalid
                .declarations
                .iter()
                .find(|d| d.syntax_id == base.syntax_id)
                .unwrap()
                .clone();
            invalid.declarations.push(duplicate);
        } else {
            invalid
                .declarations
                .retain(|d| d.syntax_id != base.syntax_id);
        }
        let omitted = rejoin_inventory(
            &inventory,
            &captured,
            &sources,
            &invalid,
            &changed,
            java_producer.clone(),
            &java_basis,
            2,
        )
        .unwrap();
        assert!(omitted.publication.type_relationships.is_empty());
        assert_eq!(omitted.report["documents"][0]["state"], "partial");
    }
}

#[test]
fn java_captured_relationship_diagnostic_still_marks_unchanged_a_partial() {
    let (_root, mut captured, sources, inventory) =
        two_document_fixture_for_language("java", false, true, true, None, true);
    captured
        .declarations
        .retain(|d| !(d.document.path == "src/B.java" && d.name.as_deref() == Some("Base")));
    let mut java_producer = producer();
    java_producer.id = "scip-java".into();
    java_producer.languages = vec![semantic::Language::Java];
    let mut java_basis = basis(&captured, &inventory);
    for basis in java_basis.values_mut() {
        basis.producer_id = java_producer.id.clone();
        basis.language = semantic::Language::Java;
    }
    let joined =
        trellis::semantic_import::join_inventory(&inventory, &captured, &sources, 2).unwrap();
    assert!(joined.diagnostics.iter().any(|(doc, diagnostics)| {
        doc.path == "src/A.java" && diagnostics.contains("unmeasurable_external_base")
    }));
    let result = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &captured,
        &sources,
        java_producer,
        &java_basis,
        2,
    )
    .unwrap();
    assert!(result.publication.type_relationships.is_empty());
    assert_eq!(result.report["documents"][0]["state"], "partial");
    assert!(!result.report["documents"][0]["diagnostic"].is_null());
}

#[test]
fn changed_base_body_preserves_captured_relationship_with_unique_r2_endpoints() {
    for base_info_in_a in [false, true] {
        let (root, captured, sources, inventory) =
            two_document_fixture_with_claims(false, true, true, None, base_info_in_a);
        let baseline = rejoin_inventory(
            &inventory,
            &captured,
            &sources,
            &captured,
            &sources,
            producer(),
            &basis(&captured, &inventory),
            2,
        )
        .unwrap();
        assert_eq!(baseline.publication.type_relationships.len(), 1);
        let physical = fs::canonicalize(root.path()).unwrap();
        fs::write(
            physical.join("src/B.js"),
            "class Base { method(){ return 2; } }\n",
        )
        .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let capture =
            Capture::admit(&IndexOptions::new(physical.clone()), &cancel, &|_| {}).unwrap();
        let identity =
            trellis::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical)
                .unwrap();
        let newer =
            native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
        let changed: BTreeMap<_, _> = capture
            .files
            .iter()
            .map(|f| {
                (
                    native::DocumentKey {
                        source_set_id: newer.revision.source_set_id.clone(),
                        language: f.language.clone(),
                        path: f.path.clone(),
                    },
                    f.text.as_bytes().to_vec(),
                )
            })
            .collect();
        let base = captured
            .declarations
            .iter()
            .find(|d| d.document.path == "src/B.js" && d.name.as_deref() == Some("Base"))
            .unwrap();
        let result = rejoin_inventory(
            &inventory,
            &captured,
            &sources,
            &newer,
            &changed,
            producer(),
            &basis(&captured, &inventory),
            2,
        )
        .unwrap();
        assert_eq!(result.publication.type_relationships.len(), 1);
        let relationship = &result.publication.type_relationships[0];
        assert!(matches!(&relationship.target,
            semantic::Target::Internal { syntax_id, document, revision_id }
                if syntax_id == &base.syntax_id && document == &base.document
                    && revision_id == &newer.revision.id));
        let proof = result
            .publication
            .provenance
            .iter()
            .find(|p| p.id == relationship.provenance_id)
            .unwrap();
        let raw = proof.derived_from.as_ref().unwrap();
        assert_eq!(raw.record_kind, semantic::RecordKind::Relationship);
        inventory
            .lookup(&raw.provenance_id, "relationship", &raw.fact_digest)
            .unwrap();
        assert_eq!(proof.revision_id, newer.revision.id);
        assert_eq!(
            proof.basis.as_ref().unwrap().revision_id,
            captured.revision.id
        );
        assert_eq!(result.report["documents"][0]["state"], "complete");
        assert_eq!(result.report["documents"][0]["matched"], 3);
        assert_eq!(result.report["documents"][1]["state"], "failed");
        assert!(
            result
                .publication
                .provenance
                .iter()
                .all(|p| p.document.path != "src/B.js")
        );
        for nonunique in [false, true] {
            let mut invalid = newer.clone();
            if nonunique {
                let duplicate = invalid
                    .declarations
                    .iter()
                    .find(|d| d.syntax_id == base.syntax_id)
                    .unwrap()
                    .clone();
                invalid.declarations.push(duplicate);
            } else {
                invalid
                    .declarations
                    .retain(|d| d.syntax_id != base.syntax_id);
            }
            let omitted = rejoin_inventory(
                &inventory,
                &captured,
                &sources,
                &invalid,
                &changed,
                producer(),
                &basis(&captured, &inventory),
                2,
            )
            .unwrap();
            assert!(omitted.publication.type_relationships.is_empty());
            assert_eq!(omitted.report["documents"][0]["state"], "partial");
            assert!(
                omitted.publication.declaration_bindings.iter().any(|b| b
                    .join
                    .anchor
                    .document
                    .path
                    == "src/A.js"
                    && b.syntax_id.is_some())
            );
        }
    }
}

#[test]
fn same_revision_all_matching_is_fresh_and_producer_drift_is_possibly_stale() {
    let (_root, captured, sources, inventory) = fixture();
    let original = basis(&captured, &inventory);
    let fresh = rejoin_inventory(
        &inventory,
        &captured,
        &sources,
        &captured,
        &sources,
        producer(),
        &original,
        2,
    )
    .unwrap();
    assert!(!fresh.publication.provenance.is_empty());
    assert!(
        fresh
            .publication
            .provenance
            .iter()
            .all(|p| p.freshness == semantic::Freshness::Fresh)
    );
    let mut changed = producer();
    changed.version = "new-producer-version".into();
    let drift = rejoin_inventory(
        &inventory, &captured, &sources, &captured, &sources, changed, &original, 2,
    )
    .unwrap();
    assert!(
        drift
            .publication
            .provenance
            .iter()
            .all(|p| p.freshness == semantic::Freshness::PossiblyStale)
    );
}
