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

fn full(snapshot: &Snapshot, workers: usize) -> ReferenceBase {
    let mut check = || Ok(());
    let request = parallel_check::RequestCheck::new(&mut check);
    request
        .result(resolve(
            snapshot,
            Sources::all(),
            MAX_SAMPLE_LIMIT,
            workers,
            &request,
        ))
        .unwrap()
}

fn replay(
    snapshot: &Snapshot,
    base: &ReferenceBase,
    dirty: BTreeSet<DocumentKey>,
    patched: BTreeSet<DocumentKey>,
    workers: usize,
) -> incremental::IncrementalReferences {
    let preparation = incremental::prepare_with_workers(snapshot, workers, &mut || Ok(())).unwrap();
    incremental::finish_incremental(
        snapshot,
        preparation,
        base,
        &dirty,
        &patched,
        &mut || Ok(()),
    )
    .unwrap()
}

#[test]
fn catalog_work_matches_full() {
    let snapshot = fixture();
    let base = full(&snapshot, 2);
    let result = replay(&snapshot, &base, BTreeSet::new(), BTreeSet::new(), 2);
    let q: usize = base.source_facts.iter().map(|source| source.work).sum();
    assert_eq!(result.trace.catalog_work, base.work_used - q);
    assert_eq!(
        result.trace.logical_work, base.work_used,
        "replayed logical Work"
    );
    let preparation: usize = base
        .source_facts
        .iter()
        .filter_map(|source| source.plan.as_ref())
        .map(|plan| plan.prepared_work)
        .sum();
    assert_eq!(result.trace.executed_work, base.work_used - preparation);
    assert_eq!(
        result.references.canonical().unwrap(),
        base.canonical().unwrap()
    );
    assert!(result.trace.file_references.is_empty());
    for i in 0..base.sources.len() {
        assert!(result.references.source_shared(i, &base, i));
    }
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
    let completed = file_references(
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
    .unwrap();
    let mut fresh = build_catalogs(read, 1_000_000, check).unwrap();
    let (declared, _) = declared_targets(read.docs, &mut fresh.rust_execution.work, check).unwrap();
    let sorted_locate =
        |document, sid: &str| declared.contains(&(document, sid)).then_some(document);
    let before = fresh.rust_execution.work.used;
    let (packages, sites) = incremental::replay_source_charges(
        read,
        &mut fresh,
        source,
        completed.facts.plan.as_ref().unwrap(),
        &sorted_locate,
        check,
    )
    .unwrap();
    assert_eq!(
        fresh.rust_execution.work.used - before,
        completed.facts.work,
        "charge-plan Work"
    );
    assert_eq!(sites, completed.facts.sites, "charge-plan skips");
    assert_eq!(
        json!(packages.values().collect::<Vec<_>>()),
        json!(completed.packages.values().collect::<Vec<_>>()),
        "source-local descriptors"
    );
    completed
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
fn charge_plan_shapes() {
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

fn copy_snapshot(snapshot: &Snapshot) -> Snapshot {
    Snapshot {
        pin: snapshot.pin,
        documents: snapshot
            .documents
            .iter()
            .map(|doc| DocumentInput {
                key: doc.key.clone(),
                file: doc.file.clone(),
                declarations: doc.declarations.clone(),
                calls: doc.calls.clone(),
                semantic: doc.semantic.clone(),
            })
            .collect(),
        manifests: snapshot.manifests.clone(),
        root_tsconfig: snapshot.root_tsconfig.clone(),
        evidence_gaps: snapshot.evidence_gaps.clone(),
        unparsed: snapshot.unparsed.clone(),
    }
}

#[test]
fn complete_locate() {
    let mut snapshot = fixture();
    snapshot.documents.reverse();
    let base = full(&snapshot, 2);
    let mut snapshot = copy_snapshot(&snapshot);
    let source = document(&snapshot, "src/lib.rs");
    let binding = anchor(&snapshot, target(&snapshot, "src/b.rs", "g"));
    snapshot.documents[source].semantic.producer_id = Some("producer".into());
    snapshot.documents[source].semantic.anchors = vec![binding];
    let dirty = BTreeSet::from([snapshot.documents[source].key.clone()]);
    let result = replay(&snapshot, &base, dirty, BTreeSet::new(), 2);
    let oracle = full(&snapshot, 2);
    assert_eq!(
        result.references.canonical().unwrap(),
        oracle.canonical().unwrap()
    );
    assert_eq!(
        result.trace.file_references,
        vec![snapshot.documents[source].key.clone()]
    );
    assert!(
        result
            .references
            .sources
            .iter()
            .flatten()
            .flat_map(|source| source.edges.values())
            .flat_map(|edge| &edge.calls)
            .any(|site| site
                .target
                .as_ref()
                .is_some_and(|target| target.document.path == "src/b.rs"))
    );
}

#[test]
fn batch_remaining_matches_full() {
    let mut snapshot = fixture();
    let base = full(&snapshot, 2);
    let baseline = replay(&snapshot, &base, BTreeSet::new(), BTreeSet::new(), 2);
    let source = document(&snapshot, "src/a.rs");
    snapshot.documents[source]
        .file
        .text
        .insert_str(0, "// changed preparation\n");
    for declaration in &mut snapshot.documents[source].declarations {
        if declaration.kind != "module" || !declaration.ancestors.is_empty() {
            declaration.range.start += "// changed preparation\n".len();
        }
        declaration.range.end += "// changed preparation\n".len();
        if let Some(range) = &mut declaration.name_range {
            range.start += "// changed preparation\n".len();
            range.end += "// changed preparation\n".len();
        }
    }
    let dirty = BTreeSet::from([snapshot.documents[source].key.clone()]);
    let result = replay(&snapshot, &base, dirty, BTreeSet::new(), 2);
    let oracle = full(&snapshot, 2);
    assert_eq!(
        result.references.canonical().unwrap(),
        oracle.canonical().unwrap()
    );
    assert!(result.trace.batches.len() >= 2);
    assert_ne!(
        result.trace.batches.last().unwrap().1,
        baseline.trace.batches.last().unwrap().1,
        "the later full window observes the current early work"
    );
    let limit = work::reference_limit(
        snapshot.documents.len(),
        work::snapshot_bytes(&snapshot, &mut || Ok(())).unwrap(),
    );
    let mut remaining = limit - result.trace.catalog_work;
    let mut sources = oracle
        .source_facts
        .iter()
        .enumerate()
        .filter(|(_, fact)| fact.plan.is_some());
    for (keys, actual) in &result.trace.batches {
        assert_eq!(*actual, remaining, "original full batch remainder");
        for key in keys {
            let (index, source) = sources.next().unwrap();
            assert_eq!(oracle.identities[index].path, key.path);
            assert!(source.plan.as_ref().unwrap().prepared_work <= *actual);
            remaining -= source.work;
        }
    }
    assert!(sources.next().is_none());
    assert_eq!(remaining, limit - oracle.work_used);

    let mut invalid = full(&snapshot, 2);
    let clean = invalid
        .source_facts
        .iter_mut()
        .find(|source| source.plan.is_some())
        .unwrap();
    Arc::make_mut(clean.plan.as_mut().unwrap()).prepared_work = limit + 1;
    let prepared = incremental::prepare_with_workers(&snapshot, 2, &mut || Ok(())).unwrap();
    let error = incremental::finish_incremental(
        &snapshot,
        prepared,
        &invalid,
        &BTreeSet::new(),
        &BTreeSet::new(),
        &mut || Ok(()),
    )
    .err()
    .unwrap();
    assert_eq!(error.to_string(), "incremental preparation does not fit");
}

#[test]
fn language_replay_matches_full() {
    for (path, text) in [
        (
            "Use.java",
            "import java.util.List; public class Use { List<String> xs; }\n",
        ),
        ("use.py", "import json\njson.dumps({})\n"),
        (
            "use.js",
            "import value from 'dep'; import fs from 'node:fs'; value(); fs.readFile('x');\n",
        ),
        (
            "use.ts",
            "import value from 'dep'; import fs from 'node:fs'; value(); fs.readFile('x');\n",
        ),
    ] {
        let mut snapshot = snapshot_of(&[(path, text), ("src/lib.rs", "pub fn idle() {}\n")]);
        snapshot.manifests.push(external::ManifestInput {
            source_set_id: snapshot.documents[0].key.source_set_id.clone(),
            path: "package.json".into(),
            content_hash: Some("same captured bytes".into()),
            bytes: Some(
                br#"{"dependencies":{"dep":"1.0.0"},"devDependencies":{"dep":"1.0.0"}}"#.to_vec(),
            ),
            native_revision_id: "N".into(),
            dependency_hash: "D".into(),
        });
        let base = full(&snapshot, 2);
        let frozen = base.canonical().unwrap();
        snapshot.manifests[0].native_revision_id = "longer current native revision".into();
        snapshot.manifests[0].dependency_hash = "longer current dependency provenance".into();
        let result = replay(&snapshot, &base, BTreeSet::new(), BTreeSet::new(), 2);
        let oracle = full(&snapshot, 2);
        assert_eq!(
            result.references.canonical().unwrap(),
            oracle.canonical().unwrap(),
            "{path}"
        );
        let index = oracle
            .identities
            .iter()
            .position(|identity| identity.path == path)
            .unwrap();
        assert!(
            !result.references.sources[index]
                .as_ref()
                .unwrap()
                .packages
                .is_empty(),
            "{path}"
        );
        assert!(result.references.source_shared(index, &base, index));
        assert!(result.trace.file_references.is_empty());
        assert_eq!(
            result.trace.catalog_work,
            oracle.work_used
                - oracle
                    .source_facts
                    .iter()
                    .map(|source| source.work)
                    .sum::<usize>()
        );
        if script_family(&snapshot.documents[document(&snapshot, path)].key.language) {
            assert_ne!(
                base.source_facts[index].work, oracle.source_facts[index].work,
                "current descriptor sizes affect source copy Work"
            );
        }
        assert_eq!(
            base.canonical().unwrap(),
            frozen,
            "older descriptor table is immutable"
        );
    }
}

#[test]
fn changed_source_rewarms_later_source() {
    let mut snapshot = snapshot_of(&[
        (
            "src/a.rs",
            "pub fn a() { crate::missed::g(); serde::de::value(); }\n",
        ),
        (
            "src/b.rs",
            "pub fn b() { crate::shared::f(); crate::shared::f(); serde::de::value(); }\n",
        ),
        (
            "src/lib.rs",
            "mod a; mod b; pub mod shared; pub mod provider;\n",
        ),
        (
            "src/shared.rs",
            "pub use crate::provider::*; pub fn g() {}\n",
        ),
        ("src/provider.rs", "pub fn f() {}\n"),
    ]);
    snapshot.manifests.push(external::ManifestInput {
        source_set_id: snapshot.documents[0].key.source_set_id.clone(),
        path: "Cargo.toml".into(),
        content_hash: Some("captured".into()),
        bytes: Some(
            b"[package]\nname='app'\n[dependencies]\nserde='1'\n[dev-dependencies]\nserde='1'\n"
                .to_vec(),
        ),
        native_revision_id: "native".into(),
        dependency_hash: "dependencies".into(),
    });
    let base = full(&snapshot, 2);
    let a = document(&snapshot, "src/a.rs");
    snapshot.documents[a].file.text = snapshot.documents[a]
        .file
        .text
        .replace("missed::g", "shared::f");
    for call in &mut snapshot.documents[a].calls {
        if let Some(spelling) = &mut call.spelling {
            *spelling = spelling.replace("missed::g", "shared::f");
        }
    }
    let dirty = BTreeSet::from([snapshot.documents[a].key.clone()]);
    let result = replay(&snapshot, &base, dirty, BTreeSet::new(), 2);
    let oracle = full(&snapshot, 2);
    assert_eq!(
        base.rust_catalog_digest, oracle.rust_catalog_digest,
        "resolution-semantic catalog stable"
    );
    assert_eq!(
        result.references.canonical().unwrap(),
        oracle.canonical().unwrap()
    );
    let b = oracle
        .identities
        .iter()
        .position(|identity| identity.path == "src/b.rs")
        .unwrap();
    assert_ne!(
        base.source_facts[b].work, oracle.source_facts[b].work,
        "early dirty lookup changes later clean memo warmth"
    );
    assert!(result.references.source_shared(b, &base, b));
    assert!(Arc::ptr_eq(
        result.references.source_facts[b].plan.as_ref().unwrap(),
        base.source_facts[b].plan.as_ref().unwrap()
    ));
    assert!(!result.references.external_packages.by_source[b].is_empty());
}

#[test]
fn replay_membership_disagreement_and_missing_plans_are_ordinary_failures() {
    let snapshot = snapshot_of(&[("use.py", "import json\njson.dumps({})\n")]);
    for missing_plan in [false, true] {
        let mut base = full(&snapshot, 1);
        if missing_plan {
            base.source_facts[0].plan = None;
        } else {
            Arc::get_mut(base.sources[0].as_mut().unwrap())
                .unwrap()
                .packages
                .clear();
        }
        let preparation = incremental::prepare_with_workers(&snapshot, 1, &mut || Ok(())).unwrap();
        let error = incremental::finish_incremental(
            &snapshot,
            preparation,
            &base,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut || Ok(()),
        )
        .err()
        .unwrap();
        assert_eq!(
            error.to_string(),
            if missing_plan {
                "incremental missing charge plan"
            } else {
                "incremental package membership changed"
            }
        );
        assert!(!crate::architecture_core::CoreFailure::repeats(&error));
    }
}

#[test]
fn patch_unresolved_target() {
    let mut snapshot = fixture();
    let source = document(&snapshot, "src/lib.rs");
    let binding = anchor(&snapshot, target(&snapshot, "src/b.rs", "g"));
    snapshot.documents[source].semantic.producer_id = Some("producer".into());
    snapshot.documents[source].semantic.anchors = vec![binding];
    let base = full(&snapshot, 1);
    let source_index = base
        .identities
        .iter()
        .position(|identity| identity.path == "src/lib.rs")
        .unwrap();
    let mut retained = base.sources[source_index]
        .as_ref()
        .unwrap()
        .as_ref()
        .clone();
    let site = retained
        .edges
        .values()
        .flat_map(|edge| &edge.calls)
        .find(|site| site.target.is_some())
        .unwrap()
        .clone();
    retained
        .unresolved
        .push((site.clone(), "retained residual"));
    retained
        .edges
        .values_mut()
        .find(|edge| edge.calls.iter().any(|site| site.target.is_some()))
        .unwrap()
        .others
        .push(site);
    let target_sid = retained.unresolved[0]
        .0
        .target
        .as_ref()
        .unwrap()
        .syntax_id
        .clone();
    let target = document(&snapshot, "src/b.rs");
    let prefix = "\n// moved\n";
    snapshot.documents[target].file.text.insert_str(0, prefix);
    for declaration in &mut snapshot.documents[target].declarations {
        declaration.range.start += prefix.len();
        declaration.range.end += prefix.len();
        if declaration.name.as_deref() == Some("g") {
            declaration.name = None;
        }
    }
    let mut check = || Ok(());
    let request = parallel_check::RequestCheck::new(&mut check);
    let admitted = admit_snapshot(&snapshot, &mut request.worker()).unwrap();
    let read = ReadContext {
        snapshot: &snapshot,
        docs: &admitted.docs,
        trees: &[],
        document_map: &admitted.document_map,
        selected: &[],
        sample_limit: MAX_SAMPLE_LIMIT,
        workers: 1,
        request: &request,
    };
    let patched = incremental::patch_source_targets(&retained, read, &mut BTreeMap::new()).unwrap();
    assert_eq!(patched.unresolved[0].1, "retained residual");
    let mut count = 0;
    for site in patched
        .edges
        .values()
        .flat_map(|edge| edge.calls.iter().chain(&edge.others))
        .chain(patched.unresolved.iter().map(|(site, _)| site))
    {
        if let Some(target) = &site.target {
            count += 1;
            assert_eq!(target.lines.start, 3);
            assert!(
                target
                    .display_key
                    .contains(&format!("function@{}", prefix.len())),
                "{}",
                target.display_key
            );
        }
    }
    assert_eq!(count, 3, "calls, others and unresolved target are patched");
    assert_eq!(patched.coverage.len(), retained.coverage.len());
    assert_eq!(patched.packages, retained.packages);
    snapshot.documents[target]
        .declarations
        .retain(|declaration| declaration.syntax_id != target_sid);
    let admitted = admit_snapshot(&snapshot, &mut request.worker()).unwrap();
    let read = ReadContext {
        snapshot: &snapshot,
        docs: &admitted.docs,
        trees: &[],
        document_map: &admitted.document_map,
        selected: &[],
        sample_limit: MAX_SAMPLE_LIMIT,
        workers: 1,
        request: &request,
    };
    assert!(incremental::patch_source_targets(&retained, read, &mut BTreeMap::new()).is_err());
}
