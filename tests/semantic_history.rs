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
    let candidate = native
        .references
        .iter()
        .find(|r| r.document == document.key && Some(&r.range) == declaration.name_range.as_ref())
        .unwrap();
    let reference = s::Reference {
        id: candidate.id.clone(),
        owner_syntax_id: candidate.owner_syntax_id.clone(),
        ordinal: candidate.ordinal,
        document: candidate.document.clone(),
        revision_id: candidate.revision_id.clone(),
        range: candidate.range.clone(),
        spelling: candidate.spelling.clone(),
        lookup_key: candidate.lookup_key.clone(),
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
            supported_roles: vec![s::Role::Definition, s::Role::Call, s::Role::Type],
            observed_roles: vec![s::Role::Definition, s::Role::Call, s::Role::Type],
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
        artifacts: BTreeMap::from([(
            (producer.id.clone(), native_ids::digest(b"", b"artifact")),
            b"artifact".to_vec(),
        )]),
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
fn populated_history_partitions_documents_and_producers() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    fs::write(root.path().join("z.rs"), "fn other() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (mut semantic, mut captured) = evidence(&native, root.path());
    let other = native.revision.documents[1].key.clone();
    let doc = native.revision.documents[0].key.clone();
    let syntax = semantic.declaration_bindings[0]
        .syntax_id
        .as_ref()
        .unwrap()
        .clone();
    let other_declaration = native
        .declarations
        .iter()
        .find(|declaration| declaration.name.as_deref() == Some("other"))
        .unwrap();
    let other_target = s::Target::Internal {
        syntax_id: other_declaration.syntax_id.clone(),
        document: other.clone(),
        revision_id: native.revision.id.clone(),
    };
    let mut other_coverage = semantic.coverage[0].clone();
    other_coverage.document_path = other.path.clone();
    other_coverage.state = s::CoverageState::Partial;
    other_coverage.observed_roles = vec![s::Role::Definition];
    other_coverage.diagnostic = Some("partial other document".into());
    semantic.coverage.push(other_coverage);
    let mut other_proof = semantic.provenance[0].clone();
    other_proof.id = "other-doc-bind".into();
    other_proof.document = other.clone();
    other_proof.content_hash = native.revision.documents[1].content_hash.clone();
    semantic.provenance.push(other_proof);
    let mut other_symbol = semantic.symbols[0].clone();
    other_symbol.key.symbol = "pkg other".into();
    other_symbol.declarations = vec![other_target];
    other_symbol.provenance_id = "other-doc-bind".into();
    semantic.symbols.push(other_symbol.clone());
    let mut other_binding = semantic.declaration_bindings[0].clone();
    other_binding.syntax_id = Some(other_declaration.syntax_id.clone());
    other_binding.symbols = vec![other_symbol.key];
    other_binding.join.anchor.document = other.clone();
    other_binding.join.anchor.content_hash = native.revision.documents[1].content_hash.clone();
    other_binding.join.anchor.range = other_declaration.name_range.clone().unwrap();
    other_binding.join.candidate_ids = vec![other_declaration.syntax_id.clone()];
    other_binding.provenance_id = "other-doc-bind".into();
    semantic.declaration_bindings.push(other_binding);

    let mut second_producer = semantic.producers[0].clone();
    second_producer.id = "semantic.other".into();
    let mut second_coverage = semantic.coverage[0].clone();
    second_coverage.producer_id = second_producer.id.clone();
    second_coverage.state = s::CoverageState::Partial;
    second_coverage.observed_roles = vec![s::Role::Definition];
    second_coverage.diagnostic = Some("partial second producer".into());
    let mut second_other = semantic.coverage[1].clone();
    second_other.producer_id = second_producer.id.clone();
    second_other.state = s::CoverageState::Omitted;
    second_other.selected = false;
    second_other.diagnostic = Some("not selected".into());
    second_other.observed_roles.clear();
    semantic.coverage.extend([second_coverage, second_other]);
    semantic.producers.push(second_producer.clone());
    let mut producer_proof = semantic.provenance[0].clone();
    producer_proof.id = "other-producer-bind".into();
    producer_proof.producer_id = second_producer.id.clone();
    producer_proof.basis.as_mut().unwrap().producer_id = second_producer.id.clone();
    semantic.provenance.push(producer_proof);
    let mut producer_symbol = semantic.symbols[0].clone();
    producer_symbol.key.symbol = "pkg other producer hello".into();
    producer_symbol.provenance_id = "other-producer-bind".into();
    semantic.symbols.push(producer_symbol.clone());
    for number in 0..64 {
        let mut unrelated = semantic.symbols[0].clone();
        unrelated.key.symbol = format!("pkg unrelated {number}");
        unrelated.declarations = vec![s::Target::External {
            symbol: s::SymbolKey {
                scheme: s::SymbolScheme::Scip,
                symbol: format!("external {number}"),
                scope: s::SymbolScope::Global,
                document: None,
            },
        }];
        semantic.symbols.push(unrelated);
    }
    let mut producer_binding = semantic.declaration_bindings[0].clone();
    producer_binding.symbols = vec![producer_symbol.key];
    producer_binding.provenance_id = "other-producer-bind".into();
    semantic.declaration_bindings.push(producer_binding);
    captured.requested_producers.push(second_producer.clone());
    captured
        .producer_executables
        .insert(second_producer.id.clone(), b"exe".to_vec());
    captured.artifacts.insert(
        (
            second_producer.id.clone(),
            native_ids::digest(b"", b"artifact"),
        ),
        b"artifact".to_vec(),
    );

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
    assert_eq!(
        history
            .provenance
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        vec!["bind", "rel"]
    );
    let other_history = store
        .semantic_history_for(second, &producer, &other, &other_declaration.syntax_id)
        .unwrap()
        .unwrap();
    assert_eq!(other_history.coverage.state, s::CoverageState::Partial);
    assert_eq!(
        other_history
            .provenance
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        vec!["other-doc-bind"]
    );
    let producer_history = store
        .semantic_history_for(second, &second_producer.id, &doc, &syntax)
        .unwrap()
        .unwrap();
    assert_eq!(producer_history.coverage.state, s::CoverageState::Partial);
    assert_eq!(
        producer_history
            .provenance
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        vec!["other-producer-bind"]
    );
    assert!(
        store
            .semantic_history_for(second, &producer, &other, &syntax)
            .unwrap()
            .unwrap()
            .provenance
            .is_empty()
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
    let plan = db.prepare("EXPLAIN QUERY PLAN SELECT s.provenance_id FROM semantic_symbols s JOIN semantic_symbol_targets t ON t.symbol_id=s.symbol_id WHERE s.tuple_id=?1 AND t.kind='internal' AND t.internal_syntax_id=?2 AND t.internal_language=?3 AND t.internal_path=?4").unwrap()
        .query_map(rusqlite::params![1, vec![0_u8; 16], "rust", "hello.rs"], |row| row.get::<_, String>(3)).unwrap()
        .collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert!(
        plan.iter()
            .any(|step| step.contains("semantic_symbol_targets_internal_syntax")),
        "{plan:?}"
    );
}

#[test]
fn pending_release_excludes_history_and_released_requested_pin_expires() {
    use trellis::store::MaintenanceOutcome;
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
    store.set_retention_clock_for_tests(1_000, 0);
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
    store.set_retention_clock_for_tests(1_001, 1);
    let second = store
        .publish_native(&graph, &capture, &next, &leader, first, &cancel)
        .unwrap();
    assert!(
        store
            .semantic_history_for(second, &producer, &doc, &syntax)
            .unwrap()
            .is_some()
    );
    store.set_retention_clock_for_tests(1_901, 901);
    let permit = store.maintenance_try_enter().unwrap();
    let probe = store.open_maintenance_queue_probe().unwrap();
    assert_eq!(
        store
            .maintenance_step(&leader, &permit, &probe, || true)
            .unwrap(),
        MaintenanceOutcome::Progress
    );
    assert!(
        store
            .semantic_history_for(second, &producer, &doc, &syntax)
            .unwrap()
            .is_none()
    );
    drop(probe);
    drop(permit);
    store.maintain_revisions(&leader).unwrap();
    let error = store
        .semantic_history_for(first, &producer, &doc, &syntax)
        .unwrap_err();
    assert!(
        error.downcast_ref::<trellis::store::PinExpired>().is_some(),
        "{error:#}"
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
        error.to_string().contains("incompatible_index:"),
        "{error:#}"
    );
    assert!(
        error.to_string().contains("invalid") || error.to_string().contains("unknown"),
        "{error:#}"
    );
    assert!(store.recovery_pending_for_tests());
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

#[test]
fn no_matching_fact_still_compares_producer_basis() {
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
    let syntax = &"sid:v1:00000000000000000000000000000000".to_owned();
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
    assert!(history.provenance.is_empty());
}

#[test]
fn history_uses_requested_heads_not_captured_descriptors() {
    for comparison in [
        "version",
        "hash",
        "language",
        "unavailable",
        "empty-version",
        "empty-language",
    ] {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let options = IndexOptions::new(root.path().to_owned());
        let (graph, native, capture) =
            index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
        let (mut semantic, captured) = evidence(&native, root.path());
        let syntax = semantic.declaration_bindings[0]
            .syntax_id
            .as_ref()
            .unwrap()
            .clone();
        if comparison.starts_with("empty-") {
            semantic.coverage[0].observed_roles.clear();
            semantic.provenance.clear();
            semantic.symbols.clear();
            semantic.declaration_bindings.clear();
            semantic.references.clear();
            semantic.call_bindings.clear();
            semantic.type_relationships.clear();
        }
        let producer = &semantic.producers[0].id;
        let document = &native.revision.documents[0].key;
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
        let mut requested = captured.clone();
        match comparison {
            "version" | "empty-version" => requested.requested_producers[0].version = "v2".into(),
            "hash" => {
                requested.requested_producers[0].executable_hash =
                    native_ids::digest(b"", b"new executable")
            }
            "language" | "empty-language" => {
                requested.requested_producers[0].languages = vec![s::Language::Python]
            }
            "unavailable" => requested.requested_producers.clear(),
            _ => unreachable!(),
        }
        let mut failed = semantic.clone();
        failed.coverage[0].state = s::CoverageState::Failed;
        failed.coverage[0].diagnostic = Some("requested producer failed".into());
        failed.coverage[0].observed_roles.clear();
        failed.provenance.clear();
        failed.symbols.clear();
        failed.declaration_bindings.clear();
        failed.references.clear();
        failed.call_bindings.clear();
        failed.type_relationships.clear();
        s::validate_evidence(&failed, &native, &requested).unwrap();
        let second = store
            .publish_native_semantic(
                &graph, &capture, &native, &failed, &requested, &leader, first, &cancel,
            )
            .unwrap();
        assert_eq!(
            store.semantic_at(second).unwrap().producers,
            semantic.producers
        );
        let third = store
            .publish_native(&graph, &capture, &native, &leader, second, &cancel)
            .unwrap();
        for pin in [second, third] {
            for id in [syntax.as_str(), "sid:v1:00000000000000000000000000000000"] {
                let history = store
                    .semantic_history_for(pin, producer, document, id)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    history.presentation_freshness,
                    s::Freshness::PossiblyStale,
                    "{comparison}"
                );
                assert_eq!(history.coverage, semantic.coverage[0]);
                assert!(
                    history
                        .provenance
                        .iter()
                        .all(|proof| proof.freshness == s::Freshness::Fresh)
                );
            }
        }
        assert_eq!(
            store.semantic_at(first).unwrap().provenance,
            semantic.provenance
        );
    }
}
