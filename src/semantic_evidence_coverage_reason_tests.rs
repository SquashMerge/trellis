use super::*;

fn partial(diagnostic: &str, reasons: CoverageReasons) -> Coverage {
    Coverage {
        producer_id: "p".into(),
        language: Language::Rust,
        source_set_id: "s".into(),
        document_path: "a.rs".into(),
        revision_id: "r".into(),
        requested: true,
        selected: true,
        state: CoverageState::Partial,
        supported_roles: vec![],
        observed_roles: vec![],
        diagnostic: Some(diagnostic.into()),
        reasons: Some(reasons),
    }
}

#[test]
fn gap_kind_names_the_skipped_construct() {
    let kind = |text: &str| CoverageReasons::gap_kind(Some(text));
    assert_eq!(kind("opaque Rust macro_invocation skipped; contents"), 0);
    assert_eq!(kind("opaque Rust format string skipped; implicit"), 0);
    assert_eq!(kind("opaque Rust named format argument skipped; names"), 0);
    assert_eq!(kind("opaque Rust attribute_item skipped; contents"), 1);
    assert_eq!(
        kind("opaque Rust inner_attribute_item skipped; contents"),
        1
    );
    assert_eq!(kind("opaque Rust macro_definition skipped; contents"), 2);
    assert_eq!(kind("opaque Rust token_tree skipped; contents"), 3);
    assert_eq!(kind("family unavailable"), 3);
    assert_eq!(CoverageReasons::gap_kind(None), 3);
}

#[test]
fn language_order_is_contract_order() {
    for (index, name) in ["java", "rust", "python", "javascript", "typescript"]
        .into_iter()
        .enumerate()
    {
        assert_eq!(language_order(name), Some(index));
        assert_eq!(LANGUAGE_ORDER[index], name);
    }
    assert_eq!(language_order("TypeScript"), None);
}

#[test]
fn reasons_must_agree_with_state_and_diagnostic() {
    let gap = CoverageReasons::new([1, 0, 0, 0], true, false, false);
    let cap = CoverageReasons::new([0; 4], true, false, false);
    let omitted = CoverageReasons::new([0; 4], false, true, false);
    let none = CoverageReasons::new([0; 4], false, false, false);
    let failed = Coverage {
        state: CoverageState::Failed,
        ..partial("unmeasurable_source_anchor", gap.clone())
    };
    for (row, ok) in [
        (partial("unsupported_native_gap", gap.clone()), true),
        (partial("occurrence_cap_exceeded", cap.clone()), true),
        (partial("unmeasurable_internal_target", omitted), true),
        (partial("occurrence_cap_exceeded", gap.clone()), false),
        (partial("document_cap_exceeded", cap.clone()), true),
        (partial("document_cap_exceeded", gap), false),
        (partial("unsupported_native_gap", cap.clone()), false),
        (partial("unmeasurable_internal_target", cap), false),
        (partial("unsupported_native_gap", none), false),
        (failed, false),
    ] {
        let reasons = row.reasons.clone().unwrap();
        assert_eq!(
            validate_coverage_reasons(&row, &reasons).is_ok(),
            ok,
            "{row:?}"
        );
    }
}
