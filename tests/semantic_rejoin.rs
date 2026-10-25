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
    let root = tempfile::tempdir().unwrap();
    let physical = fs::canonicalize(root.path()).unwrap();
    fs::create_dir(physical.join("src")).unwrap();
    let text = "function f(){g();}\n";
    fs::write(physical.join("src/A.js"), text).unwrap();
    fs::write(physical.join("artifact.scip"), hex::decode(WIRE).unwrap()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut options = IndexOptions::new(physical.clone());
    options.scip_path = Some(physical.join("artifact.scip"));
    options.manifest_path = Some(physical.join("manifest.json"));
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
    let bytes = hex::decode(WIRE).unwrap();
    let uri = format!("file://{}", physical.display());
    let manifest = json!({"formatVersion":1,"trustOrigin":"referenceProfile","completion":"complete","decoderVersion":"scip-0.10.0/CapturedScipFactV1/1","profileId":profile["profileId"],
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
    let admission =
        ReferenceProfileAdmission::from_trusted_selection(&capture, &selection, &cancel).unwrap();
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
    let a = "function run(){ target(); }\n";
    let b = "function target(){ return 1; }\n";
    fs::write(physical.join("src/A.js"), a).unwrap();
    fs::write(physical.join("src/B.js"), b).unwrap();
    let mut binary = scip::types::Index::new();
    for (path, text) in [("src/A.js", a), ("src/B.js", b)] {
        let mut doc = scip::types::Document::new();
        doc.relative_path = path.into();
        doc.language = "javascript".into();
        doc.position_encoding = EnumOrUnknown::from_i32(2);
        let (start, symbol, roles) = if path.ends_with("A.js") {
            (text.find("target").unwrap(), "pkg target.", 0)
        } else {
            (text.find("target").unwrap(), "pkg target.", 1)
        };
        let mut occurrence = scip::types::Occurrence::new();
        occurrence.range = vec![0, start as i32, (start + 6) as i32];
        occurrence.symbol = symbol.into();
        occurrence.symbol_roles = roles;
        occurrence.syntax_kind = EnumOrUnknown::from_i32(0);
        if roles == 0 {
            occurrence.enclosing_range = vec![0, start as i32, (start + 8) as i32];
        }
        doc.occurrences.push(occurrence.clone());
        if roles == 0 && conflicting_target {
            let mut conflict = occurrence;
            conflict.symbol = "external pkg other.".into();
            doc.occurrences.push(conflict);
        }
        if roles == 0 {
            let start = text.find("run").unwrap();
            let mut independent = scip::types::Occurrence::new();
            independent.range = vec![0, start as i32, (start + 3) as i32];
            independent.symbol = "pkg run.".into();
            independent.symbol_roles = 1;
            independent.syntax_kind = EnumOrUnknown::from_i32(0);
            doc.occurrences.push(independent);
            let mut info = scip::types::SymbolInformation::new();
            info.symbol = "pkg run.".into();
            info.kind = EnumOrUnknown::from_i32(26);
            info.display_name = "run".into();
            doc.symbols.push(info);
        }
        if roles == 1 {
            let mut info = scip::types::SymbolInformation::new();
            info.symbol = symbol.into();
            info.kind = EnumOrUnknown::from_i32(26);
            info.display_name = "target".into();
            doc.symbols.push(info);
        }
        binary.documents.push(doc);
    }
    let bytes = binary.write_to_bytes().unwrap();
    fs::write(physical.join("artifact.scip"), &bytes).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut options = IndexOptions::new(physical.clone());
    options.scip_path = Some(physical.join("artifact.scip"));
    options.manifest_path = Some(physical.join("manifest.json"));
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
        .find(|p| p["name"] == "scip-typescript")
        .unwrap();
    let manifest = json!({"formatVersion":1,"trustOrigin":"referenceProfile","completion":"complete","decoderVersion":"scip-0.10.0/CapturedScipFactV1/1","profileId":profile["profileId"],
        "producer":{"name":pinned["name"],"version":pinned["version"],"executableHash":pinned["executableHash"],"positionEncoding":2},
        "sourceSet":{"id":native.source_set.id,"rootId":native.source_set.root_id,"languages":native.source_set.languages,"dependencies":native.source_set.dependencies},
        "sourceRevision":native.revision.id,"sourceManifestHash":manifest_hash,
        "components":{"toolchainHash":native.revision.toolchain_hash,"configHash":native.revision.config_hash,"dependencyHash":native.revision.dependency_hash},
        "metadata":{"protocolVersion":0,"projectRoot":format!("file://{}",physical.display()),"textDocumentEncoding":1},
        "sourceDocuments":docs,"documents":docs,"exclusions":[],
        "artifacts":[{"kind":"scipBinary","path":"artifact.scip","hash":native_ids::digest(b"",&bytes),"language":"javascript","documents":["src/A.js","src/B.js"]}]});
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(physical.join("manifest.json"), &manifest_bytes).unwrap();
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let admission = ReferenceProfileAdmission::from_trusted_selection(
        &capture,
        &TrustedSelection {
            expected_manifest_hash: native_ids::digest(b"", &manifest_bytes),
            profile_id: "mac-mini-m5-pro-v1".into(),
            producer_name: "scip-typescript".into(),
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
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut options = IndexOptions::new(physical);
    options.scip_path = Some(artifact_path);
    options.manifest_path = Some(manifest_path.clone());
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let admission = ReferenceProfileAdmission::from_trusted_selection(
        &capture,
        &TrustedSelection {
            expected_manifest_hash: native_ids::digest(b"", &fs::read(&manifest_path).unwrap()),
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
fn ambiguous_destination_declaration_fails_the_whole_caller_tuple() {
    let (_root, captured, sources, inventory) = fixture();
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
}
#[test]
fn contradictory_exact_targets_remain_ambiguous_with_all_raw_links() {
    let (_root, captured, sources, inventory) = two_document_fixture(true);
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
fn changed_target_rejoins_only_with_one_measured_stable_id() {
    let (root, captured, sources, inventory) = two_document_fixture(false);
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
