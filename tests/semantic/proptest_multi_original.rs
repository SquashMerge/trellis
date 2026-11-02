//! End-to-end Decision 0002 model: two documents in two originals, one producer.
use super::*;
use proptest::prelude::*;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use trellis::semantic_evidence::{CoverageState, SemanticSnapshot, Target};

#[derive(Clone, Copy, Debug)]
enum Invalid {
    Duplicate,
    Hash,
}

#[derive(Clone, Copy, Debug)]
struct Case {
    caller_is_b: bool,
    b_first: bool,
    page_rows: usize,
    invalid: Invalid,
    suffix: u8,
}

fn hash(domain: &[u8], bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(domain);
    h.update(bytes);
    hex::encode(h.finalize())
}

fn selected(world: &World, pin: IndexPin) -> String {
    let inputs = world.store.selected_raw_inputs_at(pin).unwrap();
    // Debug includes every captured binary, proof, declared document, and selection identity.
    format!("{inputs:#?}")
}

fn check_snapshot(world: &World, pin: IndexPin, model: &Model) -> SemanticSnapshot {
    let snapshot = world.store.semantic_at(pin).unwrap();
    for path in [&model.caller, &model.target] {
        let coverage: Vec<_> = snapshot
            .coverage
            .iter()
            .filter(|c| &c.document_path == path)
            .collect();
        assert_eq!(coverage.len(), 1, "coverage for {path}");
        assert!(coverage[0].requested && coverage[0].selected);
        assert_eq!(
            coverage[0].state,
            CoverageState::Complete,
            "coverage for {path}"
        );
    }
    let reference = snapshot
        .references
        .iter()
        .find(|r| r.spelling == "target" && r.document.path == model.caller)
        .expect("positive control: caller reference");
    assert_eq!(reference.document.path, model.caller);
    assert_eq!(reference.resolution, Resolution::Resolved);
    assert!(
        matches!(&reference.declared_target, Some(Target::Internal { document, .. }) if document.path == model.target),
        "reference target: {reference:?}"
    );
    let calls: Vec<_> = snapshot
        .call_bindings
        .iter()
        .filter(|c| c.join.anchor.document.path == model.caller)
        .collect();
    assert_eq!(calls.len(), 1, "positive control: bound call");
    assert!(calls[0].call_id.is_some());
    assert_eq!(calls[0].resolution, Resolution::Resolved);
    assert!(
        matches!(&calls[0].declared_target, Some(Target::Internal { document, .. }) if document.path == model.target),
        "call target: {:?}",
        calls[0]
    );

    let batches = world.store.selected_raw_inputs_at(pin).unwrap();
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    assert_eq!(batch.captured_revision, model.revision);
    assert_eq!(batch.source_manifest_hash, model.manifest);
    assert_eq!(batch.captures.len(), 2);
    let requested: BTreeSet<_> = batch
        .requested_documents
        .iter()
        .map(|d| d.path.clone())
        .collect();
    assert_eq!(requested, model.hashes.keys().cloned().collect());
    assert_eq!(batch.documents.len(), 2);
    for (path, expected_hash) in &model.hashes {
        let (source_hash, _, capture_id) =
            &batch.documents[&(String::from("javascript"), path.clone())];
        assert_eq!(source_hash, &model.source_hashes[path]);
        let capture = batch
            .captures
            .iter()
            .find(|c| c.capture_id == *capture_id)
            .unwrap();
        assert_eq!(&capture.artifact_hash, expected_hash);
        assert_eq!(hash(b"", &capture.binary_bytes), *expected_hash);
    }
    assert_eq!(batch.proofs.len(), 5, "positive control: raw proofs");
    let mut origins = BTreeSet::new();
    let mut facts = BTreeMap::<String, Vec<(String, String)>>::new();
    for (id, proof) in &batch.proofs {
        assert_eq!(proof.language, "javascript");
        assert_eq!(proof.artifact_hash, model.hashes[&proof.path]);
        assert_eq!(
            proof.fact_digest,
            hash(b"trellis.semantic-fact.v1\0", &proof.canonical_bytes)
        );
        let envelope: Value = serde_json::from_slice(&proof.canonical_bytes).unwrap();
        assert_eq!(envelope["relativePath"], proof.path);
        assert_eq!(envelope["recordKind"], proof.record_kind);
        facts.entry(proof.path.clone()).or_default().push((
            proof.record_kind.clone(),
            envelope["raw"]["symbol"].as_str().unwrap().to_owned(),
        ));
        let tuple = json!({"producerId":batch.producer_id,"artifactHash":proof.artifact_hash,"sourceSetId":batch.source_set_id,"sourceRevision":model.revision,"language":proof.language,"relativePath":proof.path,"factDigest":proof.fact_digest});
        assert_eq!(
            *id,
            hash(
                b"trellis.capture-proof.v1\0",
                &serde_json::to_vec(&tuple).unwrap()
            )
        );
    }
    for rows in facts.values_mut() {
        rows.sort();
    }
    assert_eq!(
        facts[&model.caller],
        vec![
            ("occurrence".into(), "pkg run.".into()),
            ("occurrence".into(), "pkg target.".into()),
            ("symbolInformation".into(), "pkg run.".into())
        ]
    );
    assert_eq!(
        facts[&model.target],
        vec![
            ("occurrence".into(), "pkg target.".into()),
            ("symbolInformation".into(), "pkg target.".into())
        ]
    );
    for p in &snapshot.provenance {
        let basis = p.basis.as_ref().expect("semantic basis");
        assert_eq!(basis.artifact_hash, model.hashes[&p.document.path]);
        assert_eq!(basis.revision_id, model.revision);
        assert_eq!(basis.source_manifest_hash, model.manifest);
        let origin = p.derived_from.as_ref().expect("captured origin");
        let (_, proof) = batch
            .proofs
            .iter()
            .find(|(id, _)| id == &origin.provenance_id)
            .expect("origin belongs to selected raw proof");
        assert_eq!(proof.path, p.document.path);
        assert_eq!(proof.artifact_hash, basis.artifact_hash);
        assert_eq!(proof.fact_digest, origin.fact_digest);
        assert_eq!(json!(origin.record_kind), json!(proof.record_kind));
        origins.insert(p.document.path.clone());
    }
    assert_eq!(origins, model.hashes.keys().cloned().collect());
    snapshot
}

struct Model {
    caller: String,
    target: String,
    hashes: BTreeMap<String, String>,
    source_hashes: BTreeMap<String, String>,
    revision: String,
    manifest: String,
}

fn run(case: Case) {
    let (caller, target) = if case.caller_is_b {
        ("src/B.js", "src/A.js")
    } else {
        ("src/A.js", "src/B.js")
    };
    let caller_source = format!("{DEFINITIONS_A}// {}\n", case.suffix);
    let target_source = format!("{DEFINITIONS_B}// {}\n", case.suffix);
    let world =
        world(|root| write_files(root, &[(caller, &caller_source), (target, &target_source)]));
    let (mut a, mut b) = definitions_docs();
    a.path = caller.into();
    b.path = target.into();
    let ordered = if case.b_first == case.caller_is_b {
        vec![a.clone(), b.clone()]
    } else {
        vec![b.clone(), a.clone()]
    };
    let mut fixture = world.fixture().rows_per_page(case.page_rows);
    let mut hashes = BTreeMap::new();
    for doc in &ordered {
        hashes.insert(
            doc.path.clone(),
            hash(
                b"",
                &fixture.scip_bytes("javascript", std::slice::from_ref(doc)),
            ),
        );
        fixture.artifact("javascript", vec![doc.clone()]);
    }
    let descriptor = world.write(&fixture);
    let parsed = descriptor_value(&descriptor);
    let model = Model {
        caller: caller.into(),
        target: target.into(),
        hashes,
        source_hashes: BTreeMap::from([
            (caller.into(), hash(b"", caller_source.as_bytes())),
            (target.into(), hash(b"", target_source.as_bytes())),
        ]),
        revision: parsed["sourceRevision"].as_str().unwrap().into(),
        manifest: parsed["sourceManifestHash"].as_str().unwrap().into(),
    };
    for (ordinal, doc) in ordered.iter().enumerate() {
        assert_eq!(
            parsed["artifacts"][ordinal]["sha256"],
            model.hashes[&doc.path]
        );
    }
    let mut assignments = BTreeMap::new();
    for page in parsed["pages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["kind"] == "coverage")
    {
        let page: Value = serde_json::from_slice(
            &fs::read(world.root.join(page["relativePath"].as_str().unwrap())).unwrap(),
        )
        .unwrap();
        for row in page["rows"].as_array().unwrap() {
            assert_eq!(row["language"], "javascript");
            assert!(row["exclusion"].is_null());
            assert!(
                assignments
                    .insert(
                        row["relativePath"].as_str().unwrap().to_owned(),
                        row["artifact"].as_u64().unwrap() as usize
                    )
                    .is_none()
            );
        }
    }
    let expected: BTreeMap<_, _> = ordered
        .iter()
        .enumerate()
        .map(|(i, d)| (d.path.clone(), i))
        .collect();
    assert_eq!(assignments, expected, "declared coverage assignment");
    let (pin, admitted, _) = world.publish(&descriptor).unwrap();
    drop(admitted);
    let prior = check_snapshot(&world, pin, &model);
    let raw_before = selected(&world, pin);
    let counts = (
        world.count("SELECT COUNT(*) FROM semantic_raw_batches"),
        world.count("SELECT COUNT(*) FROM semantic_raw_captures"),
        world.count("SELECT COUNT(*) FROM semantic_revision_raw_selections"),
    );

    let mut bad = world.fixture().rows_per_page(case.page_rows);
    for doc in &ordered {
        bad.artifact("javascript", vec![doc.clone()]);
    }
    if matches!(case.invalid, Invalid::Duplicate) {
        bad.override_artifact_bytes(1, vec![ordered[1].clone(), ordered[0].clone()]);
    }
    let written = world.write(&bad);
    let bad_descriptor = if matches!(case.invalid, Invalid::Hash) {
        edit_descriptor(&written.0, |d| {
            d["artifacts"][1]["sha256"] = json!("0".repeat(64))
        })
    } else {
        written
    };
    let error = match world.publish(&bad_descriptor) {
        Ok(_) => panic!("invalid later import succeeded: {case:?}"),
        Err(e) => e,
    };
    assert!(
        matches!(
            error.downcast_ref::<ScipCaptureError>(),
            Some(ScipCaptureError::InvalidArtifact(_))
                | Some(ScipCaptureError::InvalidAdmission(_))
        ),
        "expected typed admission rejection, got {error:#}; {case:?}"
    );
    assert_eq!(world.store.index_baseline().unwrap(), pin);
    assert_eq!(selected(&world, pin), raw_before);
    assert_eq!(world.store.semantic_at(pin).unwrap(), prior);
    assert_eq!(
        (
            world.count("SELECT COUNT(*) FROM semantic_raw_batches"),
            world.count("SELECT COUNT(*) FROM semantic_raw_captures"),
            world.count("SELECT COUNT(*) FROM semantic_revision_raw_selections")
        ),
        counts
    );

    write_files(&world.root, &[("src/C.js", "function unrelated(){}\n")]);
    world.republish_with_rejoin();
    let head = world.store.index_baseline().unwrap();
    assert_ne!(head, pin);
    assert_eq!(world.rejoin_failures(), 0);
    check_snapshot(&world, head, &model);
    assert_eq!(world.store.semantic_at(pin).unwrap(), prior);
    assert_eq!(selected(&world, pin), raw_before);
}

#[test]
fn deterministic_witnesses() {
    for caller_is_b in [false, true] {
        for b_first in [false, true] {
            for invalid in [Invalid::Duplicate, Invalid::Hash] {
                run(Case {
                    caller_is_b,
                    b_first,
                    page_rows: 1,
                    invalid,
                    suffix: 0,
                });
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_MULTI_ORIGINAL_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(8), ..ProptestConfig::default() })]
    #[test]
    fn generated_multi_original(caller_is_b in any::<bool>(), b_first in any::<bool>(), page_rows in 1usize..4, duplicate in any::<bool>(), suffix in 0u8..20) {
        run(Case { caller_is_b, b_first, page_rows, invalid: if duplicate { Invalid::Duplicate } else { Invalid::Hash }, suffix });
    }
}
