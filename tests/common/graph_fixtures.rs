#![allow(dead_code)]
//! Small published workspaces for call-graph store tests. Sources are indexed for real; the
//! semantic evidence is a synthetic [`s::SemanticPublication`] that passes the store validator.
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    model::{Graph, IndexPin},
    native_evidence as n, native_ids, semantic_evidence as s,
    store::Store,
};

pub const PRODUCER: &str = "semantic.synthetic";

/// A call binding for the `nth` call (source order) of the declaration named `caller`.
#[derive(Clone)]
pub struct Bind {
    pub caller: String,
    pub nth: usize,
    pub resolution: s::Resolution,
    /// Name of the internal target declaration, when the resolution has one.
    pub target: Option<String>,
    pub dispatch: s::Dispatch,
    pub possible: Vec<String>,
    pub provenance: String,
}

pub fn bind(caller: &str, nth: usize, target: &str) -> Bind {
    Bind {
        caller: caller.into(),
        nth,
        resolution: s::Resolution::Resolved,
        target: Some(target.into()),
        dispatch: s::Dispatch::Direct,
        possible: vec![],
        provenance: format!("call-{caller}-{nth}"),
    }
}

#[derive(Clone)]
pub struct Spec {
    pub files: Vec<(String, String)>,
    /// No producer at all: a syntax-only publication.
    pub producer: bool,
    pub languages: Vec<s::Language>,
    /// Version of the requested producer; anything but "v1" makes bindings possiblyStale.
    pub requested_version: String,
    pub states: BTreeMap<String, s::CoverageState>,
    pub bindings: Vec<Bind>,
    /// Declarations that get a declaration binding (history provenance).
    pub declaration_bindings: Vec<String>,
    /// Provenance IDs for declaration bindings, by declaration name; the default is `decl-<name>`.
    pub declaration_proofs: BTreeMap<String, String>,
}

impl Spec {
    pub fn new(files: &[(&str, &str)]) -> Self {
        Self {
            files: files
                .iter()
                .map(|(path, text)| (path.to_string(), text.to_string()))
                .collect(),
            producer: true,
            languages: vec![s::Language::Rust],
            requested_version: "v1".into(),
            states: BTreeMap::new(),
            bindings: vec![],
            declaration_bindings: vec![],
            declaration_proofs: BTreeMap::new(),
        }
    }
    pub fn syntax_only(files: &[(&str, &str)]) -> Self {
        Self {
            producer: false,
            ..Self::new(files)
        }
    }
}

fn digest(bytes: &[u8]) -> String {
    native_ids::digest(b"", bytes)
}

fn native_components(
    root: &Path,
    executable_hash: &str,
    documents: &[&str],
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
    config.push(json!({"nativeAdmission":{"maxFileBytes":2 * 1024 * 1024}}));
    // The capture probes ignore files in every directory it walks, in path order.
    let mut probes = BTreeSet::new();
    for document in documents {
        let mut dir = Path::new(document).parent();
        while let Some(parent) = dir {
            for name in [".gitignore", ".ignore"] {
                probes.insert(parent.join(name));
            }
            dir = parent.parent();
        }
    }
    for probe in probes {
        config.push(entry(&probe.to_string_lossy()));
    }
    let toolchain = json!({"nativeExecutable":executable_hash,"rootRustToolchain":entry(INPUTS[22]),"rootRustToolchainToml":entry(INPUTS[23])});
    (toolchain, config, dependencies)
}

pub fn language_of(path: &str) -> s::Language {
    if path.ends_with(".js") {
        s::Language::Javascript
    } else {
        s::Language::Rust
    }
}

fn language_name(language: s::Language) -> &'static str {
    match language {
        s::Language::Javascript => "javascript",
        s::Language::Rust => "rust",
        s::Language::Python => "python",
        s::Language::Java => "java",
    }
}

fn declaration<'a>(native: &'a n::Artifact, name: &str) -> &'a n::Declaration {
    native
        .declarations
        .iter()
        .find(|d| d.name.as_deref() == Some(name) && d.name_range.is_some())
        .unwrap_or_else(|| panic!("no declaration named {name}"))
}

/// The syntax ID of the declaration named `name` in a published artifact.
pub fn syntax_id(native: &n::Artifact, name: &str) -> String {
    declaration(native, name).syntax_id.clone()
}

/// Calls owned by the declaration named `owner`, in source order.
pub fn calls_of<'a>(native: &'a n::Artifact, owner: &str) -> Vec<&'a n::Call> {
    let owner = syntax_id(native, owner);
    let mut calls: Vec<_> = native
        .calls
        .iter()
        .filter(|call| call.owner_syntax_id == owner)
        .collect();
    calls.sort_by_key(|call| (call.range.start, call.range.end));
    calls
}

fn target_of(native: &n::Artifact, name: &str) -> s::Target {
    let declaration = declaration(native, name);
    s::Target::Internal {
        syntax_id: declaration.syntax_id.clone(),
        document: declaration.document.clone(),
        revision_id: native.revision.id.clone(),
    }
}

fn canonical_target(target: &s::Target) -> Vec<u8> {
    native_ids::canonical(&serde_json::to_value(target).unwrap())
}

fn external() -> s::Target {
    s::Target::External {
        symbol: s::SymbolKey {
            scheme: s::SymbolScheme::Scip,
            symbol: "pkg outside".into(),
            scope: s::SymbolScope::Global,
            document: None,
        },
    }
}

fn evidence(
    spec: &Spec,
    native: &n::Artifact,
    root: &Path,
) -> (s::SemanticPublication, s::CapturedComponents) {
    let (toolchain, config, dependency) = native_components(
        root,
        &native.producer.executable_hash,
        &native
            .revision
            .documents
            .iter()
            .map(|d| d.key.path.as_str())
            .collect::<Vec<_>>(),
    );
    let producer = s::Producer {
        id: PRODUCER.into(),
        version: "v1".into(),
        executable_hash: digest(b"exe"),
        kind: s::SemanticKind::Semantic,
        languages: spec.languages.clone(),
        position_encoding: s::PositionEncoding::Utf8,
    };
    let manifest_hash = digest(&native_ids::canonical(&json!(
        native
            .revision
            .documents
            .iter()
            .map(|d| json!({"document":d.key,"contentHash":d.content_hash}))
            .collect::<Vec<_>>()
    )));
    let basis = |language: s::Language| s::SemanticBasis {
        producer_id: producer.id.clone(),
        producer_version: producer.version.clone(),
        producer_hash: producer.executable_hash.clone(),
        artifact_hash: digest(b"artifact"),
        language,
        source_set_id: native.revision.source_set_id.clone(),
        revision_id: native.revision.id.clone(),
        source_manifest_hash: manifest_hash.clone(),
        toolchain_hash: native.revision.toolchain_hash.clone(),
        config_hash: native.revision.config_hash.clone(),
        dependency_hash: native.revision.dependency_hash.clone(),
        lookup_dependencies: vec!["lookup".into()],
    };
    let document_of = |name: &str| declaration(native, name).document.clone();
    let hash_of = |key: &n::DocumentKey| {
        native
            .revision
            .documents
            .iter()
            .find(|d| &d.key == key)
            .unwrap()
            .content_hash
            .clone()
    };
    let proof = |id: &str, key: &n::DocumentKey, kind| s::Provenance {
        id: id.into(),
        producer_id: producer.id.clone(),
        document: key.clone(),
        revision_id: native.revision.id.clone(),
        content_hash: hash_of(key),
        evidence_kind: kind,
        basis: Some(basis(language_of(&key.path))),
        freshness: if spec.requested_version == "v1" {
            s::Freshness::Fresh
        } else {
            s::Freshness::PossiblyStale
        },
        derived_from: None,
    };
    let mut semantic = s::SemanticPublication {
        producers: vec![producer.clone()],
        coverage: vec![],
        provenance: vec![],
        symbols: vec![],
        declaration_bindings: vec![],
        references: vec![],
        call_bindings: vec![],
        type_relationships: vec![],
    };
    let fact_bearing = |key: &n::DocumentKey| {
        matches!(
            spec.states
                .get(&key.path)
                .copied()
                .unwrap_or(s::CoverageState::Complete),
            s::CoverageState::Complete | s::CoverageState::Partial
        )
    };
    for name in &spec.declaration_bindings {
        let declaration = declaration(native, name);
        let key = &declaration.document;
        if !fact_bearing(key) {
            continue;
        }
        let id = spec
            .declaration_proofs
            .get(name)
            .cloned()
            .unwrap_or_else(|| format!("decl-{name}"));
        let symbol = s::SymbolKey {
            scheme: s::SymbolScheme::Scip,
            symbol: format!("pkg {name}"),
            scope: s::SymbolScope::Global,
            document: None,
        };
        semantic
            .provenance
            .push(proof(&id, key, s::EvidenceKind::DeclarationBinding));
        semantic.symbols.push(s::Symbol {
            key: symbol.clone(),
            display_name: Some(name.clone()),
            declarations: vec![target_of(native, name)],
            provenance_id: id.clone(),
        });
        semantic.declaration_bindings.push(s::DeclarationBinding {
            syntax_id: Some(declaration.syntax_id.clone()),
            symbols: vec![symbol],
            join: s::Join {
                anchor: s::MeasuredAnchor {
                    document: key.clone(),
                    revision_id: native.revision.id.clone(),
                    content_hash: hash_of(key),
                    range: declaration.name_range.clone().unwrap(),
                    kind: s::JoinKind::DeclarationName,
                },
                status: s::JoinStatus::Exact,
                candidate_ids: vec![declaration.syntax_id.clone()],
                diagnostic: None,
            },
            provenance_id: id,
        });
    }
    for b in &spec.bindings {
        let key = document_of(&b.caller);
        if !fact_bearing(&key) {
            continue;
        }
        let call = calls_of(native, &b.caller)[b.nth];
        let shared = proof(&b.provenance, &key, s::EvidenceKind::SemanticReference);
        if !semantic.provenance.contains(&shared) {
            semantic.provenance.push(shared);
        }
        let target = || target_of(native, b.target.as_deref().unwrap());
        let (declared_target, mut candidates) = match b.resolution {
            s::Resolution::Resolved => (Some(target()), vec![]),
            s::Resolution::External => (Some(external()), vec![]),
            s::Resolution::Ambiguous => (None, vec![target(), external()]),
            s::Resolution::Unresolved => (None, vec![]),
        };
        candidates.sort_by_key(canonical_target);
        let mut possible_dispatch: Vec<_> = b
            .possible
            .iter()
            .map(|name| target_of(native, name))
            .collect();
        possible_dispatch.sort_by_key(canonical_target);
        let stale_target =
            matches!(declared_target, Some(s::Target::Internal { .. })).then_some(false);
        semantic.call_bindings.push(s::CallBinding {
            call_id: Some(call.id.clone()),
            join: s::Join {
                anchor: s::MeasuredAnchor {
                    document: key.clone(),
                    revision_id: native.revision.id.clone(),
                    content_hash: hash_of(&key),
                    range: call.callee_range.clone().unwrap(),
                    kind: s::JoinKind::Callee,
                },
                status: s::JoinStatus::Exact,
                candidate_ids: vec![call.id.clone()],
                diagnostic: None,
            },
            resolution: b.resolution,
            declared_target,
            candidates,
            dispatch: b.dispatch,
            possible_dispatch,
            possible_dispatch_complete: false,
            stale_target,
            provenance_id: b.provenance.clone(),
        });
    }
    semantic.provenance.sort_by(|a, b| a.id.cmp(&b.id));
    for document in &native.revision.documents {
        let language = language_of(&document.key.path);
        if !spec.languages.contains(&language) {
            continue;
        }
        let state = spec
            .states
            .get(&document.key.path)
            .copied()
            .unwrap_or(s::CoverageState::Complete);
        let call = semantic
            .call_bindings
            .iter()
            .any(|b| b.join.anchor.document == document.key);
        let definition = semantic
            .declaration_bindings
            .iter()
            .any(|b| b.join.anchor.document == document.key);
        let observed_roles: Vec<_> = if matches!(
            state,
            s::CoverageState::Complete | s::CoverageState::Partial
        ) {
            [(s::Role::Definition, definition), (s::Role::Call, call)]
                .into_iter()
                .filter_map(|(role, seen)| seen.then_some(role))
                .collect()
        } else {
            vec![]
        };
        semantic.coverage.push(s::Coverage {
            producer_id: PRODUCER.into(),
            language,
            source_set_id: document.key.source_set_id.clone(),
            document_path: document.key.path.clone(),
            revision_id: native.revision.id.clone(),
            requested: true,
            selected: !matches!(
                state,
                s::CoverageState::Omitted | s::CoverageState::Unsupported
            ),
            state,
            supported_roles: vec![s::Role::Definition, s::Role::Call],
            observed_roles,
            diagnostic: (!matches!(state, s::CoverageState::Complete))
                .then(|| format!("{state:?} fixture")),
        });
    }
    let mut requested = producer.clone();
    requested.version = spec.requested_version.clone();
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
        requested_producers: vec![requested],
        toolchain: native_ids::canonical(&toolchain),
        config: native_ids::canonical(&json!(config)),
        dependency: native_ids::canonical(&json!(dependency)),
        native_extraction_components: BTreeMap::new(),
    };
    (semantic, captured)
}

/// A store that publishes one revision per [`Publisher::publish`] call.
pub struct Publisher {
    pub store: Store,
    pub root: PathBuf,
    pub pin: IndexPin,
    pub native: Option<n::Artifact>,
    pub graph: Option<Graph>,
    cancel: Arc<AtomicBool>,
}

impl Publisher {
    pub fn open(state: &Path, root: &Path) -> Self {
        let store = Store::open_for_tests(state, root).unwrap();
        let pin = store.index_baseline().unwrap();
        Self {
            store,
            root: root.to_owned(),
            pin,
            native: None,
            graph: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Writes the spec's files, indexes the workspace and publishes it as the next revision.
    pub fn publish(&mut self, spec: &Spec) -> IndexPin {
        for (path, text) in &spec.files {
            let path = self.root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let options = IndexOptions::new(self.root.clone());
        let (graph, native, capture) =
            index_workspace_bundle(&options, self.store.root_id(), &self.cancel, |_| {}).unwrap();
        let leader = self.store.publication_for_tests().unwrap();
        self.pin = if spec.producer {
            let (semantic, captured) = evidence(spec, &native, &self.root);
            self.store
                .publish_native_semantic(
                    &graph,
                    &capture,
                    &native,
                    &semantic,
                    &captured,
                    &leader,
                    self.pin,
                    &self.cancel,
                )
                .unwrap()
        } else {
            self.store
                .publish_native(&graph, &capture, &native, &leader, self.pin, &self.cancel)
                .unwrap()
        };
        self.native = Some(native);
        self.graph = Some(graph);
        self.pin
    }

    pub fn native(&self) -> &n::Artifact {
        self.native.as_ref().unwrap()
    }
}

/// Publishes `spec` as one revision into `state`, for tests that reopen the store themselves.
pub fn publish_in(state: &Path, workspace: &Path, spec: &Spec) -> IndexPin {
    Publisher::open(state, workspace).publish(spec)
}

/// `root` with `n` calls: `fn root() { f0(); f1(); ... }` plus empty `fN` functions.
pub fn fanout_source(n: usize) -> String {
    let calls: String = (0..n).map(|i| format!("f{i}(); ")).collect();
    let defs: String = (0..n).map(|i| format!("fn f{i}() {{}}\n")).collect();
    format!("fn root() {{ {calls}}}\n{defs}")
}

pub fn language_set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| item.to_string()).collect()
}

pub fn language_name_of(language: s::Language) -> &'static str {
    language_name(language)
}
