use super::*;

#[test]
fn narrow_source_witness_rejects_source_drift_and_missing_current_target() {
    let document = native::DocumentKey {
        source_set_id: "set".into(),
        language: "python".into(),
        path: "a.py".into(),
    };
    let revision = native::Revision {
        id: "new".into(),
        source_set_id: "set".into(),
        documents: vec![],
        toolchain_hash: "t".into(),
        config_hash: "c".into(),
        dependency_hash: "d".into(),
    };
    let producer = Producer {
        id: "producer".into(),
        version: "1".into(),
        executable_hash: "e".repeat(64),
        kind: SemanticKind::Semantic,
        languages: vec![Language::Python],
        position_encoding: PositionEncoding::Utf8,
    };
    let validator = EmissionValidator::new(EmissionHeader {
        producer: &producer,
        captured_revision: revision.clone(),
        destination_revision: revision,
        source_set_languages: vec!["python".into()],
        artifact_hashes: BTreeSet::new(),
        source_manifest_hash: "m".into(),
    });
    let prior = Reference {
        id: "reference".into(),
        owner_syntax_id: "owner".into(),
        ordinal: 0,
        document: document.clone(),
        revision_id: "old".into(),
        range: native::Range { start: 1, end: 2 },
        spelling: "a".into(),
        lookup_key: "a".into(),
        site: ReferenceSite::Use,
        roles: vec![Role::Read],
        resolution: Resolution::Unresolved,
        declared_target: None,
        candidates: vec![],
        provenance_id: "raw".into(),
    };
    let mut current = Reference {
        revision_id: "new".into(),
        provenance_id: "derived".into(),
        ..prior.clone()
    };
    let check = |row: &Reference, known: &dyn Fn(&str, &native::DocumentKey) -> Result<bool>| {
        validator.witnessed_fact(
            WitnessFact::Reference(&prior, row),
            "raw",
            "derived",
            &document,
            &"a".repeat(64),
            known,
        )
    };
    check(&current, &|_, _| Ok(false)).unwrap();
    current.range.end += 1;
    assert!(
        check(&current, &|_, _| Ok(true))
            .unwrap_err()
            .to_string()
            .contains("source differs")
    );
    current.range = prior.range.clone();
    current.resolution = Resolution::Resolved;
    current.declared_target = Some(Target::Internal {
        syntax_id: format!("sid:v1:{}", "0".repeat(32)),
        document: document.clone(),
        revision_id: "new".into(),
    });
    assert!(check(&current, &|_, _| Ok(false)).is_err());
    check(&current, &|_, _| Ok(true)).unwrap();
}

#[test]
fn admitted_reference_requires_exact_proof_and_one_original_association() {
    let document = native::DocumentKey {
        source_set_id: "set".into(),
        language: "python".into(),
        path: "a.py".into(),
    };
    let derived = DerivedFrom {
        provenance_id: "proof".into(),
        record_kind: RecordKind::Occurrence,
        fact_digest: "f".repeat(64),
    };
    let proof = Provenance {
        id: "new".into(),
        producer_id: "producer".into(),
        document: document.clone(),
        revision_id: "revision:new".into(),
        content_hash: "a".repeat(64),
        evidence_kind: EvidenceKind::SemanticReference,
        basis: None,
        freshness: Freshness::PossiblyStale,
        derived_from: Some(derived.clone()),
    };
    let previous = Provenance {
        id: "old".into(),
        revision_id: "revision:old".into(),
        ..proof.clone()
    };
    let reference = Reference {
        id: "occ:v2:test".into(),
        owner_syntax_id: "sid:v2:test".into(),
        ordinal: 0,
        document,
        revision_id: "revision:old".into(),
        range: native::Range { start: 0, end: 1 },
        spelling: "a".into(),
        lookup_key: "a".into(),
        site: ReferenceSite::Use,
        roles: vec![Role::Read],
        resolution: Resolution::Unresolved,
        declared_target: None,
        candidates: vec![],
        provenance_id: "old".into(),
    };
    let mut original = SemanticSnapshot {
        producers: vec![],
        coverage: vec![],
        provenance: vec![previous],
        symbols: vec![],
        declaration_bindings: vec![],
        references: vec![reference],
        call_bindings: vec![],
        type_relationships: vec![],
    };
    let witness_of = |original: &SemanticSnapshot| WitnessedDocument::new(original.clone());
    let witness = witness_of(&original);
    assert_eq!(original_reference(&witness, &proof).unwrap().spelling, "a");
    original.references.push(original.references[0].clone());
    assert!(
        original_reference(&witness_of(&original), &proof)
            .unwrap_err()
            .to_string()
            .contains("fused or ambiguous")
    );
    let mut wrong = proof;
    wrong.derived_from.as_mut().unwrap().fact_digest = "0".repeat(64);
    assert!(
        original_reference(&witness, &wrong)
            .unwrap_err()
            .to_string()
            .contains("missing admitted proof")
    );
}
