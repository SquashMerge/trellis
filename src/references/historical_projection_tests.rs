use super::*;

fn anchor(outcome: SemanticOutcome) -> SemanticAnchor {
    SemanticAnchor {
        range: crate::native_evidence::Range { start: 0, end: 1 },
        call_id: None,
        type_role: true,
        direct: false,
        outcome,
        basis_site: false,
        provenance_id: "proof".into(),
        metadata: BTreeMap::from([("projection".into(), json!({"semanticStatus":"olderBasis"}))]),
    }
}

#[test]
fn historical_unknown_and_external_claims_never_use_current_structural_targets() {
    let structural = SiteResolution {
        targets: BTreeSet::from([Target::Native(4)]),
        ..SiteResolution::default()
    };
    for outcome in [
        SemanticOutcome::Unresolved,
        SemanticOutcome::Ambiguous,
        SemanticOutcome::External,
        SemanticOutcome::Boundary,
    ] {
        let result =
            semantic_resolution(&anchor(outcome), Some(&structural), &|_, _| Some(4)).unwrap();
        assert!(result.targets.is_empty());
        assert!(result.residual.is_some());
    }
}

#[test]
fn historical_internal_target_missing_from_native_is_a_boundary() {
    let result = semantic_resolution(
        &anchor(SemanticOutcome::Internal(1, "target".into())),
        None,
        &|_, _| None,
    )
    .unwrap();
    assert!(result.targets.is_empty());
    assert_eq!(result.residual, Some("semantic_endpoint_catching_up"));
}
