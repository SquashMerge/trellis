use serde_json::json;
use std::collections::BTreeMap;
use trellis::{native_evidence as n, native_ids, semantic_evidence as s};

fn fixture() -> (s::SemanticPublication, n::Artifact, s::CapturedComponents) {
    let bytes = b"fn hello() { hello(); hello(); }".to_vec();
    let h = |b: &[u8]| native_ids::digest(b"", b);
    let document = n::DocumentKey {
        source_set_id: "source-set:v1:root".into(),
        language: "rust".into(),
        path: "src/main.rs".into(),
    };
    let native_toolchain = json!({"nativeExecutable":h(b"native")});
    let native_config = vec![json!({"config":"test"})];
    let native_dependencies = vec![json!({"dependencies":"test"})];
    let toolchain = native_ids::canonical(&native_toolchain);
    let config = native_ids::canonical(&json!(native_config));
    let dependency = native_ids::canonical(&json!(native_dependencies));
    let native_id = format!(
        "revision:v2:{}",
        native_ids::digest(
            b"trellis.native-revision.v2\0",
            &native_ids::canonical(&json!({
                "sourceSetId":document.source_set_id,
                "documents":[{"byteLength":bytes.len(),"contentHash":h(&bytes),"language":document.language,"path":document.path}],
                "toolchain":native_toolchain,"config":native_config,"dependencies":native_dependencies
            }))
        )
    );
    let revision = n::Revision {
        id: native_id.clone(),
        source_set_id: document.source_set_id.clone(),
        documents: vec![n::Document {
            key: document.clone(),
            revision_id: native_id.clone(),
            content_hash: h(&bytes),
            byte_length: bytes.len(),
        }],
        toolchain_hash: h(&toolchain),
        config_hash: h(&config),
        dependency_hash: h(&dependency),
    };
    let producer = n::Producer {
        id: "trellis.native.syntax".into(),
        version: "native-v12".into(),
        executable_hash: h(b"native"),
        kind: "native".into(),
        languages: vec!["rust".into()],
        position_encoding: "utf8".into(),
    };
    let mut registry = native_ids::IdentityRegistry::default();
    let syntax_id = registry.stable(&json!({"sourceSet":document.source_set_id,"path":document.path,"language":"rust","ancestors":[],"declaration":{"kind":"function","name":"hello","signature":null,"ordinal":0}})).unwrap();
    let key = n::Key {
        kind: "function".into(),
        name: Some("hello".into()),
        signature: None,
        ordinal: 0,
    };
    let declaration = n::Declaration {
        role: n::DeclarationRole::Production,
        syntax_id: syntax_id.clone(),
        document: document.clone(),
        revision_id: revision.id.clone(),
        kind: "function".into(),
        name: Some("hello".into()),
        lookup_key: Some("hello".into()),
        ancestors: vec![],
        key,
        range: n::Range {
            start: 0,
            end: bytes.len(),
        },
        name_range: Some(n::Range { start: 3, end: 8 }),
        header: n::Header {
            kind: "function".into(),
            name: Some("hello".into()),
            modifiers: vec![],
            type_parameters: vec![],
            parameters: vec![],
            result_type: None,
            bases: vec![],
        },
        provenance_id: "native-proof".into(),
    };
    let call_id = registry.occurrence(&json!({"contentHash":h(&bytes),"extractionContext":native_ids::extraction_context("rust",&[]).unwrap(),"nativeProducerId":producer.id,"nativeProducerVersion":producer.version,"ownerSyntaxId":syntax_id,"kind":"call","ordinal":0})).unwrap();
    let call = n::Call {
        id: call_id.clone(),
        owner_syntax_id: syntax_id.clone(),
        ordinal: 0,
        document: document.clone(),
        revision_id: revision.id.clone(),
        range: n::Range { start: 13, end: 20 },
        callee_range: Some(n::Range { start: 13, end: 18 }),
        spelling: Some("hello".into()),
        region_ids: vec![],
        provenance_id: "native-proof".into(),
    };
    let references = [n::Range { start: 3, end: 8 }, n::Range { start: 13, end: 18 }, n::Range { start: 22, end: 27 }]
        .into_iter()
        .enumerate()
        .map(|(ordinal, range)| n::Reference {
            id: registry.occurrence(&json!({"contentHash":h(&bytes),"extractionContext":native_ids::extraction_context("rust",&[]).unwrap(),"nativeProducerId":producer.id,"nativeProducerVersion":producer.version,"ownerSyntaxId":syntax_id,"kind":"reference","ordinal":ordinal})).unwrap(),
            owner_syntax_id: syntax_id.clone(),
            ordinal,
            document: document.clone(),
            revision_id: revision.id.clone(),
            range,
            spelling: "hello".into(),
            lookup_key: "hello".into(),
            provenance_id: "native-proof".into(),
        })
        .collect::<Vec<_>>();
    let native = n::Artifact {
        search_cards: vec![],
        producer,
        source_set: n::SourceSet {
            id: document.source_set_id.clone(),
            root_id: "root".into(),
            languages: vec!["rust".into()],
            dependencies: vec![],
        },
        revision: revision.clone(),
        coverage: vec![],
        provenance: vec![],
        declarations: vec![declaration],
        calls: vec![call],
        control_regions: vec![],
        references,
        capability_reports: vec![n::CapabilityReport {
            document: document.clone(),
            revision_id: revision.id.clone(),
            families: [
                n::MeasurementKind::DeclarationName,
                n::MeasurementKind::Callee,
                n::MeasurementKind::Invocation,
                n::MeasurementKind::Reference,
            ]
            .into_iter()
            .map(|kind| n::MeasurementSupport {
                kind,
                available: true,
                diagnostic: None,
                gaps: vec![],
            })
            .collect(),
            macro_expansions: vec![],
        }],
    };
    let semantic_producer = s::Producer {
        id: "semantic".into(),
        version: "v1".into(),
        executable_hash: h(b"semantic-exe"),
        kind: s::SemanticKind::Semantic,
        languages: vec![s::Language::Rust],
        position_encoding: s::PositionEncoding::Utf8,
    };
    let basis = s::SemanticBasis {
        producer_id: "semantic".into(),
        producer_version: "v1".into(),
        producer_hash: semantic_producer.executable_hash.clone(),
        artifact_hash: h(b"semantic-artifact"),
        language: s::Language::Rust,
        source_set_id: document.source_set_id.clone(),
        revision_id: revision.id.clone(),
        source_manifest_hash: h(&native_ids::canonical(
            &json!([{"document":document,"contentHash":h(&bytes)}]),
        )),
        toolchain_hash: revision.toolchain_hash.clone(),
        config_hash: revision.config_hash.clone(),
        dependency_hash: revision.dependency_hash.clone(),
        lookup_dependencies: vec!["hello".into()],
    };
    let proof = |id: &str, kind| s::Provenance {
        id: id.into(),
        producer_id: "semantic".into(),
        document: document.clone(),
        revision_id: revision.id.clone(),
        content_hash: h(&bytes),
        evidence_kind: kind,
        basis: Some(basis.clone()),
        freshness: s::Freshness::Fresh,
        derived_from: None,
    };
    let symbol = s::SymbolKey {
        scheme: s::SymbolScheme::Scip,
        symbol: "pkg hello".into(),
        scope: s::SymbolScope::Global,
        document: None,
    };
    let internal = s::Target::Internal {
        syntax_id: syntax_id.clone(),
        document: document.clone(),
        revision_id: revision.id.clone(),
    };
    let external = s::Target::External {
        symbol: s::SymbolKey {
            scheme: s::SymbolScheme::Scip,
            symbol: "outside".into(),
            scope: s::SymbolScope::Global,
            document: None,
        },
    };
    let anchor = |range, kind| s::MeasuredAnchor {
        document: document.clone(),
        revision_id: revision.id.clone(),
        content_hash: h(&bytes),
        range,
        kind,
    };
    let join = |range, kind, id: String| s::Join {
        anchor: anchor(range, kind),
        status: s::JoinStatus::Exact,
        candidate_ids: vec![id],
        diagnostic: None,
    };
    let reference_id = native.references[1].id.clone();
    let semantic = s::SemanticPublication {
        producers: vec![semantic_producer],
        coverage: vec![s::Coverage {
            producer_id: "semantic".into(),
            language: s::Language::Rust,
            source_set_id: document.source_set_id.clone(),
            document_path: document.path.clone(),
            revision_id: revision.id.clone(),
            requested: true,
            selected: true,
            state: s::CoverageState::Complete,
            supported_roles: vec![
                s::Role::Definition,
                s::Role::Read,
                s::Role::Call,
                s::Role::Type,
            ],
            observed_roles: vec![
                s::Role::Definition,
                s::Role::Read,
                s::Role::Call,
                s::Role::Type,
            ],
            diagnostic: None,
            reasons: None,
        }],
        provenance: vec![
            proof("bind", s::EvidenceKind::DeclarationBinding),
            proof("ref", s::EvidenceKind::SemanticReference),
            proof("rel", s::EvidenceKind::TypeRelationship),
        ],
        symbols: vec![s::Symbol {
            key: symbol.clone(),
            display_name: Some("hello".into()),
            declarations: vec![internal.clone(), external.clone()],
            provenance_id: "bind".into(),
        }],
        declaration_bindings: vec![s::DeclarationBinding {
            syntax_id: Some(syntax_id.clone()),
            symbols: vec![symbol.clone()],
            join: join(
                n::Range { start: 3, end: 8 },
                s::JoinKind::DeclarationName,
                syntax_id.clone(),
            ),
            provenance_id: "bind".into(),
        }],
        references: vec![s::Reference {
            id: reference_id,
            owner_syntax_id: syntax_id.clone(),
            ordinal: 1,
            document: document.clone(),
            revision_id: revision.id.clone(),
            range: n::Range { start: 13, end: 18 },
            spelling: "hello".into(),
            lookup_key: "hello".into(),
            site: s::ReferenceSite::Use,
            roles: vec![s::Role::Read],
            resolution: s::Resolution::Resolved,
            declared_target: Some(internal.clone()),
            candidates: vec![],
            provenance_id: "ref".into(),
        }],
        call_bindings: vec![s::CallBinding {
            call_id: Some(call_id.clone()),
            join: join(
                n::Range { start: 13, end: 18 },
                s::JoinKind::Callee,
                call_id,
            ),
            resolution: s::Resolution::Resolved,
            declared_target: Some(internal.clone()),
            candidates: vec![],
            dispatch: s::Dispatch::Direct,
            possible_dispatch: vec![external.clone()],
            possible_dispatch_complete: false,
            stale_target: Some(false),
            provenance_id: "ref".into(),
        }],
        type_relationships: vec![s::TypeRelationship {
            kind: s::RelationshipKind::Overrides,
            source: internal,
            target: external,
            provenance_id: "rel".into(),
        }],
    };
    let captured = s::CapturedComponents {
        producer_executables: BTreeMap::from([("semantic".into(), b"semantic-exe".to_vec())]),
        artifacts: BTreeMap::from([(
            ("semantic".into(), h(b"semantic-artifact")),
            b"semantic-artifact".to_vec(),
        )]),
        sources: BTreeMap::from([(document, bytes)]),
        captured_revision: revision,
        native_toolchain,
        native_config,
        native_dependencies,
        requested_producers: semantic.producers.clone(),
        toolchain,
        config,
        dependency,
        native_extraction_components: BTreeMap::new(),
    };
    (semantic, native, captured)
}
#[test]
fn all_semantic_families_keep_the_second_native_candidate_ordinal() {
    let (semantic, native, bytes) = fixture();
    assert_eq!(native.references.len(), 3);
    assert_eq!(semantic.references.len(), 1);
    assert_eq!(semantic.references[0].id, native.references[1].id);
    assert_eq!(semantic.references[0].ordinal, 1);
    s::validate_evidence(&semantic, &native, &bytes).unwrap();
    let mut renumbered = semantic.clone();
    renumbered.references[0].ordinal = 0;
    assert!(
        s::validate_evidence(&renumbered, &native, &bytes)
            .unwrap_err()
            .to_string()
            .contains("native reference candidate")
    );
}
#[test]
fn captured_boundary_rejects_invalid_evidence() {
    let (semantic, native, bytes) = fixture();
    let mut variant = semantic.clone();
    variant.provenance[0].derived_from = Some(s::DerivedFrom {
        provenance_id: "old".into(),
        record_kind: s::RecordKind::Occurrence,
        fact_digest: "a".repeat(64),
    });
    assert!(
        s::validate_evidence(&variant, &native, &bytes)
            .unwrap_err()
            .to_string()
            .contains("derived provenance")
    );
    let mut variant = semantic.clone();
    variant.call_bindings[0].possible_dispatch_complete = true;
    assert!(s::validate_evidence(&variant, &native, &bytes).is_err());
    let mut variant = semantic.clone();
    variant.call_bindings[0].stale_target = Some(true);
    assert!(s::validate_evidence(&variant, &native, &bytes).is_err());
    let mut variant = semantic.clone();
    variant.provenance[0].basis.as_mut().unwrap().artifact_hash = "0".repeat(64);
    assert!(
        s::validate_evidence(&variant, &native, &bytes)
            .unwrap_err()
            .to_string()
            .contains("basis digest")
    );
    let mut variant = semantic.clone();
    variant.declaration_bindings[0].join.candidate_ids.clear();
    assert!(
        s::validate_evidence(&variant, &native, &bytes)
            .unwrap_err()
            .to_string()
            .contains("measured candidates")
    );
    let mut variant = semantic.clone();
    variant.references[0].id = "occ:v2:".to_owned() + &"0".repeat(32);
    assert!(
        s::validate_evidence(&variant, &native, &bytes)
            .unwrap_err()
            .to_string()
            .contains("native reference candidate")
    );
    let mut bytes = bytes;
    bytes.sources.values_mut().next().unwrap()[0] = b'X';
    assert!(
        s::validate_evidence(&semantic, &native, &bytes)
            .unwrap_err()
            .to_string()
            .contains("source digest")
    );
}
#[test]
fn required_nullable_and_nested_keys_are_closed() {
    let (semantic, _, _) = fixture();
    let mut value = serde_json::to_value(&semantic).unwrap();
    value["coverage"][0]
        .as_object_mut()
        .unwrap()
        .remove("diagnostic");
    assert!(serde_json::from_value::<s::SemanticPublication>(value).is_err());
    let mut value = serde_json::to_value(&semantic).unwrap();
    value["references"][0]["declaredTarget"]["unexpected"] = json!(1);
    assert!(serde_json::from_value::<s::SemanticPublication>(value).is_err());
}

#[test]
fn six_coverage_states_and_roles() {
    let (base, native, bytes) = fixture();
    for (state, requested, selected, diagnostic) in [
        (s::CoverageState::NotRequested, false, false, None),
        (s::CoverageState::Omitted, true, false, Some("not run")),
        (
            s::CoverageState::Unsupported,
            true,
            false,
            Some("unavailable"),
        ),
        (s::CoverageState::Failed, true, true, Some("failed")),
        (s::CoverageState::Partial, true, true, Some("partial")),
        (s::CoverageState::Complete, true, true, None),
    ] {
        let mut semantic = base.clone();
        semantic.coverage[0].state = state;
        semantic.coverage[0].requested = requested;
        semantic.coverage[0].selected = selected;
        semantic.coverage[0].diagnostic = diagnostic.map(str::to_owned);
        if !matches!(
            state,
            s::CoverageState::Complete | s::CoverageState::Partial
        ) {
            semantic.coverage[0].observed_roles.clear();
            semantic.provenance.clear();
            semantic.symbols.clear();
            semantic.declaration_bindings.clear();
            semantic.references.clear();
            semantic.call_bindings.clear();
            semantic.type_relationships.clear();
        }
        s::validate_evidence(&semantic, &native, &bytes).unwrap();
        semantic.coverage[0].selected = !selected;
        assert!(
            s::validate_evidence(&semantic, &native, &bytes)
                .unwrap_err()
                .to_string()
                .contains("coverage")
        );
    }
    let mut variant = base.clone();
    variant.coverage[0].observed_roles.push(s::Role::Type);
    assert!(s::validate_evidence(&variant, &native, &bytes).is_err());
}
#[test]
fn joins_use_distinct_measured_candidates_and_preserve_nonexact_evidence() {
    let (base, mut native, bytes) = fixture();
    let mut semantic = base.clone();
    let second = "sid:v1:".to_owned() + &"f".repeat(32);
    let mut declaration = native.declarations[0].clone();
    declaration.syntax_id = second.clone();
    native.declarations.push(declaration);
    semantic.declaration_bindings[0].syntax_id = None;
    semantic.declaration_bindings[0].join.status = s::JoinStatus::Ambiguous;
    semantic.declaration_bindings[0].join.candidate_ids = vec![
        base.declaration_bindings[0].syntax_id.clone().unwrap(),
        second,
    ];
    semantic.declaration_bindings[0].join.candidate_ids.sort();
    semantic.declaration_bindings[0].join.diagnostic = Some("two measured declarations".into());
    semantic.coverage[0]
        .observed_roles
        .retain(|r| *r != s::Role::Definition);
    s::validate_evidence(&semantic, &native, &bytes).unwrap();
    let mut wrong = semantic.clone();
    wrong.declaration_bindings[0].join.candidate_ids.pop();
    assert!(s::validate_evidence(&wrong, &native, &bytes).is_err());
    let mut unmatched = base.clone();
    unmatched.declaration_bindings[0].join.anchor.range = n::Range { start: 0, end: 2 };
    unmatched.declaration_bindings[0].join.status = s::JoinStatus::Unmatched;
    unmatched.declaration_bindings[0].join.candidate_ids.clear();
    unmatched.declaration_bindings[0].join.diagnostic = Some("no measured declaration".into());
    unmatched.declaration_bindings[0].syntax_id = None;
    unmatched.coverage[0]
        .observed_roles
        .retain(|r| *r != s::Role::Definition);
    s::validate_evidence(&unmatched, &fixture().1, &bytes).unwrap();
    unmatched.declaration_bindings[0].join.status = s::JoinStatus::Unsupported;
    assert!(s::validate_evidence(&unmatched, &fixture().1, &bytes).is_err());
}

#[test]
fn captured_basis_is_authenticated_before_requested_freshness() {
    let (mut semantic, mut native, mut captured) = fixture();
    semantic.symbols.clear();
    semantic.declaration_bindings.clear();
    semantic.references.clear();
    semantic.call_bindings.clear();
    semantic.type_relationships.clear();
    semantic.coverage[0].observed_roles.clear();
    native.revision.id = "revision-2".into();
    for d in &mut native.revision.documents {
        d.revision_id = native.revision.id.clone();
    }
    semantic.coverage[0].revision_id = native.revision.id.clone();
    for p in &mut semantic.provenance {
        p.freshness = s::Freshness::PossiblyStale;
    }
    s::validate_evidence(&semantic, &native, &captured).unwrap();
    semantic.provenance[0].freshness = s::Freshness::Fresh;
    assert!(
        s::validate_evidence(&semantic, &native, &captured)
            .unwrap_err()
            .to_string()
            .contains("freshness")
    );
    semantic.provenance[0].freshness = s::Freshness::PossiblyStale;
    captured.captured_revision.toolchain_hash = "0".repeat(64);
    assert!(
        s::validate_evidence(&semantic, &native, &captured)
            .unwrap_err()
            .to_string()
            .contains("component mismatch")
    );
}

#[test]
fn native_capture_identity_and_requested_context_are_independent() {
    let (mut semantic, native, mut captured) = fixture();
    captured.captured_revision.id = "revision:v2:".to_owned() + &"0".repeat(64);
    captured.captured_revision.documents[0].revision_id = captured.captured_revision.id.clone();
    assert!(
        s::validate_evidence(&semantic, &native, &captured)
            .unwrap_err()
            .to_string()
            .contains("native revision ID")
    );
    let (_, _, mut captured) = fixture();
    captured.native_toolchain = json!({"nativeExecutable":"forged"});
    assert!(s::validate_evidence(&semantic, &native, &captured).is_err());
    let (_, _, mut captured) = fixture();
    captured.requested_producers[0].version = "v2".into();
    for proof in &mut semantic.provenance {
        proof.freshness = s::Freshness::PossiblyStale;
    }
    s::validate_evidence(&semantic, &native, &captured).unwrap();
    captured.requested_producers.clear();
    s::validate_evidence(&semantic, &native, &captured).unwrap();
    captured.requested_producers = semantic.producers.clone();
    captured.requested_producers[0].executable_hash = "0".repeat(64);
    s::validate_evidence(&semantic, &native, &captured).unwrap();
    captured.requested_producers = semantic.producers.clone();
    captured.requested_producers[0].languages = vec![s::Language::Python];
    s::validate_evidence(&semantic, &native, &captured).unwrap();
    captured.requested_producers = semantic.producers.clone();
    semantic.provenance[0].freshness = s::Freshness::Fresh;
    assert!(s::validate_evidence(&semantic, &native, &captured).is_err());
}
#[test]
fn captured_manifest_contract_order_and_current_coverage() {
    let (mut semantic, mut native, mut captured) = fixture();
    let second = n::DocumentKey {
        source_set_id: native.revision.source_set_id.clone(),
        language: "javascript".into(),
        path: "extra.js".into(),
    };
    native.source_set.languages.push("javascript".into());
    native.revision.id = "revision:v2:".to_owned() + &"1".repeat(64);
    native.revision.documents[0].revision_id = native.revision.id.clone();
    for p in &mut semantic.provenance {
        p.freshness = s::Freshness::PossiblyStale;
    }
    semantic.coverage[0].revision_id = native.revision.id.clone();
    // Cross-revision occurrence facts are not admitted; retain only historical coverage/proofs.
    semantic.symbols.clear();
    semantic.declaration_bindings.clear();
    semantic.references.clear();
    semantic.call_bindings.clear();
    semantic.type_relationships.clear();
    semantic.coverage[0].observed_roles.clear();
    native.revision.documents.push(n::Document {
        key: second.clone(),
        revision_id: native.revision.id.clone(),
        content_hash: native_ids::digest(b"", b"let x=0;"),
        byte_length: 8,
    });
    // Captured document is still only rust, but current coverage must include JavaScript for a producer supporting it.
    semantic.producers[0]
        .languages
        .push(s::Language::Javascript);
    captured.requested_producers = semantic.producers.clone();
    semantic.coverage.push(s::Coverage {
        producer_id: "semantic".into(),
        language: s::Language::Javascript,
        source_set_id: second.source_set_id.clone(),
        document_path: second.path.clone(),
        revision_id: native.revision.id.clone(),
        requested: false,
        selected: false,
        state: s::CoverageState::NotRequested,
        supported_roles: vec![],
        observed_roles: vec![],
        diagnostic: None,
        reasons: None,
    });
    // Producer language order must itself follow contract order (rust before javascript).
    s::validate_evidence(&semantic, &native, &captured).unwrap();
    semantic.coverage.pop();
    assert!(
        s::validate_evidence(&semantic, &native, &captured)
            .unwrap_err()
            .to_string()
            .contains("missing producer coverage")
    );
    semantic.coverage.push(s::Coverage {
        producer_id: "semantic".into(),
        language: s::Language::Javascript,
        source_set_id: second.source_set_id.clone(),
        document_path: second.path.clone(),
        revision_id: native.revision.id.clone(),
        requested: false,
        selected: false,
        state: s::CoverageState::NotRequested,
        supported_roles: vec![],
        observed_roles: vec![],
        diagnostic: None,
        reasons: None,
    });
    semantic.coverage[1].document_path = "captured-only.js".into();
    assert!(
        s::validate_evidence(&semantic, &native, &captured)
            .unwrap_err()
            .to_string()
            .contains("unsupported coverage document")
    );
}
#[test]
fn independent_producers_and_duplicate_fact_partitions() {
    let (mut semantic, native, mut captured) = fixture();
    let mut second = semantic.producers[0].clone();
    second.id = "semantic-2".into();
    captured
        .producer_executables
        .insert(second.id.clone(), b"semantic-exe".to_vec());
    captured.artifacts.insert(
        (
            second.id.clone(),
            native_ids::digest(b"", b"semantic-artifact"),
        ),
        b"semantic-artifact".to_vec(),
    );
    captured.requested_producers.push(second.clone());
    semantic.producers.push(second);
    let mut coverage = semantic.coverage[0].clone();
    coverage.producer_id = "semantic-2".into();
    coverage.observed_roles = vec![s::Role::Read, s::Role::Call];
    semantic.coverage.push(coverage);
    let originals = semantic.provenance.clone();
    for mut p in originals {
        p.id = format!("2-{}", p.id);
        p.producer_id = "semantic-2".into();
        p.basis.as_mut().unwrap().producer_id = "semantic-2".into();
        semantic.provenance.push(p);
    }
    let mut symbol = semantic.symbols[0].clone();
    symbol.provenance_id = "2-bind".into();
    semantic.symbols.push(symbol);
    let mut call = semantic.call_bindings[0].clone();
    call.provenance_id = "2-ref".into();
    semantic.call_bindings.push(call);
    let candidate = &native.references[2];
    let mut reference = semantic.references[0].clone();
    reference.id = candidate.id.clone();
    reference.ordinal = candidate.ordinal;
    reference.range = candidate.range.clone();
    reference.provenance_id = "2-ref".into();
    semantic.references.push(reference);
    assert_eq!(semantic.references[0].ordinal, 1);
    assert_eq!(semantic.references[1].ordinal, 2);
    s::validate_evidence(&semantic, &native, &captured).unwrap();
    semantic.symbols.push(semantic.symbols[0].clone());
    assert!(
        s::validate_evidence(&semantic, &native, &captured)
            .unwrap_err()
            .to_string()
            .contains("duplicate semantic symbol")
    );
    semantic.symbols.pop();
    semantic.references.push(semantic.references[0].clone());
    assert!(
        s::validate_evidence(&semantic, &native, &captured)
            .unwrap_err()
            .to_string()
            .contains("duplicate reference")
    );
}
#[test]
fn invalid_roles_and_cross_fact_targets_are_rejected() {
    let (base, native, bytes) = fixture();
    let mut s = base.clone();
    s.references[0].roles = vec![s::Role::Alias];
    assert!(s::validate_evidence(&s, &native, &bytes).is_err());
    let mut s = base.clone();
    s.references[0].roles = vec![s::Role::Call];
    s.references[0].range = n::Range { start: 3, end: 8 };
    assert!(s::validate_evidence(&s, &native, &bytes).is_err());
    let mut s = base.clone();
    s.coverage[0].observed_roles.push(s::Role::Type);
    assert!(s::validate_evidence(&s, &native, &bytes).is_err());
    let mut s = base.clone();
    s.call_bindings.push(s.call_bindings[0].clone());
    s.call_bindings[1].provenance_id = "bind".into(); // incompatible proof kind
    assert!(s::validate_evidence(&s, &native, &bytes).is_err());
    s.call_bindings[1].provenance_id = "ref".into();
    assert!(
        s::validate_evidence(&s, &native, &bytes)
            .unwrap_err()
            .to_string()
            .contains("duplicate call binding")
    );
    let mut s = base.clone();
    s.type_relationships[0].source = base.type_relationships[0].target.clone();
    assert!(s::validate_evidence(&s, &native, &bytes).is_err());
}

#[test]
fn available_measurement_rejects_unsupported_and_invalid_anchors() {
    let (base, native, bytes) = fixture();
    for declaration in [true, false] {
        let mut semantic = base.clone();
        let (join, id) = if declaration {
            let b = &mut semantic.declaration_bindings[0];
            (&mut b.join, &mut b.syntax_id)
        } else {
            let b = &mut semantic.call_bindings[0];
            (&mut b.join, &mut b.call_id)
        };
        join.anchor.range = n::Range { start: 0, end: 2 };
        join.candidate_ids.clear();
        *id = None;
        join.status = s::JoinStatus::Unmatched;
        join.diagnostic = Some("no measured candidate".into());
        let role = if declaration {
            s::Role::Definition
        } else {
            s::Role::Call
        };
        semantic.coverage[0].observed_roles.retain(|r| *r != role);
        s::validate_evidence(&semantic, &native, &bytes).unwrap();
        let join = if declaration {
            &mut semantic.declaration_bindings[0].join
        } else {
            &mut semantic.call_bindings[0].join
        };
        join.status = s::JoinStatus::Unsupported;
        assert!(
            s::validate_evidence(&semantic, &native, &bytes)
                .unwrap_err()
                .to_string()
                .contains("join differs from measured candidates")
        );
        let join = if declaration {
            &mut semantic.declaration_bindings[0].join
        } else {
            &mut semantic.call_bindings[0].join
        };
        join.anchor.range = n::Range {
            start: 200,
            end: 210,
        };
        assert!(
            s::validate_evidence(&semantic, &native, &bytes)
                .unwrap_err()
                .to_string()
                .contains("converted semantic span")
        );
    }
    let mut invocation = base;
    invocation.call_bindings[0].join.anchor.kind = s::JoinKind::Invocation;
    invocation.call_bindings[0].join.anchor.range = n::Range { start: 0, end: 2 };
    invocation.call_bindings[0].join.status = s::JoinStatus::Unmatched;
    invocation.call_bindings[0].join.candidate_ids.clear();
    invocation.call_bindings[0].join.diagnostic = Some("no measured invocation".into());
    invocation.call_bindings[0].call_id = None;
    invocation.coverage[0]
        .observed_roles
        .retain(|r| *r != s::Role::Call);
    s::validate_evidence(&invocation, &native, &bytes).unwrap();
    invocation.call_bindings[0].join.status = s::JoinStatus::Unsupported;
    assert!(s::validate_evidence(&invocation, &native, &bytes).is_err());
}
#[test]
fn exact_invocation_and_distinct_provenances_are_preserved() {
    let (mut semantic, native, bytes) = fixture();
    let another = semantic
        .provenance
        .iter()
        .find(|p| p.id == "ref")
        .unwrap()
        .clone();
    let mut another = s::Provenance {
        id: "ref-2".into(),
        ..another
    };
    another.freshness = s::Freshness::Fresh;
    semantic.provenance.push(another);
    let mut binding = semantic.call_bindings[0].clone();
    binding.provenance_id = "ref-2".into();
    binding.join.anchor.kind = s::JoinKind::Invocation;
    binding.join.anchor.range = n::Range { start: 13, end: 20 };
    semantic.call_bindings.push(binding);
    s::validate_evidence(&semantic, &native, &bytes).unwrap();
    semantic.call_bindings[1].declared_target = Some(semantic.type_relationships[0].target.clone());
    semantic.call_bindings[1].resolution = s::Resolution::External;
    semantic.call_bindings[1].stale_target = None;
    assert!(
        s::validate_evidence(&semantic, &native, &bytes)
            .unwrap_err()
            .to_string()
            .contains("contradictory resolved call targets")
    );
}

#[test]
fn observed_roles_match_selected_fact_roles_in_complete_and_partial_rows() {
    let (base, native, bytes) = fixture();
    for state in [s::CoverageState::Complete, s::CoverageState::Partial] {
        let mut evidence = base.clone();
        evidence.coverage[0].state = state;
        if state == s::CoverageState::Partial {
            evidence.coverage[0].diagnostic = Some("partial analysis".into());
        }
        s::validate_evidence(&evidence, &native, &bytes).unwrap();
        for role in [
            s::Role::Definition,
            s::Role::Read,
            s::Role::Call,
            s::Role::Type,
        ] {
            let mut missing = evidence.clone();
            missing.coverage[0].observed_roles.retain(|r| *r != role);
            assert!(
                s::validate_evidence(&missing, &native, &bytes)
                    .unwrap_err()
                    .to_string()
                    .contains("coverage observed roles differ")
            );
        }
    }
}

#[test]
fn reference_roles_cannot_be_removed_from_both_coverage_lists() {
    assert_installed_role_requires_coverage(s::Role::Read);
}

#[test]
fn exact_call_roles_cannot_be_removed_from_both_coverage_lists() {
    assert_installed_role_requires_coverage(s::Role::Call);
}

fn assert_installed_role_requires_coverage(role: s::Role) {
    let (base, native, bytes) = fixture();
    for state in [s::CoverageState::Complete, s::CoverageState::Partial] {
        let mut semantic = base.clone();
        semantic.coverage[0].state = state;
        if state == s::CoverageState::Partial {
            semantic.coverage[0].diagnostic = Some("partial analysis".into());
        }
        s::validate_evidence(&semantic, &native, &bytes).unwrap();
        semantic.coverage[0].supported_roles.retain(|r| *r != role);
        semantic.coverage[0].observed_roles.retain(|r| *r != role);
        assert!(
            s::validate_evidence(&semantic, &native, &bytes).is_err(),
            "installed {role:?} without coverage in {state:?}"
        );
    }
}

#[test]
fn empty_complete_coverage_diagnostic_is_rejected() {
    let (mut semantic, native, bytes) = fixture();
    semantic.coverage[0].diagnostic = Some(String::new());
    assert!(s::validate_evidence(&semantic, &native, &bytes).is_err());
}

#[test]
fn empty_exact_declaration_join_diagnostic_is_rejected() {
    let (mut semantic, native, bytes) = fixture();
    semantic.declaration_bindings[0].join.diagnostic = Some(String::new());
    assert!(s::validate_evidence(&semantic, &native, &bytes).is_err());
}

#[test]
fn empty_exact_call_join_diagnostic_is_rejected() {
    let (mut semantic, native, bytes) = fixture();
    semantic.call_bindings[0].join.diagnostic = Some(String::new());
    assert!(s::validate_evidence(&semantic, &native, &bytes).is_err());
}

#[test]
fn call_binding_candidates_require_exact_family_count_ids_and_order() {
    let (base, native, bytes) = fixture();
    for kind in [s::JoinKind::Callee, s::JoinKind::Invocation] {
        let mut exact = base.clone();
        if kind == s::JoinKind::Invocation {
            exact.call_bindings[0].join.anchor.kind = kind;
            exact.call_bindings[0].join.anchor.range = n::Range { start: 13, end: 20 };
        }
        s::validate_evidence(&exact, &native, &bytes).unwrap();
        let mut wrong = exact.clone();
        wrong.call_bindings[0].join.candidate_ids = vec!["occ:v2:".to_owned() + &"f".repeat(32)];
        assert!(s::validate_evidence(&wrong, &native, &bytes).is_err());
        let mut wrong = exact.clone();
        wrong.call_bindings[0].join.candidate_ids.clear();
        assert!(s::validate_evidence(&wrong, &native, &bytes).is_err());
        let mut wrong = exact.clone();
        wrong.call_bindings[0]
            .join
            .candidate_ids
            .push("occ:v2:".to_owned() + &"f".repeat(32));
        wrong.call_bindings[0].join.candidate_ids.sort();
        assert!(s::validate_evidence(&wrong, &native, &bytes).is_err());
        let mut wrong = exact.clone();
        wrong.call_bindings[0].join.anchor.kind = s::JoinKind::DeclarationName;
        assert!(
            s::validate_evidence(&wrong, &native, &bytes)
                .unwrap_err()
                .to_string()
                .contains("wrong join family")
        );
    }
    let mut native = native;
    let mut second = native.calls[0].clone();
    second.id = "occ:v2:".to_owned() + &"f".repeat(32);
    native.calls.push(second.clone());
    let mut ambiguous = base;
    let binding = &mut ambiguous.call_bindings[0];
    binding.call_id = None;
    binding.join.status = s::JoinStatus::Ambiguous;
    binding.join.candidate_ids.push(second.id);
    binding.join.candidate_ids.sort();
    binding.join.diagnostic = Some("two measured calls".into());
    ambiguous.coverage[0]
        .observed_roles
        .retain(|r| *r != s::Role::Call);
    s::validate_evidence(&ambiguous, &native, &bytes).unwrap();
    ambiguous.call_bindings[0].join.candidate_ids.reverse();
    assert!(s::validate_evidence(&ambiguous, &native, &bytes).is_err());
}

#[test]
fn captured_components_are_verified_before_each_requested_comparison() {
    let (mut semantic, mut native, captured) = fixture();
    for field in ["toolchain", "config", "dependency"] {
        let mut changed = native.clone();
        match field {
            "toolchain" => changed.revision.toolchain_hash = "0".repeat(64),
            "config" => changed.revision.config_hash = "0".repeat(64),
            _ => changed.revision.dependency_hash = "0".repeat(64),
        }
        changed.revision.id = "revision:v2:".to_owned() + &"1".repeat(64);
        changed.revision.documents[0].revision_id = changed.revision.id.clone();
        let mut historical = semantic.clone();
        historical.coverage[0].revision_id = changed.revision.id.clone();
        historical.coverage[0].observed_roles.clear();
        historical.symbols.clear();
        historical.declaration_bindings.clear();
        historical.references.clear();
        historical.call_bindings.clear();
        historical.type_relationships.clear();
        for proof in &mut historical.provenance {
            proof.freshness = s::Freshness::PossiblyStale;
        }
        s::validate_evidence(&historical, &changed, &captured).unwrap();
        historical.provenance[0].freshness = s::Freshness::Fresh;
        assert!(
            s::validate_evidence(&historical, &changed, &captured).is_err(),
            "{field}"
        );
    }
    native.revision.documents[0].content_hash = "0".repeat(64);
    native.revision.documents[0].byte_length = 23;
    native.revision.id = "revision:v2:".to_owned() + &"1".repeat(64);
    native.revision.documents[0].revision_id = native.revision.id.clone();
    semantic.coverage[0].revision_id = native.revision.id.clone();
    semantic.coverage[0].observed_roles.clear();
    semantic.symbols.clear();
    semantic.declaration_bindings.clear();
    semantic.references.clear();
    semantic.call_bindings.clear();
    semantic.type_relationships.clear();
    for proof in &mut semantic.provenance {
        proof.freshness = s::Freshness::Stale;
    }
    s::validate_evidence(&semantic, &native, &captured).unwrap();
    semantic.provenance[0].freshness = s::Freshness::PossiblyStale;
    assert!(s::validate_evidence(&semantic, &native, &captured).is_err());
    let (mut semantic, native, captured) = fixture();
    semantic.type_relationships[0].source = s::Target::Internal {
        syntax_id: native.declarations[0].syntax_id.clone(),
        document: n::DocumentKey {
            path: "foreign.rs".into(),
            ..native.declarations[0].document.clone()
        },
        revision_id: native.revision.id.clone(),
    };
    assert!(s::validate_evidence(&semantic, &native, &captured).is_err());
}
#[test]
fn malformed_nested_contract_fields_do_not_default() {
    let (semantic, _, _) = fixture();
    let value = serde_json::to_value(semantic).unwrap();
    for (family, index, field) in [
        ("provenance", 0, "derivedFrom"),
        ("provenance", 0, "basis"),
        ("symbols", 0, "displayName"),
        ("declarationBindings", 0, "syntaxId"),
        ("references", 0, "declaredTarget"),
        ("callBindings", 0, "callId"),
        ("callBindings", 0, "staleTarget"),
    ] {
        let mut variant = value.clone();
        variant[family][index]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(
            serde_json::from_value::<s::SemanticPublication>(variant).is_err(),
            "{family}.{field}"
        );
    }
    let mut variant = value.clone();
    variant["provenance"][0]["basis"]["surplus"] = json!(1);
    assert!(serde_json::from_value::<s::SemanticPublication>(variant).is_err());
    let mut variant = value.clone();
    variant["coverage"][0]["state"] = json!("mystery");
    assert!(serde_json::from_value::<s::SemanticPublication>(variant).is_err());
}

#[test]
fn captured_manifest_uses_language_enum_order_not_alphabetical_order() {
    let (mut semantic, mut native, mut captured) = fixture();
    let extra = n::DocumentKey {
        source_set_id: native.revision.source_set_id.clone(),
        language: "javascript".into(),
        path: "z.js".into(),
    };
    captured.sources.insert(extra.clone(), b"x".to_vec());
    captured.captured_revision.documents.push(n::Document {
        key: extra.clone(),
        revision_id: String::new(),
        content_hash: native_ids::digest(b"", b"x"),
        byte_length: 1,
    });
    let native_input = json!({"sourceSetId":captured.captured_revision.source_set_id,
        "documents":captured.captured_revision.documents.iter().map(|d|json!({"byteLength":d.byte_length,"contentHash":d.content_hash,"language":d.key.language,"path":d.key.path})).collect::<Vec<_>>(),
        "toolchain":captured.native_toolchain,"config":captured.native_config,"dependencies":captured.native_dependencies});
    let old = captured.captured_revision.id.clone();
    let captured_id = format!(
        "revision:v2:{}",
        native_ids::digest(
            b"trellis.native-revision.v2\0",
            &native_ids::canonical(&native_input)
        )
    );
    captured.captured_revision.id = captured_id.clone();
    for d in &mut captured.captured_revision.documents {
        d.revision_id = captured_id.clone();
    }
    native.revision.id = "revision:v2:".to_owned() + &"a".repeat(64);
    native.revision.documents[0].revision_id = native.revision.id.clone();
    semantic.coverage[0].revision_id = native.revision.id.clone();
    semantic.coverage[0].observed_roles.clear();
    semantic.symbols.clear();
    semantic.declaration_bindings.clear();
    semantic.references.clear();
    semantic.call_bindings.clear();
    semantic.type_relationships.clear();
    let manifest = serde_json::Value::Array(
        captured
            .captured_revision
            .documents
            .iter()
            .map(|d| json!({"document":d.key,"contentHash":d.content_hash}))
            .collect(),
    );
    for proof in &mut semantic.provenance {
        proof.revision_id = captured_id.clone();
        proof.basis.as_mut().unwrap().revision_id = captured_id.clone();
        proof.basis.as_mut().unwrap().source_manifest_hash =
            native_ids::digest(b"", &native_ids::canonical(&manifest));
        proof.freshness = s::Freshness::PossiblyStale;
    }
    assert_ne!(old, captured_id);
    s::validate_evidence(&semantic, &native, &captured).unwrap();
    captured.captured_revision.documents.reverse();
    assert!(
        s::validate_evidence(&semantic, &native, &captured)
            .unwrap_err()
            .to_string()
            .contains("unsorted native document manifest")
    );
}

#[test]
fn contradictory_targets_can_be_reported_as_ambiguous_without_changing_exact_join() {
    let (mut semantic, native, bytes) = fixture();
    let proof = semantic
        .provenance
        .iter()
        .find(|p| p.id == "ref")
        .unwrap()
        .clone();
    semantic.provenance.push(s::Provenance {
        id: "ref-2".into(),
        ..proof
    });
    let mut binding = semantic.call_bindings[0].clone();
    binding.provenance_id = "ref-2".into();
    let mut candidates = vec![
        semantic.call_bindings[0].declared_target.clone().unwrap(),
        semantic.type_relationships[0].target.clone(),
    ];
    candidates.sort_by_key(|t| native_ids::canonical(&serde_json::to_value(t).unwrap()));
    for b in &mut semantic.call_bindings {
        b.resolution = s::Resolution::Ambiguous;
        b.declared_target = None;
        b.candidates = candidates.clone();
        b.stale_target = None;
    }
    binding.resolution = s::Resolution::Ambiguous;
    binding.declared_target = None;
    binding.candidates = candidates;
    binding.stale_target = None;
    semantic.call_bindings.push(binding);
    s::validate_evidence(&semantic, &native, &bytes).unwrap();
    semantic.call_bindings[1].candidates.reverse();
    assert!(s::validate_evidence(&semantic, &native, &bytes).is_err());
}

#[test]
fn native_capability_gaps_preserve_exact_joins_and_require_full_containment() {
    let (base, mut native, bytes) = fixture();
    let support = native.capability_reports[0]
        .families
        .iter_mut()
        .find(|family| family.kind == n::MeasurementKind::DeclarationName)
        .unwrap();
    support.gaps.push(n::MeasurementGap {
        range: n::Range { start: 9, end: 12 },
        diagnostic: "opaque test construct".into(),
    });
    s::validate_evidence(&base, &native, &bytes).unwrap();
    let mut gap = base.clone();
    gap.declaration_bindings[0].syntax_id = None;
    gap.declaration_bindings[0].join.anchor.range = n::Range { start: 9, end: 11 };
    gap.declaration_bindings[0].join.status = s::JoinStatus::Unsupported;
    gap.declaration_bindings[0].join.candidate_ids.clear();
    gap.declaration_bindings[0].join.diagnostic = Some("opaque test construct".into());
    gap.coverage[0]
        .observed_roles
        .retain(|role| *role != s::Role::Definition);
    s::validate_evidence(&gap, &native, &bytes).unwrap();
    let mut wrong = gap.clone();
    wrong.declaration_bindings[0].join.diagnostic = Some("authored unavailable".into());
    assert!(s::validate_evidence(&wrong, &native, &bytes).is_err());
    for range in [
        n::Range { start: 8, end: 10 },
        n::Range { start: 11, end: 13 },
        n::Range { start: 12, end: 13 },
    ] {
        let mut overlap = gap.clone();
        overlap.declaration_bindings[0].join.anchor.range = range;
        assert!(s::validate_evidence(&overlap, &native, &bytes).is_err());
        overlap.declaration_bindings[0].join.status = s::JoinStatus::Unmatched;
        overlap.declaration_bindings[0].join.diagnostic = Some("outside native gap".into());
        s::validate_evidence(&overlap, &native, &bytes).unwrap();
    }
    let family = native.capability_reports[0]
        .families
        .iter_mut()
        .find(|family| family.kind == n::MeasurementKind::DeclarationName)
        .unwrap();
    family.gaps[0].range = n::Range { start: 3, end: 8 };
    assert!(
        s::validate_evidence(&base, &native, &bytes)
            .unwrap_err()
            .to_string()
            .contains("contradicts native capability gap")
    );
}

#[test]
fn semantic_reference_tuple_must_match_a_complete_native_candidate() {
    let (base, native, bytes) = fixture();
    let mut variants = Vec::new();
    let mut variant = base.clone();
    variant.references[0].ordinal = 0;
    variants.push(variant);
    let mut variant = base.clone();
    variant.references[0].range = native.references[2].range.clone();
    variants.push(variant);
    let mut variant = base.clone();
    variant.references[0].spelling = "other".into();
    variants.push(variant);
    let mut variant = base.clone();
    variant.references[0].lookup_key = "other".into();
    variants.push(variant);
    let mut variant = base.clone();
    variant.references[0].owner_syntax_id = "sid:v1:".to_owned() + &"0".repeat(32);
    variants.push(variant);
    for variant in variants {
        assert!(
            s::validate_evidence(&variant, &native, &bytes)
                .unwrap_err()
                .to_string()
                .contains("native reference candidate")
        );
    }
    let mut missing = native.clone();
    missing.references.remove(1);
    assert!(
        s::validate_evidence(&base, &missing, &bytes)
            .unwrap_err()
            .to_string()
            .contains("native reference candidate")
    );
}

#[test]
fn call_role_requires_exact_native_identity_and_matching_reference() {
    let (mut semantic, native, bytes) = fixture();
    semantic.references[0].roles = vec![s::Role::Read, s::Role::Call];
    s::validate_evidence(&semantic, &native, &bytes).unwrap();
    let mut wrong = native.clone();
    wrong.calls[0].owner_syntax_id = "sid:v1:wrong".into();
    assert!(s::validate_evidence(&semantic, &wrong, &bytes).is_err());
    let mut wrong = native.clone();
    wrong.calls[0].spelling = Some("other".into());
    assert!(s::validate_evidence(&semantic, &wrong, &bytes).is_err());
}
