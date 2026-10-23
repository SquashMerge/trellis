use serde_json::json;
use std::collections::BTreeMap;
use trellis::{native_evidence as n, native_ids, semantic_evidence as s};

fn fixture() -> (s::SemanticPublication, n::Artifact, s::CapturedComponents) {
    let bytes = b"fn hello() { hello(); }".to_vec();
    let h = |b: &[u8]| native_ids::digest(b"", b);
    let document = n::DocumentKey {
        source_set_id: "source-set:v1:root".into(),
        language: "rust".into(),
        path: "src/main.rs".into(),
    };
    let revision = n::Revision {
        id: "revision-1".into(),
        source_set_id: document.source_set_id.clone(),
        documents: vec![n::Document {
            key: document.clone(),
            revision_id: "revision-1".into(),
            content_hash: h(&bytes),
            byte_length: bytes.len(),
        }],
        toolchain_hash: h(b"tool"),
        config_hash: h(b"config"),
        dependency_hash: h(b"dependency"),
    };
    let producer = n::Producer {
        id: "trellis.native.syntax".into(),
        version: "native-v8".into(),
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
        range: n::Range { start: 0, end: 24 },
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
    let native = n::Artifact {
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
    let reference_id = registry.occurrence(&json!({"contentHash":h(&bytes),"extractionContext":native_ids::extraction_context("rust",&[]).unwrap(),"nativeProducerId":native.producer.id,"nativeProducerVersion":native.producer.version,"ownerSyntaxId":syntax_id,"kind":"reference","ordinal":0})).unwrap();
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
            supported_roles: vec![s::Role::Read, s::Role::Call],
            observed_roles: vec![s::Role::Read, s::Role::Call],
            diagnostic: None,
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
            ordinal: 0,
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
        artifacts: BTreeMap::from([("semantic".into(), b"semantic-artifact".to_vec())]),
        sources: BTreeMap::from([(document, bytes)]),
        captured_revision: revision,
        toolchain: b"tool".to_vec(),
        config: b"config".to_vec(),
        dependency: b"dependency".to_vec(),
        native_extraction_components: BTreeMap::new(),
    };
    (semantic, native, captured)
}
#[test]
fn all_semantic_families_are_admitted_against_exact_native_bytes() {
    let (semantic, native, bytes) = fixture();
    s::validate_evidence(&semantic, &native, &bytes).unwrap();
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
            .contains("noncanonical reference")
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
