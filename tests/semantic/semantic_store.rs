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
    semantic_import::DestinationMutations,
    store::Store,
};

use crate::semantic_batch_support;
use semantic_batch_support::{BatchFixture, DocSpec};

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
            (
                producer.id.clone(),
                trellis::native_ids::digest(b"", b"artifact"),
            ),
            b"artifact".to_vec(),
        )]),
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
    let (retained, sources) = store.retained_native_snapshot_at(pin).unwrap();
    assert_eq!(retained.revision, native.revision);
    assert_eq!(retained.declarations.len(), native.declarations.len());
    assert!(
        native
            .declarations
            .iter()
            .all(|d| retained.declarations.contains(d))
    );
    assert_eq!(retained.references.len(), native.references.len());
    assert!(
        native
            .references
            .iter()
            .all(|r| retained.references.contains(r))
    );
    assert_eq!(
        sources[&native.revision.documents[0].key],
        fs::read(root.path().join("hello.rs")).unwrap()
    );
    assert_eq!(snapshot.producers, semantic.producers);
    assert_eq!(snapshot.coverage, semantic.coverage);
    assert_eq!(snapshot.provenance, semantic.provenance);
    assert_eq!(snapshot.symbols, semantic.symbols);
    assert_eq!(snapshot.declaration_bindings, semantic.declaration_bindings);
    assert_eq!(snapshot.references, semantic.references);
    assert_eq!(snapshot.call_bindings, semantic.call_bindings);
    assert_eq!(snapshot.type_relationships, semantic.type_relationships);
    let foreign = trellis::model::IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        index_revision: pin.index_revision,
    };
    let error = store.semantic_at(foreign).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("revision conflict: foreign or missing native pin"),
        "{error:#}"
    );
    assert_eq!(store.status().unwrap().revision, pin);
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
    captured.artifacts.insert(
        (
            other.id.clone(),
            trellis::native_ids::digest(b"", b"artifact"),
        ),
        b"artifact".to_vec(),
    );
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
fn reference_role_removed_from_both_lists_rejects_without_advancing_pin() {
    assert_semantic_rejection_keeps_pin("fn hello() { hello(); }\n", |semantic, _| {
        semantic.coverage[0]
            .supported_roles
            .retain(|r| *r != s::Role::Read);
        semantic.coverage[0]
            .observed_roles
            .retain(|r| *r != s::Role::Read);
    });
}

#[test]
fn exact_call_role_removed_from_both_lists_rejects_without_advancing_pin() {
    assert_semantic_rejection_keeps_pin("fn hello() { hello(); }\n", |semantic, _| {
        semantic.coverage[0]
            .supported_roles
            .retain(|r| *r != s::Role::Call);
        semantic.coverage[0]
            .observed_roles
            .retain(|r| *r != s::Role::Call);
    });
}

#[test]
fn reference_outside_owning_declaration_rejects_without_advancing_pin() {
    let source = "fn hello() { hello(); }\nfn other() { hello(); }\n";
    assert_semantic_rejection_keeps_pin(source, |semantic, native| {
        let reference = &mut semantic.references[0];
        let start = source.rfind("hello").unwrap();
        reference.range = n::Range {
            start,
            end: start + 5,
        };
        reference.site = s::ReferenceSite::Use;
        reference.roles = vec![s::Role::Read];
        let owner = native
            .declarations
            .iter()
            .find(|d| d.syntax_id == reference.owner_syntax_id)
            .unwrap();
        assert!(reference.range.start >= owner.range.end);
        let document = &native.revision.documents[0];
        let claimed_id = native_ids::IdentityRegistry::default()
            .occurrence(&json!({
                "contentHash": document.content_hash,
                "extractionContext": native_ids::extraction_context("rust", &[]).unwrap(),
                "nativeProducerId": native.producer.id,
                "nativeProducerVersion": native.producer.version,
                "ownerSyntaxId": reference.owner_syntax_id,
                "kind": "reference",
                "ordinal": reference.ordinal
            }))
            .unwrap();
        assert_eq!(reference.id, claimed_id);
    });
}

#[test]
fn empty_coverage_diagnostic_rejects_without_advancing_pin() {
    assert_semantic_rejection_keeps_pin("fn hello() { hello(); }\n", |semantic, _| {
        semantic.coverage[0].diagnostic = Some(String::new());
    });
}

#[test]
fn empty_declaration_join_diagnostic_rejects_without_advancing_pin() {
    assert_semantic_rejection_keeps_pin("fn hello() { hello(); }\n", |semantic, _| {
        semantic.declaration_bindings[0].join.diagnostic = Some(String::new());
    });
}

#[test]
fn empty_call_join_diagnostic_rejects_without_advancing_pin() {
    assert_semantic_rejection_keeps_pin("fn hello() { hello(); }\n", |semantic, _| {
        semantic.call_bindings[0].join.diagnostic = Some(String::new());
    });
}

fn assert_semantic_rejection_keeps_pin(
    source: &str,
    invalidate: impl FnOnce(&mut s::SemanticPublication, &n::Artifact),
) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), source).unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().to_owned()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let (mut semantic, captured) = evidence(&native, root.path());
    let reference = &mut semantic.references[0];
    let call = native
        .calls
        .iter()
        .find(|c| c.document == reference.document && c.callee_range.is_some())
        .unwrap();
    let candidate = native
        .references
        .iter()
        .find(|r| {
            r.document == call.document
                && r.owner_syntax_id == call.owner_syntax_id
                && Some(&r.range) == call.callee_range.as_ref()
        })
        .unwrap();
    reference.id = candidate.id.clone();
    reference.owner_syntax_id = candidate.owner_syntax_id.clone();
    reference.ordinal = candidate.ordinal;
    reference.range = candidate.range.clone();
    reference.site = s::ReferenceSite::Use;
    reference.roles = vec![s::Role::Read];
    semantic.coverage[0].supported_roles.push(s::Role::Read);
    semantic.coverage[0].supported_roles.sort();
    semantic.coverage[0].observed_roles.push(s::Role::Read);
    semantic.coverage[0].observed_roles.sort();
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
    let before = store.semantic_at(first).unwrap();
    let mut invalid = semantic;
    invalidate(&mut invalid, &native);
    assert!(s::validate_evidence(&invalid, &native, &captured).is_err());
    assert!(
        store
            .publish_native_semantic(
                &graph, &capture, &native, &invalid, &captured, &leader, first, &cancel,
            )
            .is_err()
    );
    assert_eq!(store.index_baseline().unwrap(), first);
    assert_eq!(store.semantic_at(first).unwrap(), before);
}

#[test]
fn factless_proof_from_prior_native_revision_cannot_publish_as_current() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
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
    let prior = store.semantic_at(first).unwrap();

    fs::write(
        root.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = 'stable'\n",
    )
    .unwrap();
    let (next_graph, next_native, next_capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    assert_ne!(native.revision.id, next_native.revision.id);
    let mut stale = semantic.clone();
    stale.coverage[0].revision_id = next_native.revision.id.clone();
    stale.coverage[0].observed_roles.clear();
    stale.provenance.truncate(1);
    stale.provenance[0].freshness = s::Freshness::PossiblyStale;
    stale.symbols.clear();
    stale.declaration_bindings.clear();
    stale.references.clear();
    stale.call_bindings.clear();
    stale.type_relationships.clear();
    assert!(stale.provenance[0].derived_from.is_none());
    s::validate_evidence(&stale, &next_native, &captured).unwrap();

    let error = store
        .publish_native_semantic(
            &next_graph,
            &next_capture,
            &next_native,
            &stale,
            &captured,
            &leader,
            first,
            &cancel,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("captured proof revision differs from candidate native revision"),
        "{error:#}"
    );
    assert_eq!(store.index_baseline().unwrap(), first);
    assert_eq!(store.status().unwrap().revision, first);
    assert_eq!(store.semantic_at(first).unwrap(), prior);
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
    let second_key = format!(
        "pin:v1:{}:{}",
        first.index_generation,
        first.index_revision + 1
    );
    let native_rows: i64 = db
        .query_row(
            "SELECT count(*) FROM native_revisions WHERE id=?1",
            [&second_key],
            |row| row.get(0),
        )
        .unwrap();
    let semantic_rows: i64 = db
        .query_row(
            "SELECT count(*) FROM semantic_coverage WHERE revision_id=?1",
            [&second_key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!((native_rows, semantic_rows), (0, 0));
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

#[test]
fn coverage_states_roundtrip_without_borrowing_semantic_facts() {
    for state in [
        s::CoverageState::Partial,
        s::CoverageState::Failed,
        s::CoverageState::Unsupported,
        s::CoverageState::Omitted,
        s::CoverageState::NotRequested,
    ] {
        let state_dir = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
        let store = Store::open_for_tests(state_dir.path(), root.path()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(root.path().to_owned()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let (mut semantic, captured) = evidence(&native, root.path());
        let coverage = &mut semantic.coverage[0];
        coverage.state = state;
        coverage.requested = state != s::CoverageState::NotRequested;
        coverage.selected = matches!(state, s::CoverageState::Partial | s::CoverageState::Failed);
        coverage.diagnostic =
            (state != s::CoverageState::NotRequested).then(|| "diagnostic".into());
        if state != s::CoverageState::Partial {
            coverage.observed_roles.clear();
            semantic.provenance.clear();
            semantic.symbols.clear();
            semantic.declaration_bindings.clear();
            semantic.references.clear();
            semantic.call_bindings.clear();
            semantic.type_relationships.clear();
        }
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
        let actual = store.semantic_at(pin).unwrap();
        assert_eq!(actual.coverage, semantic.coverage, "{state:?}");
        assert_eq!(actual.provenance, semantic.provenance, "{state:?}");
        assert_eq!(actual.symbols, semantic.symbols, "{state:?}");
        assert_eq!(
            actual.declaration_bindings, semantic.declaration_bindings,
            "{state:?}"
        );
        assert_eq!(actual.references, semantic.references, "{state:?}");
        assert_eq!(actual.call_bindings, semantic.call_bindings, "{state:?}");
        assert_eq!(
            actual.type_relationships, semantic.type_relationships,
            "{state:?}"
        );
        assert_eq!(actual.coverage[0].revision_id, native.revision.id);
    }
}

#[test]
fn typed_next_revision_fills_unsupplied_producer_new_document() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (mut semantic, mut captured) = evidence(&native, root.path());
    let mut other = semantic.producers[0].clone();
    other.id = "semantic.zother".into();
    semantic.producers.push(other.clone());
    let mut old_other = semantic.coverage[0].clone();
    old_other.producer_id = other.id.clone();
    old_other.requested = false;
    old_other.selected = false;
    old_other.state = s::CoverageState::NotRequested;
    old_other.supported_roles.clear();
    old_other.observed_roles.clear();
    semantic.coverage.push(old_other);
    captured
        .producer_executables
        .insert(other.id.clone(), b"exe".to_vec());
    captured.artifacts.insert(
        (
            other.id.clone(),
            trellis::native_ids::digest(b"", b"artifact"),
        ),
        b"artifact".to_vec(),
    );
    captured.requested_producers.push(other.clone());
    let mut requested = semantic.producers[0].clone();
    requested.id = "semantic.zrequested".into();
    semantic.producers.push(requested.clone());
    let mut omitted = semantic.coverage[0].clone();
    omitted.producer_id = requested.id.clone();
    omitted.selected = false;
    omitted.state = s::CoverageState::Omitted;
    omitted.observed_roles.clear();
    omitted.diagnostic = Some("not run".into());
    semantic.coverage.push(omitted);
    captured
        .producer_executables
        .insert(requested.id.clone(), b"exe".to_vec());
    captured.artifacts.insert(
        (
            requested.id.clone(),
            trellis::native_ids::digest(b"", b"artifact"),
        ),
        b"artifact".to_vec(),
    );
    captured.requested_producers.push(requested.clone());
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
    fs::write(root.path().join("zz.rs"), "fn zz() {}\n").unwrap();
    let (graph, next, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    assert_ne!(native.revision.id, next.revision.id);
    let (mut supplied, mut captured) = evidence(&next, root.path());
    let new_document = next
        .revision
        .documents
        .iter()
        .find(|d| d.key.path == "zz.rs")
        .unwrap();
    let mut new_coverage = supplied.coverage[0].clone();
    new_coverage.document_path = new_document.key.path.clone();
    new_coverage.requested = false;
    new_coverage.selected = false;
    new_coverage.state = s::CoverageState::NotRequested;
    new_coverage.supported_roles.clear();
    new_coverage.observed_roles.clear();
    supplied.coverage.push(new_coverage);
    captured.sources.insert(
        new_document.key.clone(),
        fs::read(root.path().join("zz.rs")).unwrap(),
    );
    let manifest = json!(
        next.revision
            .documents
            .iter()
            .map(|d| json!({"document":d.key,"contentHash":d.content_hash}))
            .collect::<Vec<_>>()
    );
    let digest = native_ids::digest(b"", &native_ids::canonical(&manifest));
    for proof in &mut supplied.provenance {
        proof.basis.as_mut().unwrap().source_manifest_hash = digest.clone();
    }
    s::validate_evidence(&supplied, &next, &captured).unwrap();
    let second = store
        .publish_native_semantic(
            &graph, &capture, &next, &supplied, &captured, &leader, first, &cancel,
        )
        .unwrap();
    let actual = store.semantic_at(second).unwrap();
    assert_eq!(actual.coverage.len(), 6);
    assert_eq!(
        actual
            .coverage
            .iter()
            .filter(|c| c.producer_id == other.id)
            .count(),
        2
    );
    assert!(actual.coverage.iter().any(|c| c.producer_id == other.id
        && c.document_path == "zz.rs"
        && c.state == s::CoverageState::NotRequested));
    assert!(actual.coverage.iter().any(|c| c.producer_id == other.id
        && c.document_path == "hello.rs"
        && c.state == s::CoverageState::NotRequested));
    assert!(actual.coverage.iter().any(|c| c.producer_id == requested.id
        && c.document_path == "zz.rs"
        && !c.requested
        && !c.selected
        && c.state == s::CoverageState::NotRequested));
    assert!(actual.coverage.iter().any(|c| c.producer_id == requested.id
        && c.document_path == "hello.rs"
        && c.requested
        && !c.selected
        && c.state == s::CoverageState::Omitted));
    assert_eq!(actual.references, supplied.references);
    assert_eq!(
        store.semantic_at(first).unwrap().coverage,
        semantic.coverage
    );
    drop(leader);
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
    let vid = |pin: trellis::model::IndexPin| -> i64 {
        db.query_row(
            "SELECT d.vid FROM revision_documents d WHERE d.revision_id=?1 AND d.path='hello.rs'",
            [format!(
                "pin:v1:{}:{}",
                pin.index_generation, pin.index_revision
            )],
            |r| r.get(0),
        )
        .unwrap()
    };
    let stable_vid = vid(second);
    fs::write(root.path().join("zz.rs"), "fn zz() { zz(); }\n").unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let third = trellis::index_coordinator::IndexJobCoordinator::prepare_with_session(
        &store,
        Some(second),
        session,
    )
    .unwrap()
    .run_serving(&options, &cancel, |_| {})
    .unwrap();
    assert_ne!(second, third);
    assert_eq!(
        vid(third),
        stable_vid,
        "unchanged document reused native vid"
    );
    let final_sem = store.semantic_at(third).unwrap();
    assert_eq!(final_sem.coverage.len(), 6);
    assert!(
        final_sem
            .coverage
            .iter()
            .any(|c| c.producer_id == "semantic.synthetic"
                && c.document_path == "hello.rs"
                && c.state == s::CoverageState::Omitted)
    );
    assert!(final_sem.coverage.iter().any(|c| c.producer_id == other.id
        && c.document_path == "zz.rs"
        && c.state == s::CoverageState::NotRequested));
    assert!(
        final_sem
            .coverage
            .iter()
            .any(|c| c.producer_id == requested.id
                && c.document_path == "zz.rs"
                && c.state == s::CoverageState::NotRequested)
    );
    assert!(final_sem.references.is_empty());
    assert_eq!(
        store.semantic_at(second).unwrap().references,
        supplied.references
    );
}

#[test]
fn malformed_nullable_targets_fail_typed_selected_read_and_schedule_recovery() {
    use rusqlite::Connection;
    for table in ["semantic_references", "semantic_call_bindings"] {
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
        let identity =
            trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
                .unwrap();
        let db = Connection::open(
            state
                .path()
                .join("cache/indexes")
                .join(identity.root_key)
                .join("index.db"),
        )
        .unwrap();
        let sql = format!(
            "UPDATE {table} SET declared_kind=NULL, declared_internal_syntax_id=NULL, declared_internal_language=NULL, declared_internal_path=NULL, declared_internal_native_revision_id=NULL, declared_external_scheme=NULL, declared_external_symbol='corrupt', declared_external_scope=NULL, declared_external_language=NULL, declared_external_path=NULL"
        );
        assert!(
            db.execute(&sql, []).is_err(),
            "nullable target constraint on {table}"
        );
        db.pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        assert_eq!(db.execute(&sql, []).unwrap(), 1);
        db.pragma_update(None, "ignore_check_constraints", false)
            .unwrap();
        drop(db);
        let error = store.semantic_at(pin).unwrap_err();
        assert!(
            error.to_string().contains("incompatible_index:"),
            "{error:#}"
        );
        assert!(
            error
                .to_string()
                .contains("corrupt semantic target payload"),
            "{error:#}"
        );
        assert!(store.recovery_pending_for_tests());
        assert_eq!(
            store.status().unwrap_err().to_string(),
            "incompatible_index: reconciliation required after invalid current index"
        );
    }
}

#[test]
fn semantic_change_on_stable_native_id_and_precommit_failures_are_atomic() {
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
    let mut changed = semantic.clone();
    changed.symbols[0].display_name = Some("different display".into());
    s::validate_evidence(&changed, &native, &captured).unwrap();
    let failed = store
        .publish_native_semantic_with_sql_fault_for_tests(
            (&graph, &capture, &native, &changed, &captured),
            &leader,
            first,
            &cancel,
        )
        .unwrap_err();
    assert!(
        failed.to_string().contains("NOT NULL constraint failed"),
        "{failed:#}"
    );
    assert_eq!(store.index_baseline().unwrap(), first);
    assert_eq!(store.semantic_at(first).unwrap().symbols, semantic.symbols);
    assert!(
        store
            .publish_native_semantic_with_precommit_cancel_for_tests(
                (&graph, &capture, &native, &changed, &captured),
                &leader,
                first,
                &cancel
            )
            .is_err()
    );
    assert_eq!(store.index_baseline().unwrap(), first);
    assert_eq!(store.semantic_at(first).unwrap().symbols, semantic.symbols);
    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    let second = store
        .publish_native_semantic(
            &graph, &capture, &native, &changed, &captured, &leader, first, &cancel,
        )
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(store.semantic_at(first).unwrap().symbols, semantic.symbols);
    assert_eq!(store.semantic_at(second).unwrap().symbols, changed.symbols);
    assert_eq!(
        store.semantic_at(second).unwrap().coverage[0].revision_id,
        native.revision.id
    );
}

#[test]
fn malformed_semantic_proof_join_and_target_are_rejected_without_partial_publication() {
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
    for variant in 0..8 {
        let mut invalid = semantic.clone();
        match variant {
            0 => {
                invalid.provenance[0].derived_from = Some(s::DerivedFrom {
                    provenance_id: "older".into(),
                    record_kind: s::RecordKind::Occurrence,
                    fact_digest: native_ids::digest(b"", b"older"),
                })
            }
            1 => invalid.call_bindings[0].stale_target = Some(true),
            2 => invalid.declaration_bindings[0].join.candidate_ids.clear(),
            3 => invalid.declaration_bindings[0].join.anchor.kind = s::JoinKind::Callee,
            4 => invalid.call_bindings[0].join.candidate_ids.clear(),
            5 => invalid.call_bindings[0].join.anchor.kind = s::JoinKind::DeclarationName,
            6 => {
                let candidate = invalid.declaration_bindings[0].join.candidate_ids[0].clone();
                invalid.declaration_bindings[0].join.candidate_ids =
                    vec![candidate.clone(), candidate];
                invalid.declaration_bindings[0].join.status = s::JoinStatus::Ambiguous;
                invalid.declaration_bindings[0].join.diagnostic = Some("false ambiguity".into());
            }
            7 => {
                invalid.call_bindings[0].join.status = s::JoinStatus::Ambiguous;
                invalid.call_bindings[0].join.diagnostic = Some("false ambiguity".into());
            }
            _ => unreachable!(),
        }
        assert!(
            store
                .publish_native_semantic(
                    &graph, &capture, &native, &invalid, &captured, &leader, first, &cancel
                )
                .is_err(),
            "invalid variant {variant}"
        );
        assert_eq!(
            store.index_baseline().unwrap(),
            first,
            "invalid variant {variant}"
        );
        assert_eq!(
            store.semantic_at(first).unwrap().references,
            semantic.references
        );
    }
}

#[test]
fn measured_join_reference_and_dispatch_variants_roundtrip() {
    for variant in 0..6 {
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
        let (mut semantic, captured) = evidence(&native, root.path());
        match variant {
            0 => {
                let join = &mut semantic.declaration_bindings[0].join;
                join.anchor.range = n::Range { start: 0, end: 2 };
                join.status = s::JoinStatus::Unmatched;
                join.candidate_ids.clear();
                join.diagnostic = Some("no measured declaration".into());
                semantic.declaration_bindings[0].syntax_id = None;
                semantic.declaration_bindings[0].symbols.clear();
            }
            1 => {
                let join = &mut semantic.call_bindings[0].join;
                join.anchor.range = n::Range { start: 0, end: 2 };
                join.status = s::JoinStatus::Unmatched;
                join.candidate_ids.clear();
                join.diagnostic = Some("no measured call".into());
                semantic.call_bindings[0].call_id = None;
                semantic.coverage[0]
                    .observed_roles
                    .retain(|r| *r != s::Role::Call);
            }
            2 => {
                let call = native
                    .calls
                    .iter()
                    .find(|c| Some(&c.id) == semantic.call_bindings[0].call_id.as_ref())
                    .unwrap();
                semantic.call_bindings[0].join.anchor.kind = s::JoinKind::Invocation;
                semantic.call_bindings[0].join.anchor.range = call.range.clone();
            }
            3 => {
                semantic.references[0].declared_target = None;
                semantic.references[0].resolution = s::Resolution::Unresolved;
                semantic.call_bindings[0].candidates.clear();
                semantic.call_bindings[0].resolution = s::Resolution::Unresolved;
                semantic.call_bindings[0].possible_dispatch.clear();
                semantic.call_bindings[0].dispatch = s::Dispatch::Unknown;
            }
            4 => {
                semantic.references[0].declared_target =
                    Some(semantic.symbols[0].declarations[1].clone());
                semantic.references[0].resolution = s::Resolution::External;
                semantic.call_bindings[0].possible_dispatch.truncate(1);
                semantic.call_bindings[0].dispatch = s::Dispatch::Direct;
            }
            5 => {
                semantic.references[0].declared_target = None;
                semantic.references[0].resolution = s::Resolution::Ambiguous;
                semantic.references[0].candidates = semantic.symbols[0].declarations.clone();
                semantic.call_bindings[0].declared_target =
                    Some(semantic.symbols[0].declarations[0].clone());
                semantic.call_bindings[0].resolution = s::Resolution::Resolved;
                semantic.call_bindings[0].candidates.clear();
                semantic.call_bindings[0].stale_target = Some(false);
            }
            _ => unreachable!(),
        }
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
        let selected = store.semantic_at(pin).unwrap();
        assert_eq!(
            selected.declaration_bindings, semantic.declaration_bindings,
            "variant {variant}"
        );
        assert_eq!(
            selected.call_bindings, semantic.call_bindings,
            "variant {variant}"
        );
        assert_eq!(
            selected.references, semantic.references,
            "variant {variant}"
        );
        assert_eq!(selected.coverage, semantic.coverage);
        assert_eq!(selected.coverage[0].revision_id, native.revision.id);
    }
}

#[test]
fn bounded_semantic_children_release_preserves_same_native_id_survivors() {
    use rusqlite::Connection;
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
    let (mut semantic, captured) = evidence(&native, root.path());
    let lookup = (0..65).map(|i| format!("lookup{i:03}")).collect::<Vec<_>>();
    for proof in &mut semantic.provenance {
        proof.basis.as_mut().unwrap().lookup_dependencies = lookup.clone();
    }
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
    assert_ne!(first, second);
    assert_eq!(
        store.semantic_at(first).unwrap().provenance,
        semantic.provenance
    );
    assert_eq!(
        store.semantic_at(second).unwrap().provenance,
        semantic.provenance
    );
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let db = Connection::open(
        state
            .path()
            .join("cache/indexes")
            .join(identity.root_key)
            .join("index.db"),
    )
    .unwrap();
    let count = |db: &Connection, pin: trellis::model::IndexPin| -> i64 {
        db.query_row("SELECT count(*) FROM semantic_basis_lookup_dependencies WHERE basis_id IN (SELECT basis_id FROM semantic_basis WHERE tuple_id IN (SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1))",[format!("pin:v1:{}:{}",pin.index_generation,pin.index_revision)],|r|r.get(0)).unwrap()
    };
    assert!(count(&db, first) > 64);
    let survivor_count = count(&db, second);
    store.set_retention_clock_for_tests(1_901, 901);
    assert!(store.maintain_one_revision_step_for_tests(&leader).unwrap());
    let pin_key = format!("pin:v1:{}:{}", first.index_generation, first.index_revision);
    let debt: i64 = db
        .query_row(
            "SELECT count(*) FROM native_revision_release_debt WHERE revision_id=?1",
            [&pin_key],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        debt, 1,
        "the first bounded step leaves replayable release debt"
    );
    assert!(
        count(&db, first) > 0,
        "the first bounded step cannot clear 65 children"
    );
    assert_eq!(count(&db, second), survivor_count);
    drop(leader);
    drop(store);
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    reopened.set_retention_clock_for_tests(1_901, 901);
    let leader = reopened.publication_for_tests().unwrap();
    reopened.maintain_revisions(&leader).unwrap();
    reopened.collect_unreferenced(&leader).unwrap();
    assert_eq!(count(&db, first), 0);
    assert_eq!(count(&db, second), survivor_count);
    let released_children: i64 = db.query_row(
        "SELECT count(*) FROM semantic_provenance WHERE tuple_id IN (SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1)",
        [&pin_key], |r| r.get(0),
    ).unwrap();
    assert_eq!(released_children, 0);
    let released = reopened.semantic_at(first).unwrap_err();
    assert!(
        released
            .to_string()
            .contains("pin_expired: released native revision"),
        "{released:#}"
    );
    assert_eq!(
        reopened.semantic_at(second).unwrap().provenance,
        semantic.provenance
    );
    let fk: i64 = db
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fk, 0);
}

#[test]
fn full_stream_unchanged_capture_keeps_typed_semantic_pin() {
    use trellis::index_coordinator::IndexJobCoordinator;
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (semantic, captured) = evidence(&native, root.path());
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
    drop(leader);
    let session = store.writer_session_for_tests().unwrap();
    let modes = std::sync::Mutex::new(Vec::new());
    let returned = IndexJobCoordinator::prepare_with_session(&store, Some(pin), session)
        .unwrap()
        .run_serving(&options, &cancel, |p| {
            if p.phase.starts_with("mode:") {
                modes.lock().unwrap().push(p.phase)
            }
        })
        .unwrap();
    assert_eq!(returned, pin);
    assert!(
        modes.lock().unwrap().iter().any(|m| m == "mode:unchanged"),
        "{modes:?}"
    );
    assert_eq!(
        store.semantic_at(pin).unwrap().references,
        semantic.references
    );
    fs::write(
        root.path().join("hello.rs"),
        "fn hello() { hello(); hello(); }\n",
    )
    .unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let advanced = IndexJobCoordinator::prepare_with_session(&store, Some(pin), session)
        .unwrap()
        .run_serving(&options, &cancel, |_| {})
        .unwrap();
    assert_ne!(advanced, pin);
    let current = store.semantic_at(advanced).unwrap();
    assert_eq!(current.coverage[0].state, s::CoverageState::Omitted);
    assert!(current.references.is_empty());
    assert!(current.symbols.is_empty());
    assert_eq!(
        store.semantic_at(pin).unwrap().references,
        semantic.references
    );
}

#[test]
fn corrupt_selected_child_and_impossible_enum_fail_without_partial_result() {
    use rusqlite::Connection;
    for variant in 0..2 {
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
        let identity =
            trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
                .unwrap();
        let db = Connection::open(
            state
                .path()
                .join("cache/indexes")
                .join(identity.root_key)
                .join("index.db"),
        )
        .unwrap();
        match variant {
            0 => {
                db.execute("UPDATE semantic_coverage_roles SET ordinal=9 WHERE ordinal=0 AND kind='observed'",[]).unwrap();
            }
            1 => {
                db.pragma_update(None, "ignore_check_constraints", true)
                    .unwrap();
                db.execute("UPDATE semantic_coverage SET state='impossible-state'", [])
                    .unwrap();
            }
            _ => unreachable!(),
        }
        drop(db);
        let error = store.semantic_at(pin).unwrap_err();
        assert!(
            error.to_string().contains("incompatible_index:"),
            "{error:#}"
        );
        let detail = if variant == 0 {
            "corrupt semantic child ordinal"
        } else {
            "impossible-state"
        };
        assert!(error.to_string().contains(detail), "{error:#}");
        assert!(store.recovery_pending_for_tests());
        assert_eq!(
            store.status().unwrap_err().to_string(),
            "incompatible_index: reconciliation required after invalid current index"
        );
    }
}

#[test]
fn native_only_selective_recovery_keeps_requested_heads_without_old_facts() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let leader = store.publication_for_tests().unwrap();
    let prior = store
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
    let (changed_graph, changed_native, changed_capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    assert_ne!(changed_native.revision.id, native.revision.id);
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
        store
            .publish_native_recovery_selective_for_tests(
                &changed_graph,
                &changed_capture,
                &changed_native,
                &leader,
                &cancel,
            )
            .is_err()
    );
    assert_eq!(store.index_baseline().unwrap(), prior);
    assert_eq!(
        store.semantic_at(prior).unwrap().references,
        semantic.references
    );
    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    let next = store
        .publish_native_recovery_selective_for_tests(
            &changed_graph,
            &changed_capture,
            &changed_native,
            &leader,
            &cancel,
        )
        .unwrap();
    let selected = store.semantic_at(next).unwrap();
    assert_eq!(selected.coverage.len(), 1);
    assert!(selected.coverage[0].requested);
    assert!(!selected.coverage[0].selected);
    assert_eq!(selected.coverage[0].state, s::CoverageState::Omitted);
    assert_eq!(selected.coverage[0].revision_id, changed_native.revision.id);
    assert!(selected.provenance.is_empty());
    assert!(selected.references.is_empty());
    assert_eq!(
        store.semantic_at(prior).unwrap().references,
        semantic.references
    );
}

#[test]
fn native_only_selective_recovery_replaces_semantics_after_private_stage() {
    use trellis::index_coordinator::IndexJobCoordinator;
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let (semantic, captured) = evidence(&native, root.path());
    let leader = store.publication_for_tests().unwrap();
    let prior = store
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
    drop(leader);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let path = state
        .path()
        .join("cache/indexes")
        .join(identity.root_key)
        .join("index.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE index_metadata SET stats='not-json'", [])
        .unwrap();
    drop(db);
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("incompatible_index")
    );
    let session = store.writer_session_for_tests().unwrap();
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let error = IndexJobCoordinator::prepare_with_session(&store, Some(prior), session.clone())
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    let db = rusqlite::Connection::open(&path).unwrap();
    let old:i64=db.query_row("SELECT count(*) FROM semantic_provenance WHERE tuple_id IN (SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1)",
        [format!("pin:v1:{}:{}",prior.index_generation,prior.index_revision)],|r|r.get(0)).unwrap();
    assert_eq!(
        old,
        semantic.provenance.len() as i64,
        "cancelled recovery preserves old private selection"
    );
    drop(db);
    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    let recovered = IndexJobCoordinator::prepare_with_session(&store, Some(prior), session)
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    assert_ne!(recovered.index_generation, prior.index_generation);
    assert!(store.semantic_at(recovered).unwrap().coverage.is_empty());
    assert!(store.semantic_at(recovered).unwrap().provenance.is_empty());
    assert!(
        store
            .semantic_at(prior)
            .unwrap_err()
            .to_string()
            .contains("foreign or missing native pin")
    );
}

#[test]
fn metadata_only_native_change_omits_semantics_with_reused_document_version() {
    use trellis::index_coordinator::IndexJobCoordinator;
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.rs"), "fn hello() { hello(); }\n").unwrap();
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
    drop(leader);
    let mut changed = options.clone();
    changed.max_file_bytes += 1;
    let (_, m_native, _) =
        index_workspace_bundle(&changed, store.root_id(), &cancel, |_| {}).unwrap();
    assert_ne!(native.revision.id, m_native.revision.id);
    assert_eq!(
        native.revision.documents[0].content_hash,
        m_native.revision.documents[0].content_hash
    );
    let session = store.writer_session_for_tests().unwrap();
    let next = IndexJobCoordinator::prepare_with_session(&store, Some(first), session)
        .unwrap()
        .run_serving(&changed, &cancel, |_| {})
        .unwrap();
    assert_ne!(next, first);
    let current = store.semantic_at(next).unwrap();
    assert_eq!(current.coverage[0].state, s::CoverageState::Omitted);
    assert_eq!(current.coverage[0].revision_id, m_native.revision.id);
    assert!(current.provenance.is_empty());
    assert!(current.references.is_empty());
    assert_eq!(
        store.semantic_at(first).unwrap().references,
        semantic.references
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
    let vid = |pin: trellis::model::IndexPin| -> i64 {
        db.query_row(
            "SELECT vid FROM revision_documents WHERE revision_id=?1",
            [format!(
                "pin:v1:{}:{}",
                pin.index_generation, pin.index_revision
            )],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(vid(first), vid(next));
}

#[test]
fn producer_descriptor_language_shrink_is_atomic() {
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
    semantic.producers[0].languages = vec![s::Language::Python, s::Language::Rust];
    semantic.producers[0].languages.sort();
    captured.requested_producers = semantic.producers.clone();
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
    let mut changed = semantic.clone();
    changed.producers[0].languages = vec![s::Language::Rust];
    captured.requested_producers = changed.producers.clone();
    s::validate_evidence(&changed, &native, &captured).unwrap();
    let error = store
        .publish_native_semantic(
            &graph, &capture, &native, &changed, &captured, &leader, first, &cancel,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("semantic producer languages changed"),
        "{error:#}"
    );
    assert_eq!(store.index_baseline().unwrap(), first);
    assert_eq!(
        store.semantic_at(first).unwrap().producers,
        semantic.producers
    );
}

#[test]
fn corrupt_symbol_document_fails_typed_without_partial_result() {
    assert_corrupt_symbol_documents("semantic_symbols", "key_");
}

#[test]
fn corrupt_binding_symbol_document_fails_typed_without_partial_result() {
    assert_corrupt_symbol_documents("semantic_declaration_binding_symbols", "");
}

fn assert_corrupt_symbol_documents(table: &str, prefix: &str) {
    for corruption in [
        "missing_language",
        "missing_path",
        "missing_document",
        "global_document",
    ] {
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
        let (mut semantic, captured) = evidence(&native, root.path());
        semantic.symbols[0].key.scope = s::SymbolScope::Document;
        semantic.symbols[0].key.document = Some(native.revision.documents[0].key.clone());
        semantic.declaration_bindings[0].symbols[0] = semantic.symbols[0].key.clone();
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
        assert_eq!(snapshot.symbols, semantic.symbols);
        assert_eq!(snapshot.declaration_bindings, semantic.declaration_bindings);
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
        let assignment = match corruption {
            "missing_language" => format!("{prefix}document_language=NULL"),
            "missing_path" => format!("{prefix}document_path=NULL"),
            "missing_document" => {
                format!("{prefix}document_language=NULL,{prefix}document_path=NULL")
            }
            "global_document" => format!("{prefix}scope='global'"),
            _ => unreachable!(),
        };
        assert_eq!(
            db.execute(&format!("UPDATE {table} SET {assignment}"), [])
                .unwrap(),
            1
        );
        drop(db);
        let error = store.semantic_at(pin).unwrap_err();
        assert!(
            error.to_string().contains("incompatible_index"),
            "{table} {corruption}: {error:#}"
        );
        assert!(
            error
                .to_string()
                .contains("corrupt semantic symbol scope/document"),
            "{table} {corruption}: {error:#}"
        );
        assert!(store.recovery_pending_for_tests(), "{table} {corruption}");
    }
}

/// A published native revision of one JavaScript file, and the store that holds it.
struct BatchWorld {
    _state: tempfile::TempDir,
    root: tempfile::TempDir,
    store: Store,
    index_db: std::path::PathBuf,
    cancel: Arc<AtomicBool>,
    batches: std::cell::Cell<usize>,
}

fn batch_world(text: &str) -> BatchWorld {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let physical = fs::canonicalize(root.path()).unwrap();
    fs::create_dir(physical.join("src")).unwrap();
    fs::write(physical.join("src/A.js"), text).unwrap();
    let store = Store::open_for_tests(state.path(), &physical).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(physical.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(leader);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical).unwrap();
    let roots = trellis::store::topology::TopologyRoots::isolated_for_tests(
        state.path().join("cache"),
        state.path().join("data"),
    );
    BatchWorld {
        index_db: roots.index_db(&identity),
        _state: state,
        root,
        store,
        cancel,
        batches: std::cell::Cell::new(0),
    }
}

impl BatchWorld {
    fn root(&self) -> std::path::PathBuf {
        fs::canonicalize(self.root.path()).unwrap()
    }

    fn fixture(&self) -> BatchFixture {
        BatchFixture::from_published(&self.index_db, &self.root())
    }

    fn write(&self, fixture: &BatchFixture) -> (std::path::PathBuf, String) {
        let number = self.batches.get();
        self.batches.set(number + 1);
        fixture.write(
            &self
                .root()
                .join(format!(".trellis-semantic-batch-{number}")),
        )
    }

    /// Stages and joins the batch against the head.
    fn join(
        &self,
        descriptor: &(std::path::PathBuf, String),
    ) -> (
        trellis::semantic_batch::AdmittedBatch,
        trellis::semantic_batch::JoinedBatch,
    ) {
        let (admitted, joined, _) = trellis::semantic_batch::testing::stage_and_join_admitted(
            &self.store,
            &self.root(),
            &descriptor.0,
            &descriptor.1,
            &trellis::semantic_batch::BatchLimits::PRODUCTION,
            DestinationMutations::default(),
            &self.cancel,
        )
        .unwrap();
        (admitted, joined)
    }

    fn publish(
        &self,
        admitted: &mut trellis::semantic_batch::AdmittedBatch,
        joined: &trellis::semantic_batch::JoinedBatch,
        expected: trellis::model::IndexPin,
    ) -> anyhow::Result<trellis::model::IndexPin> {
        let leader = self.store.publication_for_tests()?;
        self.store.publish_semantic_batch(
            joined,
            admitted.stage_mut(),
            expected,
            &leader,
            &self.cancel,
        )
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.index_db).unwrap()
    }
}

const CALL_SOURCE: &str = "function f(){g();}\n";

#[test]
fn contradictory_plain_reference_proofs_survive_store_selected_roundtrip() {
    let world = batch_world(CALL_SOURCE);
    // Two symbols claim the same call site.
    let position = CALL_SOURCE.find('g').unwrap() as i32;
    let doc = DocSpec::new("src/A.js")
        .occurrence(
            &[0, position, position + 1],
            &[0, position, position + 3],
            "external pkg A#",
            0,
        )
        .occurrence(
            &[0, position, position + 1],
            &[0, position, position + 3],
            "external pkg B#",
            0,
        );
    let mut fixture = world.fixture().reference("scip-typescript");
    fixture.artifact("javascript", vec![doc]);
    let descriptor = world.write(&fixture);
    let (mut admitted, joined) = world.join(&descriptor);
    let head = joined.destination_pin();
    let next = world.publish(&mut admitted, &joined, head).unwrap();
    let stored = world.store.semantic_at(next).unwrap();
    let reference = stored
        .references
        .iter()
        .find(|r| r.spelling == "g")
        .unwrap();
    assert_eq!(reference.resolution, s::Resolution::Ambiguous);
    assert_eq!(reference.candidates.len(), 2);
    assert_eq!(
        world
            .store
            .semantic_batch_report_at(next, &descriptor.1)
            .unwrap(),
        Some(joined.report().clone())
    );
    let selected_proofs: Vec<_> = stored
        .provenance
        .iter()
        .filter(|p| p.evidence_kind == s::EvidenceKind::SemanticReference)
        .collect();
    assert_eq!(selected_proofs.len(), 2);
    let inputs = world.store.selected_raw_inputs_at(next).unwrap();
    assert_eq!(inputs.len(), 1);
    for proof in selected_proofs {
        let link = proof.derived_from.as_ref().unwrap();
        let persisted = world
            .store
            .semantic_raw_proof_at(
                &inputs[0].producer_id,
                &proof.basis.as_ref().unwrap().artifact_hash,
                &inputs[0].source_set_id,
                &inputs[0].captured_revision,
                &link.provenance_id,
            )
            .unwrap()
            .unwrap();
        assert_eq!(persisted.fact_digest, link.fact_digest);
        assert_eq!(persisted.record_kind, "occurrence");
        assert_eq!(proof.document, reference.document);
    }
}

#[test]
fn raw_proof_import_cas_and_selected_read_are_atomic() {
    use trellis::store::BatchPublishFault;
    let world = batch_world(CALL_SOURCE);
    let position = CALL_SOURCE.find('g').unwrap() as i32;
    let doc = || {
        DocSpec::new("src/A.js")
            .occurrence(&[0, 9, 10], &[], "pkg f.", 1)
            .occurrence(
                &[0, position, position + 1],
                &[0, position, position + 3],
                "external pkg g.",
                0,
            )
            .symbol("pkg f.", 17, &[])
    };
    // A second producer's overlay is already published and must survive the import.
    let base = world.store.index_baseline().unwrap();
    let mut other = world.fixture();
    other.artifact("javascript", vec![doc()]);
    let other_descriptor = world.write(&other);
    let (mut other_admitted, other_joined) = world.join(&other_descriptor);
    let first = world
        .publish(
            &mut other_admitted,
            &other_joined,
            other_joined.destination_pin(),
        )
        .unwrap();
    let other_bindings = world.store.semantic_at(first).unwrap().declaration_bindings;
    assert!(!other_bindings.is_empty());

    let mut fixture = world.fixture().reference("scip-typescript");
    fixture.artifact("javascript", vec![doc()]);
    let descriptor = world.write(&fixture);
    let (mut admitted, joined) = world.join(&descriptor);
    let head = joined.destination_pin();
    assert_eq!(head, first);
    let leader = world.store.publication_for_tests().unwrap();
    // A pin that is not the join's destination is a revision conflict.
    let stale = world
        .store
        .publish_semantic_batch(&joined, admitted.stage_mut(), base, &leader, &world.cancel)
        .unwrap_err();
    assert!(
        format!("{stale:#}").starts_with("revision conflict"),
        "{stale:#}"
    );
    assert_eq!(world.store.status().unwrap().revision, first);
    let inputs_before = world.store.selected_raw_inputs_at(first).unwrap();
    for fault in [
        BatchPublishFault::SqlFault,
        BatchPublishFault::PrecommitCancel,
    ] {
        assert!(
            world
                .store
                .publish_semantic_batch_with_fault_for_tests(
                    &joined,
                    admitted.stage_mut(),
                    head,
                    &leader,
                    &world.cancel,
                    fault,
                )
                .is_err()
        );
        world
            .cancel
            .store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(world.store.status().unwrap().revision, first);
    }
    assert_eq!(
        world.store.selected_raw_inputs_at(first).unwrap().len(),
        inputs_before.len()
    );
    assert_eq!(
        world.store.semantic_at(first).unwrap().declaration_bindings,
        other_bindings
    );
    drop(leader);
    let newer = world.publish(&mut admitted, &joined, head).unwrap();
    let stored = world.store.semantic_at(newer).unwrap();
    assert!(stored.declaration_bindings.len() > other_bindings.len());
    assert!(
        stored
            .declaration_bindings
            .iter()
            .any(|binding| other_bindings.contains(binding))
    );
    assert_eq!(
        world
            .store
            .semantic_batch_report_at(newer, &descriptor.1)
            .unwrap(),
        Some(joined.report().clone())
    );
    assert_eq!(
        world
            .store
            .semantic_batch_report_at(newer, &other_descriptor.1)
            .unwrap(),
        Some(other_joined.report().clone())
    );
    let inputs = world.store.selected_raw_inputs_at(newer).unwrap();
    assert_eq!(inputs.len(), 2);
    let selected = inputs
        .iter()
        .find(|input| input.producer_id == "scip-typescript")
        .unwrap();
    assert_eq!(selected.captured_pin, first);
    assert_eq!(selected.captures.len(), 1);
    assert_eq!(
        selected.captures[0].binary_bytes,
        fs::read(world.root().join(selected_artifact_path(&descriptor))).unwrap()
    );
    assert_eq!(
        selected.descriptor.source_manifest_hash,
        selected.source_manifest_hash
    );
    assert_eq!(selected.requested_documents.len(), 1);
    // The same batch published again replaces the overlay and its report in place.
    let (mut again, rejoined) = world.join(&descriptor);
    let replaced = world.publish(&mut again, &rejoined, newer).unwrap();
    assert_eq!(
        world
            .store
            .semantic_at(replaced)
            .unwrap()
            .declaration_bindings
            .len(),
        stored.declaration_bindings.len()
    );
    assert_eq!(
        world
            .db()
            .query_row("SELECT COUNT(*) FROM semantic_raw_batches", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    // A corrupted stored proof is a typed failure and schedules recovery, never a partial read.
    let proof_id: String = world
        .db()
        .query_row(
            "SELECT p.proof_id FROM semantic_raw_proofs p JOIN semantic_raw_captures c ON c.capture_id=p.capture_id WHERE c.producer_id='scip-typescript' AND p.record_kind='occurrence' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    world
        .db()
        .execute(
            "UPDATE semantic_raw_proofs SET record_kind='relationship' WHERE proof_id=?1",
            [&proof_id],
        )
        .unwrap();
    let direct = world
        .store
        .semantic_raw_proof_at(
            &selected.producer_id,
            &selected.captures[0].artifact_hash,
            &selected.source_set_id,
            &selected.captured_revision,
            &proof_id,
        )
        .unwrap_err();
    assert!(
        direct
            .to_string()
            .contains("incompatible_index: corrupt raw proof inventory"),
        "{direct:#}"
    );
    assert!(world.store.recovery_pending_for_tests());
}

fn selected_artifact_path(descriptor: &(std::path::PathBuf, String)) -> String {
    let parsed: serde_json::Value =
        serde_json::from_slice(&fs::read(&descriptor.0).unwrap()).unwrap();
    parsed["artifacts"][0]["relativePath"]
        .as_str()
        .unwrap()
        .to_owned()
}
