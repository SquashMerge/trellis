use trellis::{
    model::{AnchorReason, AnchorStatus},
    native_evidence::{Declaration, DeclarationRole, DocumentKey, Header, Key, Range},
    store::anchors::{ContinuityState, GroupContinuity, audit_anchor, capture_anchor},
};

fn declaration(id: &str, revision: &str, ordinal: usize, start: usize, name: &str) -> Declaration {
    Declaration {
        role: DeclarationRole::Production,
        syntax_id: id.into(),
        document: DocumentKey {
            source_set_id: "source-set:v1:test".into(),
            language: "rust".into(),
            path: "src/lib.rs".into(),
        },
        revision_id: revision.into(),
        kind: "function".into(),
        name: Some(name.into()),
        lookup_key: Some(name.into()),
        ancestors: vec![],
        key: Key {
            kind: "function".into(),
            name: Some(name.into()),
            signature: None,
            ordinal,
        },
        range: Range {
            start,
            end: start + 10,
        },
        name_range: Some(Range {
            start,
            end: start + name.len(),
        }),
        header: Header {
            kind: "function".into(),
            name: Some(name.into()),
            modifiers: vec!["pub".into()],
            type_parameters: vec![],
            parameters: vec![],
            result_type: None,
            bases: vec![],
        },
        provenance_id: "provenance:v1:test".into(),
    }
}

#[test]
fn worked_audit_vectors() {
    let id = "sid:v1:0123456789abcdef0123456789abcdef";
    let old = declaration(id, "rev-old", 0, 0, "run");
    let anchor = capture_anchor(&old, std::slice::from_ref(&old)).unwrap();
    assert_eq!(
        anchor.header_hash,
        "322c20d2dc7cf143c8f2a70efc3e7031e5f802ac81ae0fd2389b868819434054"
    );
    assert_eq!(
        anchor.sibling_group_hash,
        "76757e1401a672fa2a0c84315ef4b99e8dc502df5cecdcbb6e0fb8ff8cb0067c"
    );
    let same = audit_anchor(
        &anchor,
        "rev-old",
        Some(&old),
        std::slice::from_ref(&old),
        None,
    )
    .unwrap();
    assert_eq!(same.status, AnchorStatus::Attached);
    assert_eq!(same.target_id.as_deref(), Some(id));

    let current = declaration(id, "rev-new", 0, 0, "run");
    let body_only = audit_anchor(
        &anchor,
        "rev-new",
        Some(&current),
        std::slice::from_ref(&current),
        None,
    )
    .unwrap();
    assert_eq!(
        body_only.status,
        AnchorStatus::Attached,
        "unique headers do not require group continuity"
    );

    let id2 = "sid:v1:abcdef0123456789abcdef0123456789";
    let duplicate = declaration(id2, "rev-old", 1, 20, "run");
    let old_group = vec![old.clone(), duplicate];
    let duplicate_anchor = capture_anchor(&old, &old_group).unwrap();
    let current2 = declaration(id2, "rev-new", 1, 20, "run");
    let current_group = vec![current.clone(), current2];
    let unknown = audit_anchor(
        &duplicate_anchor,
        "rev-new",
        Some(&current),
        &current_group,
        None,
    )
    .unwrap();
    assert_eq!(unknown.reason, AnchorReason::UnprovenContinuity);
    let witness = GroupContinuity {
        from_revision_id: "rev-old".into(),
        to_revision_id: "rev-new".into(),
        state: ContinuityState::Unchanged,
        evidence: Some("independent source diff witness".into()),
    };
    assert_eq!(
        audit_anchor(
            &duplicate_anchor,
            "rev-new",
            Some(&current),
            &current_group,
            Some(&witness)
        )
        .unwrap()
        .status,
        AnchorStatus::Attached
    );
}

#[test]
fn ordered_failure_precedence() {
    let id = "sid:v1:0123456789abcdef0123456789abcdef";
    let old = declaration(id, "a", 0, 0, "run");
    let anchor = capture_anchor(&old, std::slice::from_ref(&old)).unwrap();
    assert_eq!(
        audit_anchor(&anchor, "b", None, &[], None).unwrap().reason,
        AnchorReason::Missing
    );
    let mut changed = declaration(id, "b", 0, 0, "run");
    changed.header.modifiers.push("async".into());
    assert_eq!(
        audit_anchor(
            &anchor,
            "b",
            Some(&changed),
            std::slice::from_ref(&changed),
            None
        )
        .unwrap()
        .reason,
        AnchorReason::HeaderMismatch
    );
}

#[test]
fn invalid_cross_revision_witness_is_rejected() {
    let id = "sid:v1:0123456789abcdef0123456789abcdef";
    let old = declaration(id, "a", 0, 0, "run");
    let anchor = capture_anchor(&old, std::slice::from_ref(&old)).unwrap();
    let current = declaration(id, "b", 0, 0, "run");
    let invalid = GroupContinuity {
        from_revision_id: "a".into(),
        to_revision_id: "b".into(),
        state: ContinuityState::Unchanged,
        evidence: Some(" ".into()),
    };
    assert!(
        audit_anchor(
            &anchor,
            "b",
            Some(&current),
            std::slice::from_ref(&current),
            Some(&invalid)
        )
        .is_err()
    );
}

#[test]
fn generated_semantic_contract_anchors_are_the_rust_golden() {
    use trellis::{model::DurableAnchor, native_evidence::Declaration};
    let records: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/semantic-evidence/v1/example/generated/bundles/64071df52645a61825f80b45db3745ca66a65bf20f10404f18ff07dcb794a715/records.json"
    )).unwrap();
    let declarations: Vec<Declaration> =
        serde_json::from_value(records["declarations"].clone()).unwrap();
    let anchors: Vec<DurableAnchor> =
        serde_json::from_value(records["durableAnchors"].clone()).unwrap();
    for expected in anchors {
        let captured: Vec<_> = declarations
            .iter()
            .filter(|row| {
                row.revision_id == expected.captured_revision_id
                    && row.document == expected.document
            })
            .cloned()
            .collect();
        let focused = captured
            .iter()
            .find(|row| row.syntax_id == expected.syntax_id)
            .unwrap();
        assert_eq!(capture_anchor(focused, &captured).unwrap(), expected);
    }
}

#[test]
fn authored_semantic_contract_cases_execute_through_production_audit() {
    let authored: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/semantic-evidence/v1/example/expected/anchors.json"
    ))
    .unwrap();
    let capture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/semantic-evidence/v1/example/captures/native.json"
    ))
    .unwrap();
    let records: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/semantic-evidence/v1/example/generated/bundles/64071df52645a61825f80b45db3745ca66a65bf20f10404f18ff07dcb794a715/records.json"
    )).unwrap();
    let declarations: Vec<Declaration> =
        serde_json::from_value(records["declarations"].clone()).unwrap();
    let captured_rows = capture["declarations"].as_array().unwrap();
    let declaration_for_ref = |reference: &str| -> &Declaration {
        let authored_row = captured_rows
            .iter()
            .find(|row| row["ref"] == reference)
            .unwrap();
        declarations
            .iter()
            .find(|row| {
                row.revision_id == authored_row["revisionId"].as_str().unwrap()
                    && serde_json::to_value(&row.document).unwrap() == authored_row["document"]
                    && row.range.start == authored_row["range"]["start"].as_u64().unwrap() as usize
                    && row.range.end == authored_row["range"]["end"].as_u64().unwrap() as usize
            })
            .unwrap()
    };
    for case in authored["cases"].as_array().unwrap() {
        let captured = declaration_for_ref(case["capturedDeclarationRef"].as_str().unwrap());
        let captured_document: Vec<_> = declarations
            .iter()
            .filter(|row| {
                row.revision_id == captured.revision_id && row.document == captured.document
            })
            .cloned()
            .collect();
        let anchor = capture_anchor(captured, &captured_document).unwrap();
        let current_revision = case["currentRevisionId"].as_str().unwrap();
        let current_document: Vec<_> = declarations
            .iter()
            .filter(|row| row.revision_id == current_revision && row.document == captured.document)
            .cloned()
            .collect();
        let current = current_document
            .iter()
            .find(|row| row.syntax_id == anchor.syntax_id);
        let continuity: GroupContinuity =
            serde_json::from_value(case["continuity"].clone()).unwrap();
        let actual = audit_anchor(
            &anchor,
            current_revision,
            current,
            &current_document,
            Some(&continuity),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(actual.status).unwrap(),
            case["expectedResult"]["status"],
            "{}",
            case["id"]
        );
        assert_eq!(
            serde_json::to_value(actual.reason).unwrap(),
            case["expectedResult"]["reason"],
            "{}",
            case["id"]
        );
        let expected_target = match &case["expectedResult"]["targetId"] {
            serde_json::Value::Null => None,
            value => Some(
                declaration_for_ref(value["ref"].as_str().unwrap())
                    .syntax_id
                    .clone(),
            ),
        };
        assert_eq!(actual.target_id, expected_target, "{}", case["id"]);
    }
}

#[test]
fn full_ordered_group_vectors_and_golden_fixture() {
    let id = "sid:v1:0123456789abcdef0123456789abcdef";
    let sibling_id = "sid:v1:abcdef0123456789abcdef0123456789";
    let old = declaration(id, "r1", 0, 10, "run");
    let old_sibling = declaration(sibling_id, "r1", 1, 30, "run");
    let duplicate_anchor = capture_anchor(&old, &[old.clone(), old_sibling]).unwrap();

    // Duplicate same-revision inventories attach without an external witness.
    assert_eq!(
        audit_anchor(
            &duplicate_anchor,
            "r1",
            Some(&old),
            &[old.clone(), declaration(sibling_id, "r1", 1, 30, "run")],
            None
        )
        .unwrap()
        .status,
        AnchorStatus::Attached
    );
    let contradictory_same_revision = GroupContinuity {
        from_revision_id: "r1".into(),
        to_revision_id: "r1".into(),
        state: ContinuityState::Changed,
        evidence: None,
    };
    assert_eq!(
        audit_anchor(
            &duplicate_anchor,
            "r1",
            Some(&old),
            &[old.clone(), declaration(sibling_id, "r1", 1, 30, "run")],
            Some(&contradictory_same_revision)
        )
        .unwrap()
        .status,
        AnchorStatus::Attached
    );

    // An earlier different-header insertion takes the original ordinal ID. Audit must
    // reject that exact ID and never follow the shifted old declaration.
    let mut inserted_different = declaration(id, "r2", 0, 0, "run");
    inserted_different.header.modifiers.push("async".into());
    let shifted_original = declaration(sibling_id, "r2", 1, 30, "run");
    let result = audit_anchor(
        &duplicate_anchor,
        "r2",
        Some(&inserted_different),
        &[inserted_different.clone(), shifted_original],
        None,
    )
    .unwrap();
    assert_eq!(result.reason, AnchorReason::HeaderMismatch);
    assert_eq!(result.target_id, None);

    // Earlier identical insertion changes the count/group and wins over unknown continuity.
    let inserted_id = "sid:v1:11111111111111111111111111111111";
    let shifted = declaration(id, "r2", 1, 30, "run");
    let inserted = declaration(inserted_id, "r2", 0, 10, "run");
    let tail = declaration(sibling_id, "r2", 2, 50, "run");
    assert_eq!(
        audit_anchor(
            &duplicate_anchor,
            "r2",
            Some(&shifted),
            &[inserted, shifted.clone(), tail],
            None
        )
        .unwrap()
        .reason,
        AnchorReason::GroupChanged
    );

    // A changed header wins even when the sibling group also changed.
    let mut changed_header = declaration(id, "r2", 0, 10, "run");
    changed_header.header.result_type = Some("String".into());
    assert_eq!(
        audit_anchor(
            &duplicate_anchor,
            "r2",
            Some(&changed_header),
            std::slice::from_ref(&changed_header),
            None
        )
        .unwrap()
        .reason,
        AnchorReason::HeaderMismatch
    );

    // Equal duplicate inventory under a different revision remains unproven.
    let current = declaration(id, "r2", 0, 10, "run");
    let current_sibling = declaration(sibling_id, "r2", 1, 30, "run");
    assert_eq!(
        audit_anchor(
            &duplicate_anchor,
            "r2",
            Some(&current),
            &[current.clone(), current_sibling],
            None
        )
        .unwrap()
        .reason,
        AnchorReason::UnprovenContinuity
    );

    // Unique declarations attach even when unrelated sibling membership changes.
    let unique = declaration(id, "r1", 0, 10, "unique");
    let unique_anchor = capture_anchor(&unique, std::slice::from_ref(&unique)).unwrap();
    let current_unique = declaration(id, "r2", 0, 10, "unique");
    let mut other = declaration(sibling_id, "r2", 1, 30, "unique");
    other.header.result_type = Some("String".into());
    assert_eq!(
        audit_anchor(
            &unique_anchor,
            "r2",
            Some(&current_unique),
            &[current_unique.clone(), other],
            None
        )
        .unwrap()
        .status,
        AnchorStatus::Attached
    );

    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/semantic-evidence/v1/example/expected/anchors.json"
    ))
    .unwrap();
    let authored: Vec<_> = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            (
                case["id"].as_str().unwrap(),
                case["expectedResult"]["reason"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        authored,
        vec![
            ("unique-unchanged-header-changed-sibling", "none"),
            ("duplicate-proven", "none"),
            ("duplicate-unknown", "unprovenContinuity"),
            ("duplicate-changed", "groupChanged"),
            ("missing", "missing"),
            ("header-mismatch", "headerMismatch"),
        ]
    );
}

#[test]
fn invalid_document_revision_and_continuity_associations_fail_closed() {
    let id = "sid:v1:0123456789abcdef0123456789abcdef";
    let old = declaration(id, "r1", 0, 0, "run");
    let anchor = capture_anchor(&old, std::slice::from_ref(&old)).unwrap();
    let mut wrong_document = declaration(id, "r2", 0, 0, "run");
    wrong_document.document.path = "other.rs".into();
    assert!(
        audit_anchor(
            &anchor,
            "r2",
            Some(&wrong_document),
            std::slice::from_ref(&wrong_document),
            None
        )
        .is_err()
    );
    let wrong_revision = declaration(id, "r3", 0, 0, "run");
    assert!(
        audit_anchor(
            &anchor,
            "r2",
            Some(&wrong_revision),
            std::slice::from_ref(&wrong_revision),
            None
        )
        .is_err()
    );
    let wrong_endpoints = GroupContinuity {
        from_revision_id: "wrong".into(),
        to_revision_id: "r2".into(),
        state: ContinuityState::Unknown,
        evidence: None,
    };
    let current = declaration(id, "r2", 0, 0, "run");
    assert!(
        audit_anchor(
            &anchor,
            "r2",
            Some(&current),
            std::slice::from_ref(&current),
            Some(&wrong_endpoints)
        )
        .is_err()
    );
}

#[cfg(feature = "test-support")]
#[test]
fn capture_from_native_snapshot_includes_unnamed_and_ignores_live_source_edits() {
    use std::{
        collections::BTreeMap,
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        indexer::{IndexOptions, index_workspace_bundle},
        model::{Position, SavedView, ViewQuery},
        store::Store,
    };
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("anchors.js");
    fs::write(
        &source,
        "const f = () => 1; function named() { return 1; }\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(work.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let unnamed = native
        .declarations
        .iter()
        .find(|row| row.name.is_none())
        .expect("fixture must expose an unnamed native declaration")
        .clone();
    let leader = store.publication_for_tests().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let view = SavedView {
        id: "unnamed".into(),
        title: "Unnamed".into(),
        query: ViewQuery {
            seed: unnamed.syntax_id.clone(),
            depth: 1,
            max_nodes: 40,
            max_calls: 200,
            include_callbacks: false,
            exclude_paths: vec![],
        },
        pins: BTreeMap::<String, Position>::new(),
        hidden: vec![],
    };
    let saved = store.save_view_at(pin, &view).unwrap();
    let raw_before = saved.view.anchor.as_ref().unwrap().get().to_owned();
    assert_eq!(
        saved.attachment.result.as_ref().unwrap().status,
        AnchorStatus::Attached
    );
    fs::write(&source, "this is deliberately not indexed\n").unwrap();
    let reread = store.saved_view_at("unnamed", Some(pin)).unwrap().unwrap();
    assert_eq!(reread.view.anchor.as_ref().unwrap().get(), raw_before);
    assert_eq!(
        reread.attachment.result.unwrap().status,
        AnchorStatus::Attached
    );
}

#[cfg(feature = "test-support")]
#[test]
fn production_duplicate_cross_revision_is_never_inferred_unchanged() {
    use std::{
        collections::BTreeMap,
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        indexer::{IndexOptions, index_workspace_bundle},
        model::{AnnotationRequest, SavedView, ViewQuery},
        store::Store,
    };
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("duplicates.js");
    fs::write(
        &source,
        "function same() { return 1; }\nfunction same() { return 1; }\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let build = |store: &Store| {
        index_workspace_bundle(
            &IndexOptions::new(work.path().to_owned()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap()
    };
    let (graph, native, capture) = build(&store);
    let selected = native
        .declarations
        .iter()
        .find(|row| row.name.as_deref() == Some("same") && row.key.ordinal == 0)
        .unwrap()
        .clone();
    let leader = store.publication_for_tests().unwrap();
    let first = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let view = SavedView {
        id: "duplicate".into(),
        title: "Duplicate".into(),
        query: ViewQuery {
            seed: selected.syntax_id.clone(),
            depth: 1,
            max_nodes: 40,
            max_calls: 200,
            include_callbacks: false,
            exclude_paths: vec![],
        },
        pins: BTreeMap::new(),
        hidden: vec![],
    };
    store.save_view_at(first, &view).unwrap();
    let note = store
        .save_annotation_at(
            first,
            &AnnotationRequest {
                id: "duplicate-note".into(),
                node_id: selected.syntax_id.clone(),
                body: "note".into(),
                title: Some("Title".into()),
            },
        )
        .unwrap();
    let note_raw = note.annotation.anchor.as_ref().unwrap().get().to_owned();
    fs::write(
        &source,
        "function same() { return 2; }\nfunction same() { return 1; }\n",
    )
    .unwrap();
    let (graph, native, capture) = build(&store);
    let second = store
        .publish_native(&graph, &capture, &native, &leader, first, &cancel)
        .unwrap();
    let reread = store
        .saved_view_at("duplicate", Some(second))
        .unwrap()
        .unwrap();
    assert_eq!(
        reread.attachment.result.unwrap().reason,
        AnchorReason::UnprovenContinuity
    );
    let note = store.saved_annotations_at(Some(second)).unwrap().remove(0);
    assert_eq!(
        note.attachment.result.unwrap().reason,
        AnchorReason::UnprovenContinuity
    );
    assert_eq!(note.annotation.anchor.unwrap().get(), note_raw);
}
