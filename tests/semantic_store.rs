#![cfg(feature = "test-support")]
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    native_evidence as n, native_ids, semantic_evidence as s,
    store::Store,
};

fn native_components(
    root: &std::path::Path,
    executable_hash: &str,
    max_file_bytes: u64,
) -> (
    serde_json::Value,
    Vec<serde_json::Value>,
    Vec<serde_json::Value>,
) {
    const INPUTS: [&str; 24] = [
        "package.json",
        "tsconfig.json",
        "jsconfig.json",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lock",
        "bun.lockb",
        "Cargo.toml",
        "Cargo.lock",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
        "gradle.properties",
        "pyproject.toml",
        "requirements.txt",
        "uv.lock",
        "poetry.lock",
        "Pipfile",
        "Pipfile.lock",
        "rust-toolchain",
        "rust-toolchain.toml",
    ];
    let entry = |name: &str| match fs::read(root.join(name)) {
        Ok(bytes) => {
            json!({"path":name,"type":"present","bytes":hex::encode(&bytes),"byteLength":bytes.len()})
        }
        Err(_) => json!({"path":name,"type":"absent"}),
    };
    let dependencies = INPUTS[..22]
        .iter()
        .map(|name| entry(name))
        .collect::<Vec<_>>();
    let mut config = dependencies.clone();
    config.extend(INPUTS[22..].iter().map(|name| entry(name)));
    config.push(json!({"nativeAdmission":{"maxFileBytes":max_file_bytes}}));
    for name in [".gitignore", ".ignore"] {
        config.push(entry(name));
    }
    let toolchain = json!({"nativeExecutable":executable_hash,"rootRustToolchain":entry(INPUTS[22]),"rootRustToolchainToml":entry(INPUTS[23])});
    (toolchain, config, dependencies)
}

fn evidence(
    native: &n::Artifact,
    root: &std::path::Path,
) -> (s::SemanticPublication, s::CapturedComponents) {
    let h = |bytes: &[u8]| native_ids::digest(b"", bytes);
    let (toolchain, config, dependency) =
        native_components(root, &native.producer.executable_hash, 2 * 1024 * 1024);
    let document = native.revision.documents.first().unwrap();
    let bytes = fs::read(root.join(&document.key.path)).unwrap();
    let producer = s::Producer {
        id: "semantic.synthetic".into(),
        version: "v1".into(),
        executable_hash: h(b"exe"),
        kind: s::SemanticKind::Semantic,
        languages: vec![s::Language::Rust],
        position_encoding: s::PositionEncoding::Utf8,
    };
    let basis = s::SemanticBasis {
        producer_id: producer.id.clone(),
        producer_version: producer.version.clone(),
        producer_hash: producer.executable_hash.clone(),
        artifact_hash: h(b"artifact"),
        language: s::Language::Rust,
        source_set_id: document.key.source_set_id.clone(),
        revision_id: native.revision.id.clone(),
        source_manifest_hash: h(&native_ids::canonical(
            &json!([{"document":document.key,"contentHash":document.content_hash}]),
        )),
        toolchain_hash: native.revision.toolchain_hash.clone(),
        config_hash: native.revision.config_hash.clone(),
        dependency_hash: native.revision.dependency_hash.clone(),
        lookup_dependencies: vec!["hello".into()],
    };
    let declarations = &native.declarations;
    let declaration = declarations
        .iter()
        .find(|d| d.name.as_deref() == Some("hello") && d.name_range.is_some())
        .unwrap();
    let target = s::Target::Internal {
        syntax_id: declaration.syntax_id.clone(),
        document: document.key.clone(),
        revision_id: native.revision.id.clone(),
    };
    let external = s::Target::External {
        symbol: s::SymbolKey {
            scheme: s::SymbolScheme::Scip,
            symbol: "outside".into(),
            scope: s::SymbolScope::Global,
            document: None,
        },
    };
    let key = s::SymbolKey {
        scheme: s::SymbolScheme::Scip,
        symbol: "pkg hello".into(),
        scope: s::SymbolScope::Global,
        document: None,
    };
    let join = s::Join {
        anchor: s::MeasuredAnchor {
            document: document.key.clone(),
            revision_id: native.revision.id.clone(),
            content_hash: document.content_hash.clone(),
            range: declaration.name_range.clone().unwrap(),
            kind: s::JoinKind::DeclarationName,
        },
        status: s::JoinStatus::Exact,
        candidate_ids: vec![declaration.syntax_id.clone()],
        diagnostic: None,
    };
    let proof = |id: &str, kind| s::Provenance {
        id: id.into(),
        producer_id: producer.id.clone(),
        document: document.key.clone(),
        revision_id: native.revision.id.clone(),
        content_hash: document.content_hash.clone(),
        evidence_kind: kind,
        basis: Some(basis.clone()),
        freshness: s::Freshness::Fresh,
        derived_from: None,
    };
    let call = native
        .calls
        .iter()
        .find(|call| call.callee_range.is_some() && call.spelling.as_deref() == Some("hello"))
        .unwrap();
    let call_join = s::Join {
        anchor: s::MeasuredAnchor {
            document: document.key.clone(),
            revision_id: native.revision.id.clone(),
            content_hash: document.content_hash.clone(),
            range: call.callee_range.clone().unwrap(),
            kind: s::JoinKind::Callee,
        },
        status: s::JoinStatus::Exact,
        candidate_ids: vec![call.id.clone()],
        diagnostic: None,
    };
    let occurrence_id=native_ids::IdentityRegistry::default().occurrence(&json!({"contentHash":document.content_hash,"extractionContext":native_ids::extraction_context("rust",&[]).unwrap(),"nativeProducerId":native.producer.id,"nativeProducerVersion":native.producer.version,"ownerSyntaxId":declaration.syntax_id,"kind":"reference","ordinal":0})).unwrap();
    let reference = s::Reference {
        id: occurrence_id,
        owner_syntax_id: declaration.syntax_id.clone(),
        ordinal: 0,
        document: document.key.clone(),
        revision_id: native.revision.id.clone(),
        range: declaration.name_range.clone().unwrap(),
        spelling: "hello".into(),
        lookup_key: "hello".into(),
        site: s::ReferenceSite::Declaration,
        roles: vec![s::Role::Definition],
        resolution: s::Resolution::Resolved,
        declared_target: Some(target.clone()),
        candidates: vec![],
        provenance_id: "reference".into(),
    };
    let call_binding = s::CallBinding {
        call_id: Some(call.id.clone()),
        join: call_join,
        resolution: s::Resolution::Ambiguous,
        declared_target: None,
        candidates: vec![target.clone(), external.clone()],
        dispatch: s::Dispatch::Virtual,
        possible_dispatch: vec![target.clone(), external.clone()],
        possible_dispatch_complete: false,
        stale_target: None,
        provenance_id: "call".into(),
    };
    let semantic = s::SemanticPublication {
        producers: vec![producer.clone()],
        coverage: vec![s::Coverage {
            producer_id: producer.id.clone(),
            language: s::Language::Rust,
            source_set_id: document.key.source_set_id.clone(),
            document_path: document.key.path.clone(),
            revision_id: native.revision.id.clone(),
            requested: true,
            selected: true,
            state: s::CoverageState::Complete,
            supported_roles: vec![s::Role::Definition],
            observed_roles: vec![s::Role::Definition],
            diagnostic: None,
        }],
        provenance: vec![
            proof("bind", s::EvidenceKind::DeclarationBinding),
            proof("rel", s::EvidenceKind::TypeRelationship),
            proof("reference", s::EvidenceKind::SemanticReference),
            proof("call", s::EvidenceKind::SemanticReference),
        ],
        symbols: vec![s::Symbol {
            key: key.clone(),
            display_name: Some("hello".into()),
            declarations: vec![target.clone(), external.clone()],
            provenance_id: "bind".into(),
        }],
        declaration_bindings: vec![s::DeclarationBinding {
            syntax_id: Some(declaration.syntax_id.clone()),
            symbols: vec![key],
            join,
            provenance_id: "bind".into(),
        }],
        references: vec![reference],
        call_bindings: vec![call_binding],
        type_relationships: vec![s::TypeRelationship {
            kind: s::RelationshipKind::Overrides,
            source: target,
            target: external,
            provenance_id: "rel".into(),
        }],
    };
    let captured = s::CapturedComponents {
        producer_executables: BTreeMap::from([(producer.id.clone(), b"exe".to_vec())]),
        artifacts: BTreeMap::from([(producer.id.clone(), b"artifact".to_vec())]),
        sources: BTreeMap::from([(document.key.clone(), bytes)]),
        captured_revision: native.revision.clone(),
        native_toolchain: toolchain.clone(),
        native_config: config.clone(),
        native_dependencies: dependency.clone(),
        requested_producers: vec![producer],
        toolchain: native_ids::canonical(&toolchain),
        config: native_ids::canonical(&json!(config)),
        dependency: native_ids::canonical(&json!(dependency)),
        native_extraction_components: BTreeMap::new(),
    };
    (semantic, captured)
}

#[test]
fn direct_semantic_publication_reads_typed_rows_at_owning_pin() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("hello.rs"),
        "fn hello() { hello(); }
",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    s::validate_evidence(&semantic, &native, &captured).unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = store
        .publish_native_semantic(
            &graph,
            &capture,
            &native,
            &semantic,
            &captured,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let snapshot = store.semantic_at(pin).unwrap();
    assert_eq!(snapshot.producers, semantic.producers);
    assert_eq!(snapshot.coverage, semantic.coverage);
    assert_eq!(snapshot.provenance, semantic.provenance);
    assert_eq!(snapshot.symbols, semantic.symbols);
    assert_eq!(snapshot.declaration_bindings, semantic.declaration_bindings);
    assert_eq!(snapshot.references, semantic.references);
    assert_eq!(snapshot.call_bindings, semantic.call_bindings);
    assert_eq!(snapshot.type_relationships, semantic.type_relationships);
}

#[test]
fn native_only_next_revision_omits_requested_semantics_without_rebinding() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("hello.rs"),
        "fn hello() { hello(); }
",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let leader = store.publication_for_tests().unwrap();
    let first = store
        .publish_native_semantic(
            &graph,
            &capture,
            &native,
            &semantic,
            &captured,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    fs::write(
        root.path().join("hello.rs"),
        "fn hello() { hello(); hello(); }
",
    )
    .unwrap();
    let (graph, next, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    assert_ne!(native.revision.id, next.revision.id);
    let second = store
        .publish_native(&graph, &capture, &next, &leader, first, &cancel)
        .unwrap();
    assert_ne!(first, second);
    let earlier = store.semantic_at(first).unwrap();
    assert_eq!(earlier.declaration_bindings, semantic.declaration_bindings);
    let current = store.semantic_at(second).unwrap();
    assert!(current.provenance.is_empty());
    assert!(current.symbols.is_empty());
    assert!(current.declaration_bindings.is_empty());
    assert!(current.references.is_empty());
    assert!(current.call_bindings.is_empty());
    assert!(current.type_relationships.is_empty());
    assert_eq!(current.coverage[0].state, s::CoverageState::Omitted);
    assert_eq!(
        current.coverage[0].diagnostic.as_deref(),
        Some("semantic producer not run for this revision")
    );
    assert_eq!(current.coverage[0].revision_id, next.revision.id);
}

#[test]
fn same_native_revision_inherits_semantics_without_rebinding() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let leader = store.publication_for_tests().unwrap();
    let first = store
        .publish_native_semantic(
            &graph,
            &capture,
            &native,
            &semantic,
            &captured,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let second = store
        .publish_native(&graph, &capture, &native, &leader, first, &cancel)
        .unwrap();
    assert_ne!(first, second);
    let result = store.semantic_at(second).unwrap();
    assert_eq!(result.coverage, semantic.coverage);
    assert_eq!(result.provenance, semantic.provenance);
    assert_eq!(result.symbols, semantic.symbols);
    assert_eq!(result.declaration_bindings, semantic.declaration_bindings);
    assert_eq!(result.references, semantic.references);
    assert_eq!(result.call_bindings, semantic.call_bindings);
    assert_eq!(result.type_relationships, semantic.type_relationships);
}

#[test]
fn released_semantic_pin_drops_rows_but_retains_head() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    store.set_retention_clock_for_tests(1_000, 0);
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let leader = store.publication_for_tests().unwrap();
    let first = store
        .publish_native_semantic(
            &graph,
            &capture,
            &native,
            &semantic,
            &captured,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    store.set_retention_clock_for_tests(1_001, 1);
    let second = store
        .publish_native(&graph, &capture, &native, &leader, first, &cancel)
        .unwrap();
    assert_eq!(
        store.semantic_at(first).unwrap().references,
        semantic.references
    );
    store.set_retention_clock_for_tests(1_901, 901);
    store.release_revision(first, &leader).unwrap();
    assert!(store.semantic_at(first).is_err());
    assert_eq!(
        store.semantic_at(second).unwrap().declaration_bindings,
        semantic.declaration_bindings
    );
}

#[test]
fn multiple_producers_keep_distinct_coverage_without_spurious_facts() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let (mut semantic, mut captured) = evidence(&native, root.path());
    let mut other = semantic.producers[0].clone();
    other.id = "semantic.zother".into();
    semantic.producers.push(other.clone());
    let mut other_coverage = semantic.coverage[0].clone();
    other_coverage.producer_id = other.id.clone();
    other_coverage.requested = false;
    other_coverage.selected = false;
    other_coverage.state = s::CoverageState::NotRequested;
    other_coverage.supported_roles.clear();
    other_coverage.observed_roles.clear();
    semantic.coverage.push(other_coverage);
    captured
        .producer_executables
        .insert(other.id.clone(), b"exe".to_vec());
    captured
        .artifacts
        .insert(other.id.clone(), b"artifact".to_vec());
    captured.requested_producers.push(other);
    s::validate_evidence(&semantic, &native, &captured).unwrap();
    let leader = store.publication_for_tests().unwrap();
    let pin = store
        .publish_native_semantic(
            &graph,
            &capture,
            &native,
            &semantic,
            &captured,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let result = store.semantic_at(pin).unwrap();
    assert_eq!(result.producers, semantic.producers);
    assert_eq!(result.coverage, semantic.coverage);
    assert_eq!(result.references, semantic.references);
}

#[test]
fn fresh_native_only_publication_has_no_semantic_rows() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
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
    let result = store.semantic_at(pin).unwrap();
    assert!(result.coverage.is_empty());
    assert!(result.provenance.is_empty());
    assert!(result.symbols.is_empty());
}

#[test]
fn rejected_semantic_publication_keeps_prior_pin_and_rows() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let leader = store.publication_for_tests().unwrap();
    let first = store
        .publish_native_semantic(
            &graph,
            &capture,
            &native,
            &semantic,
            &captured,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let mut invalid = semantic.clone();
    invalid.references[0].spelling = "not-the-source".into();
    assert!(
        store
            .publish_native_semantic(
                &graph, &capture, &native, &invalid, &captured, &leader, first, &cancel
            )
            .is_err()
    );
    assert_eq!(store.index_baseline().unwrap(), first);
    assert_eq!(
        store.semantic_at(first).unwrap().references,
        semantic.references
    );
}

#[test]
fn cancelled_semantic_publication_does_not_advance_native_head() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let leader = store.publication_for_tests().unwrap();
    let before = store.index_baseline().unwrap();
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
        store
            .publish_native_semantic(
                &graph, &capture, &native, &semantic, &captured, &leader, before, &cancel
            )
            .is_err()
    );
    assert_eq!(store.index_baseline().unwrap(), before);
}
