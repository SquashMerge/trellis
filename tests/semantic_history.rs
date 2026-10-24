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
fn latest_retained_history_is_declaration_keyed_and_keeps_captured_proof() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let doc = native.revision.documents[0].key.clone();
    let syntax = semantic.declaration_bindings[0]
        .syntax_id
        .as_ref()
        .unwrap()
        .clone();
    let producer = semantic.producers[0].id.clone();
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
    let (graph, next, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let second = store
        .publish_native(&graph, &capture, &next, &leader, first, &cancel)
        .unwrap();
    let history = store
        .semantic_history_for(second, &producer, &doc, &syntax)
        .unwrap()
        .unwrap();
    assert_eq!(history.coverage, semantic.coverage[0]);
    assert_eq!(history.presentation_freshness, s::Freshness::Stale);
    assert_eq!(
        history
            .provenance
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        vec!["bind", "rel"]
    );
    assert!(
        history
            .provenance
            .iter()
            .all(|p| p.revision_id == native.revision.id && p.freshness == s::Freshness::Fresh)
    );
    let absent = "sid:v1:00000000000000000000000000000000".to_string();
    let none = store
        .semantic_history_for(second, &producer, &doc, &absent)
        .unwrap()
        .unwrap();
    assert_eq!(none.coverage, semantic.coverage[0]);
    assert!(none.provenance.is_empty());
    assert!(
        store
            .semantic_history_for(second, "different", &doc, &syntax)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .semantic_history_for(first, &producer, &doc, &syntax)
            .unwrap()
            .is_none()
    );
    let foreign = trellis::model::IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        index_revision: second.index_revision,
    };
    assert!(
        store
            .semantic_history_for(foreign, &producer, &doc, &syntax)
            .is_err()
    );
}

#[test]
fn latest_eligible_tuple_does_not_fall_back_to_older_matching_facts() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let syntax = semantic.declaration_bindings[0]
        .syntax_id
        .as_ref()
        .unwrap()
        .clone();
    let producer = semantic.producers[0].id.clone();
    let doc = native.revision.documents[0].key.clone();
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
    let mut empty = semantic.clone();
    empty.provenance.clear();
    empty.symbols.clear();
    empty.declaration_bindings.clear();
    empty.references.clear();
    empty.call_bindings.clear();
    empty.type_relationships.clear();
    empty.coverage[0].state = s::CoverageState::Partial;
    empty.coverage[0].diagnostic = Some("no declarations".into());
    empty.coverage[0].observed_roles.clear();
    store.set_retention_clock_for_tests(1_001, 1);
    let second = store
        .publish_native_semantic(
            &graph, &capture, &native, &empty, &captured, &leader, first, &cancel,
        )
        .unwrap();
    assert_ne!(first, second);
    fs::write(
        root.path().join("hello.rs"),
        "fn hello() { hello(); hello(); }\n",
    )
    .unwrap();
    let (graph, next, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let third = store
        .publish_native(&graph, &capture, &next, &leader, second, &cancel)
        .unwrap();
    let history = store
        .semantic_history_for(third, &producer, &doc, &syntax)
        .unwrap()
        .unwrap();
    assert_eq!(history.coverage, empty.coverage[0]);
    assert!(history.provenance.is_empty());
    assert_eq!(history.presentation_freshness, s::Freshness::Stale);
    assert_eq!(
        store.semantic_at(first).unwrap().declaration_bindings,
        semantic.declaration_bindings
    );
    store.set_retention_clock_for_tests(1_901, 901);
    store.release_revision(second, &leader).unwrap();
    assert!(
        store
            .semantic_history_for(third, &producer, &doc, &syntax)
            .unwrap()
            .is_none()
    );
    store.collect_unreferenced(&leader).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let db = rusqlite::Connection::open(
        state
            .path()
            .join("cache/indexes")
            .join(identity.root_key)
            .join("index.db"),
    )
    .unwrap();
    let violations: Vec<String> = db
        .prepare("PRAGMA foreign_key_check")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    assert!(store.semantic_at(second).is_err());
    assert!(store.semantic_at(first).is_err());
}

#[test]
fn same_native_id_distinct_pins_compare_full_basis_without_forcing_staleness() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let producer = &semantic.producers[0].id;
    let doc = &native.revision.documents[0].key;
    let syntax = semantic.declaration_bindings[0].syntax_id.as_ref().unwrap();
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
    assert_eq!(
        store.semantic_at(second).unwrap().declaration_bindings,
        semantic.declaration_bindings
    );
    assert!(
        store
            .semantic_history_for(second, producer, doc, syntax)
            .unwrap()
            .is_none()
    );
    let mut failed = semantic.clone();
    failed.provenance.clear();
    failed.symbols.clear();
    failed.declaration_bindings.clear();
    failed.references.clear();
    failed.call_bindings.clear();
    failed.type_relationships.clear();
    failed.coverage[0].state = s::CoverageState::Failed;
    failed.coverage[0].diagnostic = Some("producer failed".into());
    failed.coverage[0].observed_roles.clear();
    let third = store
        .publish_native_semantic(
            &graph, &capture, &native, &failed, &captured, &leader, second, &cancel,
        )
        .unwrap();
    let history = store
        .semantic_history_for(third, producer, doc, syntax)
        .unwrap()
        .unwrap();
    assert_eq!(history.coverage.revision_id, native.revision.id);
    assert_eq!(history.presentation_freshness, s::Freshness::Fresh);
    assert_eq!(history.provenance.len(), 2);
    assert!(
        store
            .semantic_at(third)
            .unwrap()
            .declaration_bindings
            .is_empty()
    );
}

#[test]
fn unchanged_bytes_new_native_id_are_possibly_stale() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let producer = &semantic.producers[0].id;
    let doc = &native.revision.documents[0].key;
    let syntax = semantic.declaration_bindings[0].syntax_id.as_ref().unwrap();
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
    let mut changed_options = options.clone();
    changed_options.max_file_bytes = 1024 * 1024;
    let (graph, next, capture) =
        index_workspace_bundle(&changed_options, store.root_id(), &cancel, |_| {}).unwrap();
    assert_ne!(next.revision.id, native.revision.id);
    assert_eq!(
        next.revision.documents[0].content_hash,
        native.revision.documents[0].content_hash
    );
    let second = store
        .publish_native(&graph, &capture, &next, &leader, first, &cancel)
        .unwrap();
    let history = store
        .semantic_history_for(second, producer, doc, syntax)
        .unwrap()
        .unwrap();
    assert_eq!(history.presentation_freshness, s::Freshness::PossiblyStale);
    assert_eq!(history.coverage.revision_id, native.revision.id);
    assert_eq!(history.provenance[0].revision_id, native.revision.id);
    assert!(
        store
            .semantic_at(second)
            .unwrap()
            .declaration_bindings
            .is_empty()
    );
}

#[test]
fn corrupted_retained_tuple_fails_typed_without_partial_history() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let producer = &semantic.producers[0].id;
    let doc = &native.revision.documents[0].key;
    let syntax = semantic.declaration_bindings[0].syntax_id.as_ref().unwrap();
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
    let (graph, next, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let second = store
        .publish_native(&graph, &capture, &next, &leader, first, &cancel)
        .unwrap();
    assert!(
        store
            .semantic_history_for(second, producer, doc, syntax)
            .unwrap()
            .is_some()
    );
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let db = rusqlite::Connection::open(
        state
            .path()
            .join("cache/indexes")
            .join(identity.root_key)
            .join("index.db"),
    )
    .unwrap();
    db.execute("UPDATE semantic_basis SET language='invalid' WHERE tuple_id=(SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1)", [format!("pin:v1:{}:{}", first.index_generation, first.index_revision)]).unwrap();
    let error = store
        .semantic_history_for(second, producer, doc, syntax)
        .unwrap_err();
    assert!(
        error.to_string().contains("invalid") || error.to_string().contains("unknown"),
        "{error:#}"
    );
}

#[test]
fn producer_version_drift_keeps_stable_id_history_partition() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let producer = &semantic.producers[0].id;
    let doc = &native.revision.documents[0].key;
    let syntax = semantic.declaration_bindings[0].syntax_id.as_ref().unwrap();
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
    let mut failed = semantic.clone();
    failed.producers[0].version = "v2".into();
    failed.coverage[0].state = s::CoverageState::Failed;
    failed.coverage[0].diagnostic = Some("producer failed".into());
    failed.coverage[0].observed_roles.clear();
    failed.provenance.clear();
    failed.symbols.clear();
    failed.declaration_bindings.clear();
    failed.references.clear();
    failed.call_bindings.clear();
    failed.type_relationships.clear();
    let mut captured_v2 = captured.clone();
    captured_v2.requested_producers = failed.producers.clone();
    let second = store
        .publish_native_semantic(
            &graph,
            &capture,
            &native,
            &failed,
            &captured_v2,
            &leader,
            first,
            &cancel,
        )
        .unwrap();
    let history = store
        .semantic_history_for(second, producer, doc, syntax)
        .unwrap()
        .unwrap();
    assert_eq!(history.coverage.revision_id, native.revision.id);
    assert_eq!(history.presentation_freshness, s::Freshness::PossiblyStale);
    assert_eq!(history.provenance.len(), 2);
    assert_eq!(
        history.provenance[0]
            .basis
            .as_ref()
            .unwrap()
            .producer_version,
        "v1"
    );
}
