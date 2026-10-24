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

fn evidence_for(
    native: &n::Artifact,
    root: &std::path::Path,
    path: &str,
) -> (s::SemanticPublication, s::CapturedComponents) {
    let h = |bytes: &[u8]| native_ids::digest(b"", bytes);
    let (toolchain, config, dependency) =
        native_components(root, &native.producer.executable_hash, 2 * 1024 * 1024);
    let document = native
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == path)
        .unwrap();
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
        source_manifest_hash: h(&native_ids::canonical(&json!(
            native
                .revision
                .documents
                .iter()
                .map(|d| json!({"document":d.key,"contentHash":d.content_hash}))
                .collect::<Vec<_>>()
        ))),
        toolchain_hash: native.revision.toolchain_hash.clone(),
        config_hash: native.revision.config_hash.clone(),
        dependency_hash: native.revision.dependency_hash.clone(),
        lookup_dependencies: vec!["hello".into()],
    };
    let declarations = native
        .declarations
        .iter()
        .filter(|d| d.document == document.key)
        .collect::<Vec<_>>();
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
        symbol: format!("pkg {path} hello"),
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
        id: format!("{path}:{id}"),
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
        .find(|call| {
            call.document == document.key
                && call.callee_range.is_some()
                && call.spelling.as_deref() == Some("hello")
        })
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
        provenance_id: format!("{path}:reference"),
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
        provenance_id: format!("{path}:call"),
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
            provenance_id: format!("{path}:bind"),
        }],
        declaration_bindings: vec![s::DeclarationBinding {
            syntax_id: Some(declaration.syntax_id.clone()),
            symbols: vec![key],
            join,
            provenance_id: format!("{path}:bind"),
        }],
        references: vec![reference],
        call_bindings: vec![call_binding],
        type_relationships: vec![s::TypeRelationship {
            kind: s::RelationshipKind::Overrides,
            source: target,
            target: external,
            provenance_id: format!("{path}:rel"),
        }],
    };
    let captured = s::CapturedComponents {
        producer_executables: BTreeMap::from([(producer.id.clone(), b"exe".to_vec())]),
        artifacts: BTreeMap::from([(producer.id.clone(), b"artifact".to_vec())]),
        sources: native
            .revision
            .documents
            .iter()
            .map(|d| (d.key.clone(), fs::read(root.join(&d.key.path)).unwrap()))
            .collect(),
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

fn evidence(
    native: &n::Artifact,
    root: &std::path::Path,
) -> (s::SemanticPublication, s::CapturedComponents) {
    evidence_for(native, root, "hello.rs")
}

fn setup() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Store,
    Arc<AtomicBool>,
    IndexOptions,
) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    (state, root, store, cancel, options)
}

#[test]
fn streamed_semantics_publish_at_owning_pin_and_failed_transaction_preserves_head() {
    let (_state, root, store, cancel, options) = setup();
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
    let prior = store.semantic_at(first).unwrap();
    let next_capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let (next, next_components) = evidence(&native, root.path());
    let error = store
        .publish_stream_semantic_for_tests(
            &options,
            &next_capture,
            &next,
            &next_components,
            &leader,
            &cancel,
            true,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected semantic stream failure"),
        "{error:#}"
    );
    assert_eq!(store.status().unwrap().revision, first);
    assert_eq!(store.semantic_at(first).unwrap(), prior);
    let fresh_capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let second = store
        .publish_stream_semantic_for_tests(
            &options,
            &fresh_capture,
            &next,
            &next_components,
            &leader,
            &cancel,
            false,
        )
        .unwrap();
    assert_ne!(first, second);
    let current = store.semantic_at(second).unwrap();
    assert_eq!(current.references, semantic.references);
    assert_eq!(current.call_bindings, semantic.call_bindings);
    assert_eq!(current.coverage, semantic.coverage);
    assert_eq!(store.semantic_at(first).unwrap(), prior);
}

#[test]
fn selective_semantics_share_native_transaction_and_reject_bad_join() {
    let (_state, root, store, cancel, options) = setup();
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (semantic, captured) = evidence(&native, root.path());
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
    let error = store
        .publish_selective_semantic_for_tests(
            &graph, &capture, &native, &semantic, &captured, &leader, &cancel, true,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected semantic selective failure"),
        "{error:#}"
    );
    assert_eq!(store.status().unwrap().revision, first);
    assert!(store.semantic_at(first).unwrap().coverage.is_empty());
    let second = store
        .publish_selective_semantic_for_tests(
            &graph, &capture, &native, &semantic, &captured, &leader, &cancel, false,
        )
        .unwrap();
    assert_eq!(
        store.semantic_at(second).unwrap().references,
        semantic.references
    );
    let mut false_join = semantic.clone();
    false_join.declaration_bindings[0].join.status = s::JoinStatus::Ambiguous;
    false_join.declaration_bindings[0].join.diagnostic = Some("claimed ambiguity".into());
    let error = store
        .publish_selective_semantic_for_tests(
            &graph,
            &capture,
            &native,
            &false_join,
            &captured,
            &leader,
            &cancel,
            false,
        )
        .unwrap_err();
    assert!(error.to_string().contains("join"), "{error:#}");
    assert_eq!(store.status().unwrap().revision, second);
    assert_eq!(
        store.semantic_at(second).unwrap().references,
        semantic.references
    );
}

#[test]
fn private_stage_admits_typed_evidence_without_changing_live_head() {
    let (_state, root, store, cancel, options) = setup();
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let leader = store.publication_for_tests().unwrap();
    let head = store
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
    let live = store.semantic_at(head).unwrap();
    drop(leader);
    let stage_capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let error = store
        .publish_private_stage_semantic_for_tests(
            &options,
            &stage_capture,
            &semantic,
            &captured,
            &cancel,
            true,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected semantic stage failure"),
        "{error:#}"
    );
    assert_eq!(store.index_baseline().unwrap(), head);
    let next_capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let observation = store
        .publish_private_stage_semantic_for_tests(
            &options,
            &next_capture,
            &semantic,
            &captured,
            &cancel,
            false,
        )
        .unwrap();
    let staged_pin = observation.pin;
    let staged = observation.semantic;
    let staged_revision = observation.native_revision;
    let staged_declarations = observation.declarations;
    let staged_calls = observation.calls;
    assert_ne!(staged_pin.index_generation, head.index_generation);
    assert_eq!(staged_pin.index_revision, 1);
    assert_eq!(staged_revision, native.revision);
    let binding = &staged.declaration_bindings[0];
    let candidate = &binding.join.candidate_ids[0];
    let declaration = staged_declarations
        .iter()
        .find(|declaration| &declaration.syntax_id == candidate)
        .unwrap();
    assert_eq!(declaration.document, binding.join.anchor.document);
    assert_eq!(declaration.revision_id, staged_revision.id);
    assert_eq!(
        declaration.name_range.as_ref(),
        Some(&binding.join.anchor.range)
    );
    let reference = &staged.references[0];
    assert_eq!(reference.owner_syntax_id, declaration.syntax_id);
    assert_eq!(reference.document, declaration.document);
    assert_eq!(reference.revision_id, staged_revision.id);
    let expected_target = s::Target::Internal {
        syntax_id: declaration.syntax_id.clone(),
        document: declaration.document.clone(),
        revision_id: staged_revision.id.clone(),
    };
    assert_eq!(reference.declared_target.as_ref(), Some(&expected_target));
    let call_binding = &staged.call_bindings[0];
    let call = staged_calls
        .iter()
        .find(|call| Some(&call.id) == call_binding.call_id.as_ref())
        .unwrap();
    assert_eq!(call.document, call_binding.join.anchor.document);
    assert_eq!(call.revision_id, staged_revision.id);
    assert_eq!(
        call.callee_range.as_ref(),
        Some(&call_binding.join.anchor.range)
    );
    assert_eq!(call_binding.join.candidate_ids, vec![call.id.clone()]);
    assert!(call_binding.candidates.contains(&expected_target));
    assert_eq!(staged.coverage, semantic.coverage);
    assert_eq!(staged.provenance, semantic.provenance);
    assert_eq!(staged.references, semantic.references);
    assert_eq!(staged.call_bindings, semantic.call_bindings);
    assert_eq!(staged.declaration_bindings, semantic.declaration_bindings);
    assert_eq!(staged.type_relationships, semantic.type_relationships);
    assert_eq!(store.index_baseline().unwrap(), head);
    assert_eq!(store.semantic_at(head).unwrap(), live);
}

#[test]
fn changed_local_semantics_bind_new_revision_and_keep_old_pin_isolated() {
    let (_state, root, store, cancel, options) = setup();
    fs::write(root.path().join("other.rs"), "fn hello() { hello(); }\n").unwrap();
    fs::write(
        root.path().join("hello.rs"),
        "fn hello() { hello(); } fn number() -> i32 { 1 }\n",
    )
    .unwrap();
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (mut semantic, captured) = evidence(&native, root.path());
    let (other, _) = evidence_for(&native, root.path(), "other.rs");
    semantic.coverage.extend(other.coverage);
    semantic.provenance.extend(other.provenance);
    semantic.symbols.extend(other.symbols);
    semantic
        .declaration_bindings
        .extend(other.declaration_bindings);
    semantic.references.extend(other.references);
    semantic.call_bindings.extend(other.call_bindings);
    semantic.type_relationships.extend(other.type_relationships);
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
        "fn hello() { hello(); } fn number() -> i32 { 2 }\n",
    )
    .unwrap();
    let (new_graph, new_native, _) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let new_capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
    assert_ne!(native.revision.id, new_native.revision.id);
    let (mut new_semantic, new_components) = evidence(&new_native, root.path());
    let (reused, _) = evidence_for(&new_native, root.path(), "other.rs");
    new_semantic.coverage.extend(reused.coverage);
    new_semantic.provenance.extend(reused.provenance);
    new_semantic.symbols.extend(reused.symbols);
    new_semantic
        .declaration_bindings
        .extend(reused.declaration_bindings);
    new_semantic.references.extend(reused.references);
    new_semantic.call_bindings.extend(reused.call_bindings);
    new_semantic
        .type_relationships
        .extend(reused.type_relationships);
    let err = store
        .publish_local_semantic_for_tests(
            &options,
            &new_capture,
            &new_semantic,
            &new_components,
            &leader,
            &cancel,
            true,
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("injected semantic local failure"),
        "{err:#}"
    );
    assert_eq!(store.status().unwrap().revision, first);
    assert_eq!(
        store.semantic_at(first).unwrap().references,
        semantic.references
    );
    let retry_capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let second = store
        .publish_local_semantic_for_tests(
            &options,
            &retry_capture,
            &new_semantic,
            &new_components,
            &leader,
            &cancel,
            false,
        )
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(
        store.semantic_at(first).unwrap().references,
        semantic.references
    );
    assert_eq!(
        store.semantic_at(second).unwrap().references,
        new_semantic.references
    );
    let current = store.semantic_at(second).unwrap();
    assert_eq!(current.coverage.len(), 2);
    assert!(
        current
            .coverage
            .iter()
            .all(|c| c.revision_id == new_native.revision.id)
    );
    assert_eq!(
        current.declaration_bindings,
        new_semantic.declaration_bindings
    );
    assert_eq!(current.call_bindings, new_semantic.call_bindings);
    assert_eq!(current.type_relationships, new_semantic.type_relationships);
    assert_eq!(current.provenance, new_semantic.provenance);
    let previous = store.semantic_at(first).unwrap();
    assert_eq!(previous.declaration_bindings, semantic.declaration_bindings);
    assert_eq!(previous.call_bindings, semantic.call_bindings);
    assert_eq!(previous.type_relationships, semantic.type_relationships);
    assert_ne!(
        current.references[0].revision_id,
        previous.references[0].revision_id
    );
    assert!(
        current
            .references
            .iter()
            .any(|r| r.document.path == "other.rs")
    );
    assert!(
        previous
            .references
            .iter()
            .any(|r| r.document.path == "other.rs")
    );
    let old_other = native
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == "other.rs")
        .unwrap();
    let new_other = new_native
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == "other.rs")
        .unwrap();
    assert_eq!(old_other.content_hash, new_other.content_hash);
    assert_eq!(
        previous
            .references
            .iter()
            .find(|r| r.document.path == "other.rs")
            .unwrap()
            .id,
        current
            .references
            .iter()
            .find(|r| r.document.path == "other.rs")
            .unwrap()
            .id
    );
    assert_ne!(
        new_graph
            .files
            .iter()
            .find(|f| f.path == "hello.rs")
            .unwrap()
            .hash,
        graph
            .files
            .iter()
            .find(|f| f.path == "hello.rs")
            .unwrap()
            .hash
    );
}

#[test]
fn streamed_new_native_revision_omits_old_facts_and_keeps_producers_separate() {
    let (_state, root, store, cancel, options) = setup();
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (mut semantic, mut captured) = evidence(&native, root.path());
    let mut other = semantic.producers[0].clone();
    other.id = "semantic.other".into();
    semantic.producers.push(other.clone());
    let mut not_requested = semantic.coverage[0].clone();
    not_requested.producer_id = other.id.clone();
    not_requested.requested = false;
    not_requested.selected = false;
    not_requested.state = s::CoverageState::NotRequested;
    not_requested.supported_roles.clear();
    not_requested.observed_roles.clear();
    semantic.coverage.push(not_requested);
    captured
        .producer_executables
        .insert(other.id.clone(), b"exe".to_vec());
    captured
        .artifacts
        .insert(other.id.clone(), b"artifact".to_vec());
    captured.requested_producers.push(other);
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
        "fn hello() { hello(); hello(); }\n",
    )
    .unwrap();
    let capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let second = store.publish_stream_semantic_for_tests(
        &options,
        &capture,
        &s::SemanticPublication {
            producers: vec![],
            coverage: vec![],
            provenance: vec![],
            symbols: vec![],
            declaration_bindings: vec![],
            references: vec![],
            call_bindings: vec![],
            type_relationships: vec![],
        },
        &captured,
        &leader,
        &cancel,
        false,
    );
    // A typed empty input cannot impersonate a complete producer manifest.
    assert!(second.is_err());
    assert_eq!(store.status().unwrap().revision, first);
    let capture = trellis::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let native_only = store
        .publish_stream_native_only_for_tests(&options, &capture, &leader, &cancel)
        .unwrap();
    let current = store.semantic_at(native_only).unwrap();
    assert!(current.references.is_empty());
    assert!(current.call_bindings.is_empty());
    assert_eq!(current.coverage.len(), 2);
    assert_eq!(current.coverage[0].state, s::CoverageState::Omitted);
    assert_eq!(current.coverage[1].state, s::CoverageState::NotRequested);
    assert_ne!(current.coverage[0].revision_id, native.revision.id);
    assert_eq!(
        store.semantic_at(first).unwrap().references,
        semantic.references
    );
}
