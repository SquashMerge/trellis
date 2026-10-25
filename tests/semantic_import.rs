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
    semantic_import::{MeasuredFacts, join_inventory},
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

#[test]
fn authenticated_javascript_claims_join_real_native_candidates() {
    let (_root, native, sources, inventory) = fixture();
    assert_eq!(inventory.claims.len(), 2);
    let facts = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(
        facts
            .declaration_bindings
            .iter()
            .any(|b| b.join.status == semantic::JoinStatus::Exact)
    );
    assert!(
        facts
            .joins
            .iter()
            .all(|(_, join)| join.candidate_ids.len() <= 1)
    );
    let doc = native.revision.documents[0].key.clone();
    let measured = MeasuredFacts::new(&native, &sources).unwrap();
    let r = native
        .declarations
        .iter()
        .find_map(|d| d.name_range.clone())
        .unwrap();
    assert_eq!(
        measured
            .join(&doc, r, semantic::JoinKind::DeclarationName)
            .unwrap()
            .status,
        semantic::JoinStatus::Exact
    );
}

#[test]
fn unavailable_capability_is_not_inferred_from_missing_candidate() {
    let (_root, native, sources, _inventory) = fixture();
    let doc = native.revision.documents[0].key.clone();
    let measured = MeasuredFacts::new(&native, &sources).unwrap();
    let join = measured
        .join(
            &doc,
            native::Range { start: 0, end: 8 },
            semantic::JoinKind::Callee,
        )
        .unwrap();
    assert_eq!(join.status, semantic::JoinStatus::Unmatched);
    assert_eq!(join.candidate_ids, Vec::<String>::new());
}

#[test]
fn admitted_raw_coordinates_reject_split_utf16_scalar_and_crlf() {
    use trellis::semantic_import::occurrence_range;
    let (_root, _native, _sources, inventory) = fixture();
    let mut claim = inventory
        .claims
        .iter()
        .find(|c| c.envelope.record_kind == "occurrence")
        .unwrap()
        .clone();
    let text = "😀\r\nBase".as_bytes();
    claim.envelope.raw["range"] = json!({"encoding":"multiLine","startLine":1,"startCharacter":0,"endLine":1,"endCharacter":4});
    assert_eq!(
        occurrence_range(&claim, text, 2).unwrap(),
        native::Range { start: 6, end: 10 }
    );
    claim.envelope.raw["range"] =
        json!({"encoding":"singleLine","line":0,"startCharacter":0,"endCharacter":1});
    assert!(
        occurrence_range(&claim, text, 2)
            .unwrap_err()
            .to_string()
            .contains("splits scalar")
    );
    claim.envelope.raw["range"] =
        json!({"encoding":"singleLine","line":0,"startCharacter":0,"endCharacter":2});
    assert_eq!(
        occurrence_range(&claim, text, 2).unwrap(),
        native::Range { start: 0, end: 4 }
    );
    assert!(occurrence_range(&claim, text, 1).is_err());
    claim.envelope.position_encoding = 0;
    assert_eq!(
        occurrence_range(&claim, text, 2).unwrap(),
        native::Range { start: 0, end: 4 }
    );
}

#[test]
fn four_measured_families_retain_exact_native_identity_and_capability_authority() {
    let (_root, native, sources, _inventory) = fixture();
    let doc = native.revision.documents[0].key.clone();
    let measured = MeasuredFacts::new(&native, &sources).unwrap();
    for d in native
        .declarations
        .iter()
        .filter(|d| d.document == doc && d.name_range.is_some())
    {
        let join = measured
            .join(
                &doc,
                d.name_range.clone().unwrap(),
                semantic::JoinKind::DeclarationName,
            )
            .unwrap();
        assert_eq!(join.status, semantic::JoinStatus::Exact);
        assert_eq!(join.candidate_ids, vec![d.syntax_id.clone()]);
    }
    for call in native.calls.iter().filter(|c| c.document == doc) {
        let invocation = measured
            .join(&doc, call.range.clone(), semantic::JoinKind::Invocation)
            .unwrap();
        assert_eq!(invocation.candidate_ids, vec![call.id.clone()]);
        if let Some(callee) = &call.callee_range {
            let join = measured
                .join(&doc, callee.clone(), semantic::JoinKind::Callee)
                .unwrap();
            assert_eq!(join.candidate_ids, vec![call.id.clone()]);
        }
    }
    for reference in native.references.iter().filter(|r| r.document == doc) {
        let join = measured
            .join(&doc, reference.range.clone(), semantic::JoinKind::Reference)
            .unwrap();
        assert_eq!(join.candidate_ids, vec![reference.id.clone()]);
    }
    let absent = native::Range { start: 0, end: 8 };
    assert_eq!(
        measured
            .join(&doc, absent.clone(), semantic::JoinKind::Callee)
            .unwrap()
            .status,
        semantic::JoinStatus::Unmatched
    );
    let mut gap = native.clone();
    let support = gap.capability_reports[0]
        .families
        .iter_mut()
        .find(|f| f.kind == native::MeasurementKind::Callee)
        .unwrap();
    support.gaps.push(native::MeasurementGap {
        range: absent.clone(),
        diagnostic: "opaque_native_construct".into(),
    });
    assert_eq!(
        MeasuredFacts::new(&gap, &sources)
            .unwrap()
            .join(&doc, absent.clone(), semantic::JoinKind::Callee)
            .unwrap()
            .status,
        semantic::JoinStatus::Unsupported
    );
    assert_eq!(
        MeasuredFacts::new(&gap, &sources)
            .unwrap()
            .join(
                &doc,
                native::Range { start: 0, end: 9 },
                semantic::JoinKind::Callee
            )
            .unwrap()
            .status,
        semantic::JoinStatus::Unmatched
    );
    let support = gap.capability_reports[0]
        .families
        .iter_mut()
        .find(|f| f.kind == native::MeasurementKind::Callee)
        .unwrap();
    support.gaps.clear();
    support.available = false;
    support.diagnostic = Some("no_native_convention".into());
    assert_eq!(
        MeasuredFacts::new(&gap, &sources)
            .unwrap()
            .join(&doc, absent, semantic::JoinKind::Callee)
            .unwrap()
            .diagnostic
            .as_deref(),
        Some("no_native_convention")
    );
}

#[test]
fn duplicate_and_conflicting_authenticated_claims_preserve_raw_proofs() {
    let (_root, native, sources, inventory) = fixture();
    let original = inventory
        .claims
        .iter()
        .find(|c| c.envelope.record_kind == "occurrence")
        .unwrap()
        .clone();
    let mut same = inventory.clone();
    same.claims.push(original.clone());
    assert!(
        join_inventory(&same, &native, &sources, 2).is_err(),
        "duplicate raw proof must fail closed"
    );
    let mut competing = inventory.clone();
    let mut other = original.clone();
    other.envelope.raw["symbol"] = json!("external pkg Other#");
    other.canonical_bytes = native_ids::canonical(
        &json!({"formatVersion":1,"recordKind":"occurrence","relativePath":other.envelope.relative_path,
        "positionEncoding":other.envelope.position_encoding,"raw":other.envelope.raw}),
    );
    other.fact_digest = native_ids::digest(b"trellis.semantic-fact.v1\0", &other.canonical_bytes);
    other.captured_proof_id = native_ids::digest(
        b"trellis.capture-proof.v1\0",
        &native_ids::canonical(&json!({
        "producerId":"scip-typescript","artifactHash":inventory.artifact_hash,"sourceSetId":native.source_set.id,
        "sourceRevision":native.revision.id,"language":other.language,"relativePath":other.envelope.relative_path,
        "factDigest":other.fact_digest})),
    );
    let other_proof = other.captured_proof_id.clone();
    competing.claims.push(other);
    let output = join_inventory(&competing, &native, &sources, 2).unwrap();
    let ids: Vec<_> = output
        .joins
        .iter()
        .filter(|(_, j)| j.anchor.kind == semantic::JoinKind::DeclarationName)
        .map(|(proof, j)| (proof.as_str(), j.status))
        .collect();
    assert!(ids.iter().any(|(id, _)| *id == original.captured_proof_id));
    assert!(ids.iter().any(|(id, _)| *id == other_proof));
}

#[test]
fn reference_and_call_roles_are_not_authorized_by_native_shape_alone() {
    let (_root, native, sources, inventory) = fixture();
    let facts = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(
        facts.call_bindings.is_empty(),
        "no authenticated call occurrence in pinned fixture"
    );
    assert!(
        facts
            .references
            .iter()
            .all(|r| !r.roles.contains(&semantic::Role::Call))
    );
    for (proof, join) in &facts.joins {
        let claim = inventory
            .claims
            .iter()
            .find(|c| &c.captured_proof_id == proof)
            .unwrap();
        assert_eq!(claim.envelope.record_kind, "occurrence");
        assert_eq!(join.anchor.document.language, "javascript");
    }
}
