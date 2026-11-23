use super::*;
use parallel_tests::snapshot_of;
use shared::{SemanticChargeOutcome, StructuralCharge};

fn fixture() -> Snapshot {
    snapshot_of(&[
        (
            "src/lib.rs",
            "pub mod a; pub mod b;\npub fn run() { crate::a::f(); crate::a::f(); crate::a::f(); }\n",
        ),
        ("src/a.rs", "pub fn f() {}\n"),
        ("src/b.rs", "pub fn g() {}\n"),
        ("idle.py", ""),
        ("absent.py", ""),
    ])
}

fn document(snapshot: &Snapshot, path: &str) -> usize {
    snapshot
        .documents
        .iter()
        .position(|doc| doc.key.path == path)
        .unwrap()
}

fn anchor(snapshot: &Snapshot, outcome: SemanticOutcome) -> SemanticAnchor {
    let doc = &snapshot.documents[document(snapshot, "src/lib.rs")];
    let call = &doc.calls[0];
    SemanticAnchor {
        range: call.callee_range.clone().unwrap_or(call.range.clone()),
        call_id: Some(call.id.clone()),
        type_role: false,
        direct: true,
        outcome,
        basis_site: true,
        provenance_id: "proof".into(),
        metadata: SemanticMetadata::default(),
    }
}

fn target(snapshot: &Snapshot, path: &str, name: &str) -> SemanticOutcome {
    let index = document(snapshot, path);
    let declaration = snapshot.documents[index]
        .declarations
        .iter()
        .find(|declaration| declaration.name.as_deref() == Some(name))
        .unwrap();
    SemanticOutcome::Internal(index, declaration.syntax_id.clone())
}

/// Run a single prepared site through the real full source completion. Preparation Work
/// is deliberately zero here, so target-admission charges can be asserted independently.
fn single(snapshot: &Snapshot, raw: bool, ambiguous: bool) -> CompletedFile {
    let mut check = || Ok(());
    let request = parallel_check::RequestCheck::new(&mut check);
    let check = &mut request.worker();
    let admitted = admit_snapshot(snapshot, check).unwrap();
    let trees = admitted
        .docs
        .iter()
        .map(|doc| parse(doc, check).unwrap())
        .collect::<Vec<_>>();
    let selected = (0..admitted.docs.len()).collect::<Vec<_>>();
    let read = ReadContext {
        snapshot,
        docs: &admitted.docs,
        trees: &trees,
        document_map: &admitted.document_map,
        selected: &selected,
        sample_limit: 1,
        workers: 1,
        request: &request,
    };
    let source = admitted
        .docs
        .iter()
        .position(|doc| doc.key.path == "src/lib.rs")
        .unwrap();
    let mut catalogs = build_catalogs(read, 1_000_000, check).unwrap();
    let (declared, _) =
        declared_targets(read.docs, &mut catalogs.rust_execution.work, check).unwrap();
    let locate = |document, sid: &str| {
        admitted
            .sorted
            .get(document)
            .copied()
            .filter(|index| declared.contains(&(*index, sid)))
    };
    let prepared = prepare_file(
        source,
        admitted.docs[source],
        &trees[source],
        1_000_000,
        check,
    )
    .unwrap();
    let mut site = prepared
        .sites
        .into_iter()
        .find(|site| site.site.spelling == "crate::a::f")
        .unwrap();
    if !raw {
        site.raw = None;
    }
    if ambiguous {
        site.site.owner_syntax_id = None;
    }
    site.anchor = (!admitted.docs[source].semantic.anchors.is_empty()).then_some(0);
    file_references(
        read,
        &mut catalogs,
        PreparedFile {
            source,
            work: Work::new(1_000_000),
            sites: vec![site],
        },
        &locate,
        &mut 0,
        &mut BTreeMap::new(),
        check,
    )
    .unwrap()
}

#[test]
fn full_source_facts_match_actual_work_sites_and_raw_requests() {
    let mut snapshot = fixture();
    snapshot.documents.reverse();
    let absent = snapshot.documents[document(&snapshot, "absent.py")]
        .key
        .clone();
    snapshot.evidence_gaps.insert(absent, "fixture_gap");
    let base = {
        let mut check = || Ok(());
        let request = parallel_check::RequestCheck::new(&mut check);
        resolve(&snapshot, Sources::all(), 1, 1, &request).unwrap()
    };
    let mut check = || Ok(());
    let request = parallel_check::RequestCheck::new(&mut check);
    let check = &mut request.worker();
    let admitted = admit_snapshot(&snapshot, check).unwrap();
    let trees = admitted
        .docs
        .iter()
        .map(|doc| parse(doc, check).unwrap())
        .collect::<Vec<_>>();
    let selected = admitted
        .docs
        .iter()
        .enumerate()
        .filter_map(|(i, doc)| (!snapshot.evidence_gaps.contains_key(&doc.key)).then_some(i))
        .collect::<Vec<_>>();
    let read = ReadContext {
        snapshot: &snapshot,
        docs: &admitted.docs,
        trees: &trees,
        document_map: &admitted.document_map,
        selected: &selected,
        sample_limit: 1,
        workers: 1,
        request: &request,
    };
    let limit = work::reference_limit(read.docs.len(), admitted.total_bytes);
    let mut catalogs = build_catalogs(read, limit, check).unwrap();
    declared_targets(read.docs, &mut catalogs.rust_execution.work, check).unwrap();
    assert_eq!(
        base.work_used,
        catalogs.rust_execution.work.used
            + base
                .source_facts()
                .iter()
                .map(|facts| facts.work)
                .sum::<usize>()
    );
    for (index, facts) in base.source_facts().iter().enumerate() {
        let doc = admitted.docs[index];
        if doc.key.path == "absent.py" {
            assert!(facts.plan.is_none());
            assert_eq!((facts.work, facts.sites, facts.endpoints.len()), (0, 0, 0));
            continue;
        }
        let prepared = prepare_file(index, doc, &trees[index], limit, check).unwrap();
        let plan = facts.plan.as_ref().unwrap();
        assert_eq!(plan.prepared_work, prepared.work.used);
        assert_eq!(plan.sites.len(), prepared.sites.len());
        for (recorded, original) in plan.sites.iter().zip(prepared.sites) {
            match (&recorded.structural, original.raw) {
                (StructuralCharge::Resolve(raw), Some(original)) => {
                    assert_eq!(json!(raw), json!(original))
                }
                (StructuralCharge::None, None) => {}
                _ => panic!("unexpected full structural guard"),
            }
        }
        let source = base.sources[index].as_ref().unwrap();
        assert_eq!(
            facts.sites,
            source
                .coverage
                .values()
                .map(|coverage| coverage.sites)
                .sum::<usize>()
        );
        assert!(facts.work >= plan.prepared_work);
        assert!(facts.endpoints.windows(2).all(|pair| pair[0] < pair[1]));
    }
    let source = admitted
        .docs
        .iter()
        .position(|doc| doc.key.path == "src/lib.rs")
        .unwrap();
    let a = admitted
        .docs
        .iter()
        .position(|doc| doc.key.path == "src/a.rs")
        .unwrap() as u32;
    assert!(base.source_facts()[source].endpoints.contains(&a));
    let sampled = base.sources[source]
        .as_ref()
        .unwrap()
        .edges
        .values()
        .map(|edge| edge.calls.len() + edge.others.len())
        .sum::<usize>();
    assert!(base.source_facts()[source].sites > sampled);
}

#[test]
fn full_charge_plan_records_all_semantic_outcomes_and_structural_guards() {
    for outcome in [
        SemanticOutcome::Boundary,
        SemanticOutcome::External,
        SemanticOutcome::Ambiguous,
        SemanticOutcome::Unresolved,
    ] {
        for historical in [false, true] {
            let mut snapshot = fixture();
            let mut anchor = anchor(&snapshot, outcome.clone());
            if historical {
                anchor.metadata.insert("projection".into(), json!({}));
            }
            let source = document(&snapshot, "src/lib.rs");
            snapshot.documents[source].semantic.anchors = vec![anchor];
            let completed = single(&snapshot, true, false);
            let plan = completed.facts.plan.unwrap();
            assert_eq!(
                json!(plan.sites[0].semantic.as_ref().unwrap().historical),
                json!(historical)
            );
            assert_eq!(completed.facts.sites, 1);
            assert!(matches!(
                plan.sites[0].structural,
                StructuralCharge::Resolve(_)
            ));
            let expected = match outcome {
                SemanticOutcome::Boundary => json!("Boundary"),
                SemanticOutcome::External => json!("External"),
                SemanticOutcome::Ambiguous => json!("Ambiguous"),
                SemanticOutcome::Unresolved => json!("Unresolved"),
                _ => unreachable!(),
            };
            assert_eq!(
                json!(plan.sites[0].semantic.as_ref().unwrap().outcome),
                expected
            );
            let semantic_only = single(&snapshot, false, false);
            let accepted = historical
                || matches!(
                    outcome,
                    SemanticOutcome::Boundary | SemanticOutcome::External
                );
            assert_eq!(semantic_only.facts.sites, usize::from(accepted));
            assert_eq!(semantic_only.facts.work, 0);
        }
    }
    let mut snapshot = fixture();
    let source = document(&snapshot, "src/lib.rs");
    let mut binding = anchor(&snapshot, target(&snapshot, "src/b.rs", "g"));
    snapshot.documents[source].semantic.producer_id = Some("producer".into());
    snapshot.documents[source].semantic.anchors = vec![binding.clone()];
    let completed = single(&snapshot, false, false);
    assert_eq!(
        (completed.facts.work, completed.facts.sites),
        (2, 1),
        "one internal lookup and one effective target"
    );
    let plan = completed.facts.plan.unwrap();
    assert!(matches!(plan.sites[0].structural, StructuralCharge::None));
    let SemanticChargeOutcome::Internal { document, sid } =
        &plan.sites[0].semantic.as_ref().unwrap().outcome
    else {
        panic!("internal plan")
    };
    assert_eq!(
        *document as usize,
        completed
            .found
            .edges
            .keys()
            .find_map(|(target, _, _)| match target {
                Target::Native(index) => Some(*index),
                _ => None,
            })
            .unwrap()
    );
    assert!(!sid.is_empty());

    let full = single(&snapshot, true, false);
    let sorted_a = snapshot
        .documents
        .iter()
        .map(|doc| &doc.key)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .position(|key| key.path == "src/a.rs")
        .unwrap() as u32;
    assert!(
        full.facts.endpoints.contains(&sorted_a),
        "structural endpoint survives semantic override"
    );
    assert!(full.facts.endpoints.contains(document));
    binding.outcome = target(&snapshot, "src/lib.rs", "run");
    snapshot.documents[source].semantic.anchors = vec![binding];
    let skipped = single(&snapshot, false, false);
    assert_eq!(
        (skipped.facts.work, skipped.facts.sites),
        (0, 0),
        "semantic-only self site is skipped"
    );
    assert_eq!(
        skipped.facts.plan.unwrap().sites.len(),
        1,
        "skipped requests remain in the recipe"
    );
    snapshot.documents[source].semantic.anchors.clear();
    let absent = single(&snapshot, false, false);
    assert_eq!(absent.facts.sites, 0);
    let ambiguous = single(&snapshot, true, true);
    assert!(matches!(
        ambiguous.facts.plan.unwrap().sites[0].structural,
        StructuralCharge::OwnerAmbiguous
    ));
    assert_eq!(ambiguous.found.unresolved[0].1, "reference_owner_ambiguous");
    let mut historical = anchor(&snapshot, SemanticOutcome::External);
    historical.metadata.insert("projection".into(), json!({}));
    snapshot.documents[source].semantic.anchors = vec![historical];
    snapshot.documents[source].semantic.projection = Some(crate::model::SemanticProjection {
        current_pin: snapshot.pin,
        semantic_basis_pin: snapshot.pin,
        basis_native_revision_id: "basis".into(),
        semantic_status: crate::model::SemanticStatus::OlderBasis,
    });
    let suppressed = single(&snapshot, true, true);
    assert!(
        matches!(
            suppressed.facts.plan.unwrap().sites[0].structural,
            StructuralCharge::None
        ),
        "historical suppression precedes owner ambiguity"
    );
}

#[test]
fn complete_semantic_endpoints_precede_sampling_skips_and_boundary_elimination() {
    let mut snapshot = fixture();
    snapshot.documents.reverse();
    let source = document(&snapshot, "src/lib.rs");
    let a = document(&snapshot, "src/a.rs");
    let b = document(&snapshot, "src/b.rs");
    let mut binding = anchor(&snapshot, SemanticOutcome::Boundary);
    binding.metadata.insert(
        "historicalTargets".into(),
        json!([
            {"kind":"internal","language":"rust","path":"src/a.rs"},
            {"kind":"external","language":"rust","path":"idle.py"}
        ]),
    );
    binding.metadata.insert(
        "historicalExternalTargets".into(),
        json!([
            {"language":"rust","path":"src/lib.rs"}
        ]),
    );
    snapshot.documents[source].semantic.anchors = vec![binding];
    snapshot.documents[source]
        .semantic
        .endpoint_documents
        .insert(b);
    snapshot.documents[source].semantic.relationships = vec![SemanticRelationship {
        kind: "implements".into(),
        source_syntax_id: "source".into(),
        target: Some((a, "target".into())),
        provenance_id: "relationship".into(),
        metadata: SemanticMetadata::default(),
    }];
    let base = resolve_all(&snapshot, &mut || Ok(())).unwrap();
    let sorted = |path: &str| {
        base.identities
            .iter()
            .position(|identity| identity.path == path)
            .unwrap() as u32
    };
    let facts = &base.source_facts()[sorted("src/lib.rs") as usize];
    for path in ["src/lib.rs", "src/a.rs", "src/b.rs"] {
        assert!(facts.endpoints.contains(&sorted(path)), "{path}");
    }
    assert!(!facts.endpoints.contains(&sorted("idle.py")));
    let internal = anchor(&snapshot, target(&snapshot, "src/b.rs", "g"));
    snapshot.documents[source].semantic.anchors = vec![internal];
    let base = resolve_all(&snapshot, &mut || Ok(())).unwrap();
    let plan = base.source_facts()[sorted("src/lib.rs") as usize]
        .plan
        .as_ref()
        .unwrap();
    assert!(plan.sites.iter().any(|site| matches!(
        site.semantic.as_ref().map(|semantic| &semantic.outcome),
        Some(SemanticChargeOutcome::Internal { document, .. }) if *document == sorted("src/b.rs")
    )));
}
