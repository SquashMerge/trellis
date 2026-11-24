//! The canonical core comparison (#1301).
use super::*;
use crate::indexer::{IndexOptions, index_workspace_bundle};
use serde_json::json;
use std::{fs, sync::atomic::AtomicBool};

fn write(root: &std::path::Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn publish(store: &Store, dir: &std::path::Path) {
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(dir.into()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &store.writer().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
}

const SERVER: &str = "mod util;\nasync fn h() {}\nfn router() -> Router {\n    Router::new().route(\"/health\", get(h))\n}\nfn main() { util::u(); }\n";
const CLIENT: &str = "async function app(api) {\n  await api(\"/health\");\n}\n";
/// A declaration rule, and two apps: every part of a core has rows.
const RULES: &str = "version=1\n\
    [roles.handler]\nselect={paths=['crates/**'],name='*'}\n\
    [roles.target]\nselect={paths=['crates/**']}\n\
    [[rules]]\nid='calls'\nfor_each='handler'\nrequire=[{calls='target'}]\n";

fn checkout() -> (tempfile::TempDir, tempfile::TempDir, Store) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    write(dir, "Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    write(
        dir,
        "crates/alpha/Cargo.toml",
        "[package]\nname = \"alpha\"\n",
    );
    write(dir, "crates/alpha/src/main.rs", SERVER);
    write(dir, "crates/alpha/src/util.rs", "pub fn u() {}\n");
    write(dir, "web/package.json", "{\"name\":\"web-app\"}\n");
    write(dir, "web/app.js", CLIENT);
    let store = crate::testing::retry_admission(|| Store::open_for_tests(state.path(), dir));
    publish(&store, dir);
    (state, root, store)
}

fn core(store: &Store) -> ArchitectureCore {
    let config = crate::rules::parse(RULES).unwrap();
    let response = store.evidence_response().unwrap();
    let pin = response.selected_pin(None).unwrap();
    response
        .architecture_core_at(pin, Some(&config), true, &mut || Ok(()), &|_| {})
        .unwrap()
}

#[test]
fn architecture_incremental_canonical_index() {
    let (_state, _root, store) = checkout();
    let mut computed = core(&store);
    let before = computed.canonical().unwrap();
    let index = &computed.dependencies;
    let inputs = store
        .architecture_fingerprints_for_tests(Some(computed.pin))
        .unwrap();
    assert_eq!(
        index.order.as_ref(),
        inputs.documents.keys().cloned().collect::<Vec<_>>()
    );
    assert_eq!(
        index.documents.len(),
        computed.references.source_facts().len()
    );
    assert_eq!(
        hex::encode(index.rust_catalog),
        before["references"]["rustCatalogDigest"]
    );
    let mut scanned_zero = false;
    for ((key, dependency), facts) in index
        .order
        .iter()
        .zip(&index.documents)
        .zip(computed.references.source_facts())
    {
        assert_eq!(dependency.fingerprint.as_ref(), &inputs.documents[key]);
        assert_eq!(dependency.references.work, facts.work);
        assert_eq!(dependency.references.sites, facts.sites);
        assert_eq!(dependency.references.endpoints.as_ref(), facts.endpoints);
        assert!(facts.endpoints.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            facts
                .endpoints
                .iter()
                .all(|&endpoint| (endpoint as usize) < index.order.len())
        );
        match (&dependency.references.plan, &facts.plan) {
            (Some(plan), Some(recorded)) => {
                assert!(Arc::ptr_eq(plan, recorded));
                scanned_zero |= facts.sites == 0;
            }
            (None, None) => {
                assert_eq!((facts.work, facts.sites), (0, 0));
                assert!(facts.endpoints.is_empty());
            }
            _ => panic!("dependency plan differs from the full source fact"),
        }
    }
    assert!(scanned_zero, "zero-site scans keep a plan");
    let source = computed
        .references
        .source_facts()
        .iter()
        .position(|fact| fact.sites > 0 && !fact.endpoints.is_empty())
        .unwrap();
    macro_rules! changed {
        ($index:ident, $change:block) => {{
            let saved = computed.dependencies.clone();
            let $index = &mut computed.dependencies;
            $change
            let after = computed.canonical().unwrap();
            assert_ne!(before, after, "{}", stringify!($change));
            assert!(first_difference(&before, &after).unwrap().starts_with("/dependencies/"));
            computed.dependencies = saved;
        }};
    }
    changed!(index, {
        Arc::make_mut(&mut index.order)[0].path.push('2');
    });
    for member in 0..5 {
        changed!(index, {
            let digest = match member {
                0 => &mut index.rust_catalog,
                1 => &mut index.manifests,
                2 => &mut index.root_config,
                3 => &mut index.producers,
                _ => &mut index.selections,
            };
            digest[0] ^= 1;
        });
    }
    for member in 0..5 {
        changed!(index, {
            let fingerprint =
                Arc::make_mut(&mut Arc::make_mut(&mut index.documents[source]).fingerprint);
            match member {
                0 => fingerprint.native.version_id.push('2'),
                1 => fingerprint.native.coverage_requested ^= true,
                2 => fingerprint.native.coverage_selected ^= true,
                3 => fingerprint.native.coverage_state.push('2'),
                _ => fingerprint.native.coverage_diagnostic = Some("changed".into()),
            }
        });
    }
    changed!(index, {
        Arc::make_mut(&mut Arc::make_mut(&mut index.documents[source]).fingerprint).semantic[0] ^=
            1;
    });
    changed!(index, {
        Arc::make_mut(&mut Arc::make_mut(&mut index.documents[source]).fingerprint).sids[0] ^= 1;
    });
    changed!(index, {
        Arc::make_mut(&mut Arc::make_mut(&mut index.documents[source]).fingerprint).projected ^=
            true;
    });
    for member in 0..6 {
        changed!(index, {
            let references =
                Arc::make_mut(&mut Arc::make_mut(&mut index.documents[source]).references);
            match member {
                0 => references.work += 1,
                1 => references.sites += 1,
                2 => references.endpoints = Arc::from([]),
                3 => references.plan = None,
                4 => Arc::make_mut(references.plan.as_mut().unwrap()).prepared_work += 1,
                _ => Arc::make_mut(references.plan.as_mut().unwrap())
                    .sites
                    .clear(),
            }
        });
    }
    for member in 0..3 {
        changed!(index, {
            let counts = &mut Arc::make_mut(&mut index.documents[source]).semantic;
            match member {
                0 => counts.rows += 1,
                1 => counts.metadata += 1,
                _ => counts.lookups += 1,
            }
        });
        changed!(index, {
            match member {
                0 => index.global_semantic.rows += 1,
                1 => index.global_semantic.metadata += 1,
                _ => index.global_semantic.lookups += 1,
            }
        });
    }
    assert_eq!(before, computed.canonical().unwrap());
}

#[test]
fn architecture_incremental_absent_source_has_no_plan() {
    let (_state, _root, store) = checkout();
    let response = store.evidence_response().unwrap();
    let pin = response.selected_pin(None).unwrap();
    let mut input = response
        .architecture_input(pin, &mut || Ok(()), &|_| {})
        .unwrap();
    let missing = input.snapshot.documents[0].key.clone();
    input.snapshot.unparsed.insert(missing.clone());
    input
        .snapshot
        .evidence_gaps
        .insert(missing.clone(), "evidence_budget");
    response.finish(()).unwrap();
    let computed =
        ArchitectureCore::compute(&store, input, None, false, &mut || Ok(()), &|_| {}).unwrap();
    let position = computed
        .dependencies
        .order
        .iter()
        .position(|key| key == &missing)
        .unwrap();
    let references = &computed.dependencies.documents[position].references;
    assert!(references.plan.is_none());
    assert_eq!((references.work, references.sites), (0, 0));
    assert!(references.endpoints.is_empty());
}

#[test]
fn two_builds_of_one_revision_are_canonically_equal_and_an_edit_is_not() {
    let (_state, root, store) = checkout();
    let first = core(&store).canonical().unwrap();
    // Every part is compared, and this checkout gives each one rows.
    let references = &first["references"];
    assert!(
        references["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| !source["edges"].as_array().unwrap().is_empty()),
        "{references}"
    );
    let rules = &first["rules"]["base"];
    assert!(
        rules["documents"]
            .as_array()
            .is_some_and(|documents| documents
                .iter()
                .any(|rows| !rows[0].as_array().unwrap().is_empty())),
        "{rules}"
    );
    assert_eq!(first["metrics"].as_array().unwrap().len(), 3);
    let apps = first["appCandidates"].as_array().unwrap();
    assert_eq!(
        apps.iter()
            .filter(|candidate| candidate[1].as_str().unwrap().starts_with("App("))
            .count(),
        2,
        "{apps:?}"
    );
    assert_eq!(first["interfaces"]["routes"].as_array().unwrap().len(), 1);
    assert_eq!(first["interfaces"]["calls"].as_array().unwrap().len(), 1);

    let second = core(&store).canonical().unwrap();
    assert_eq!(first_difference(&first, &second), None);
    assert_eq!(first, second);

    // A line above the handler moves its route and its metrics.
    write(
        root.path(),
        "crates/alpha/src/main.rs",
        &format!("// moved\n{SERVER}"),
    );
    publish(&store, root.path());
    let edited = core(&store).canonical().unwrap();
    let at = first_difference(&first, &edited).expect("an edit changes the core");
    assert!(at.starts_with("/dependencies/documents/"), "{at}");
    assert_ne!(
        first["interfaces"]["routes"][0],
        edited["interfaces"]["routes"][0]
    );
}

/// `core` with `edit` applied to the first metric row of the first measured document.
fn with_first_metric_row(
    mut core: ArchitectureCore,
    edit: fn(&mut DeclarationMetrics),
) -> ArchitectureCore {
    let Some(Ok(mut metrics)) = core.metrics.take() else {
        panic!("the core measured its documents");
    };
    let (key, document) = metrics
        .iter()
        .find(|(_, document)| !document.rows.is_empty())
        .map(|(key, document)| (key.clone(), document.clone()))
        .expect("a measured declaration");
    let mut rows = document.rows.clone();
    edit(&mut rows[0]);
    metrics.insert(
        key,
        Arc::new(MetricsDocument {
            rows,
            gap: document.gap.clone(),
            physical_lines: document.physical_lines,
            physical_bytes: document.physical_bytes,
            file_role: document.file_role,
        }),
    );
    core.metrics = Some(Ok(metrics));
    core
}

/// A metric row's `name` and `kind` feed the top-cognitive answer though its JSON skips
/// them: a change to either alone is a canonical difference.
#[test]
fn a_metric_row_name_or_kind_alone_is_a_canonical_difference() {
    let (_state, _root, store) = checkout();
    let base = core(&store).canonical().unwrap();
    type RowEdit = fn(&mut DeclarationMetrics);
    let edits: [(&str, RowEdit); 2] = [
        ("name", |row| row.name = Some("renamed".into())),
        ("kind", |row| row.kind = "renamedKind".into()),
    ];
    for (field, edit) in edits {
        let edited = with_first_metric_row(core(&store), edit)
            .canonical()
            .unwrap();
        let at = first_difference(&base, &edited).expect("the edit is a difference");
        assert!(
            at.starts_with("/metrics/") && at.ends_with(&format!("/{field}")),
            "{field}: {at}"
        );
    }
}

#[test]
fn a_kept_failure_is_part_of_the_canonical_core() {
    let (_state, _root, store) = checkout();
    let core = core(&store);
    let mut failed = core.canonical().unwrap();
    let failure = CoreFailure::of(&anyhow::anyhow!("bounded_work: rules selection budget"));
    let ArchitectureCore {
        pin,
        references,
        rules,
        metrics,
        app_candidates,
        interfaces,
        summaries,
        dependencies,
    } = core;
    let rules = rules.map(|part| RulesPart {
        content_hash: part.content_hash,
        base: Err(failure.clone()),
    });
    let kept = ArchitectureCore {
        pin,
        references,
        rules,
        metrics,
        app_candidates,
        interfaces,
        summaries,
        dependencies,
    }
    .canonical()
    .unwrap();
    assert_eq!(
        first_difference(&failed, &kept).as_deref(),
        Some("/rules/base/apps")
    );
    failed["rules"]["base"] = json!({"failure": {
        "rule": null,
        "integrity": false,
        "message": "bounded_work: rules selection budget",
    }});
    assert_eq!(failed, kept);
}

#[test]
fn architecture_incremental_base_requires_successful_metrics_and_complete_dependencies() {
    let (_state, _root, store) = checkout();
    let mut core = core(&store);
    assert!(core.incremental_base_eligible());
    let metrics = core.metrics.take().unwrap();
    assert!(
        !core.incremental_base_eligible(),
        "an unmeasured core cannot seed"
    );
    core.metrics = Some(Err(CoreFailure::of(&anyhow::anyhow!(
        "bounded_work: metrics"
    ))));
    assert!(
        !core.incremental_base_eligible(),
        "failed metrics cannot seed"
    );
    core.metrics = Some(metrics);
    core.dependencies.documents.pop();
    assert!(
        !core.incremental_base_eligible(),
        "a partial index cannot seed"
    );
}

#[test]
fn the_first_difference_is_named_by_its_path() {
    let base = json!({"a": [1, {"b": "x"}], "c": {"d": true}});
    assert_eq!(first_difference(&base, &base.clone()), None);
    let mut changed = base.clone();
    changed["a"][1]["b"] = json!("y");
    changed["c"]["d"] = json!(false);
    assert_eq!(first_difference(&base, &changed).as_deref(), Some("/a/1/b"));
    let longer = json!({"a": [1, {"b": "x"}, 3], "c": {"d": true}});
    assert_eq!(first_difference(&base, &longer).as_deref(), Some("/a/2"));
    let extra = json!({"a": [1, {"b": "x"}], "c": {"d": true, "e": 1}});
    assert_eq!(first_difference(&base, &extra).as_deref(), Some("/c/e"));
    assert_eq!(
        first_difference(&json!(1), &json!("1")).as_deref(),
        Some("")
    );
}

#[test]
fn full_reference_canonical_includes_aligned_source_facts_and_descriptor_slots() {
    let (_state, _root, store) = checkout();
    let canonical = core(&store).canonical().unwrap();
    let references = &canonical["references"];
    let sources = references["sources"].as_array().unwrap();
    let facts = references["sourceFacts"].as_array().unwrap();
    let descriptors = references["externalPackages"].as_array().unwrap();
    assert_eq!(
        sources.len(),
        references["identities"].as_array().unwrap().len()
    );
    assert_eq!(sources.len(), facts.len());
    assert_eq!(sources.len(), descriptors.len());
    assert_eq!(references["rustCatalogDigest"].as_str().unwrap().len(), 64);
    assert!(
        facts
            .iter()
            .any(|facts| facts["sites"].as_u64().unwrap() > 0)
    );
    for (index, source) in sources.iter().enumerate() {
        if source.is_null() {
            assert!(facts[index]["plan"].is_null());
            assert_eq!(facts[index]["work"], 0);
            assert_eq!(facts[index]["sites"], 0);
            assert_eq!(facts[index]["endpoints"], json!([]));
        } else {
            let plan = &facts[index]["plan"];
            assert!(plan["sites"].is_array());
            assert!(plan["prepared_work"].is_u64());
            assert!(
                facts[index]["work"].as_u64().unwrap() >= plan["prepared_work"].as_u64().unwrap()
            );
            assert_eq!(source["packages"], descriptors[index]);
            assert_eq!(
                source["membership"].as_array().unwrap().len(),
                descriptors[index].as_array().unwrap().len()
            );
        }
    }
}

fn native_input(store: &Store) -> ArchitectureInput {
    let response = store.evidence_response().unwrap();
    let pin = response.selected_pin(None).unwrap();
    let input = response
        .architecture_native_input(pin, &mut || Ok(()), &|_| {})
        .unwrap();
    response.finish(input).unwrap()
}

fn incremental_checkout() -> (tempfile::TempDir, tempfile::TempDir, Store) {
    let (state, root, store) = checkout();
    write(root.path(), "idle.rs", "pub fn idle() { let _ = 1; }\n");
    publish(&store, root.path());
    (state, root, store)
}

#[test]
fn architecture_incremental_global_state() {
    let (_state, root, store) = incremental_checkout();
    let config = crate::rules::parse(RULES).unwrap();
    let base = store
        .architecture_build_for_tests(None, Some(&config))
        .unwrap();
    let frozen = base.core.canonical().unwrap();
    write(
        root.path(),
        "crates/alpha/src/main.rs",
        &format!("{SERVER}// changed\n"),
    );
    publish(&store, root.path());
    let built = store
        .architecture_build_for_tests(Some(&base.core), Some(&config))
        .unwrap();
    let paths = |keys: &[DocumentKey]| keys.iter().map(|key| key.path.clone()).collect::<Vec<_>>();
    let ArchitectureBuildPath::Incremental {
        dirty,
        patched,
        reused,
    } = &built.path
    else {
        panic!("expected incremental: {:?}", built.path)
    };
    assert_eq!(paths(dirty), ["crates/alpha/src/main.rs"]);
    assert!(patched.is_empty());
    assert_eq!(reused.len(), 3);
    assert_eq!(built.semantic_sources, *dirty);
    assert_eq!(built.references.file_references, *dirty);
    let candidate = built.core.canonical().unwrap();
    let oracle = core(&store).canonical().unwrap();
    assert_eq!(first_difference(&candidate, &oracle), None);
    for field in [
        "modules",
        "identities",
        "partialFiles",
        "producers",
        "documentStatus",
    ] {
        assert_eq!(candidate["references"][field], oracle["references"][field]);
    }
    assert_ne!(
        candidate["references"]["modules"],
        frozen["references"]["modules"]
    );
    assert_eq!(base.core.canonical().unwrap(), frozen);
}

#[test]
fn architecture_incremental_metrics() {
    let (_state, root, store) = incremental_checkout();
    let base = store.architecture_build_for_tests(None, None).unwrap();
    let weak = base.core.downgrade();
    write(root.path(), "idle.rs", "pub fn idle() { let _ = 2; }\n");
    publish(&store, root.path());
    let built = store
        .architecture_build_for_tests(Some(&base.core), None)
        .unwrap();
    assert!(matches!(
        built.path,
        ArchitectureBuildPath::Incremental { .. }
    ));
    assert_eq!(
        built
            .measured_sources
            .iter()
            .map(|key| key.path.as_str())
            .collect::<Vec<_>>(),
        ["idle.rs"]
    );
    let sharing = built.core.sharing(&base.core);
    for (key, shared) in sharing {
        if key.path == "idle.rs" {
            assert!(
                !shared.source && !shared.metric && !shared.fingerprint && !shared.plan,
                "{shared:?}"
            );
        } else {
            assert!(
                shared.source
                    && shared.metric
                    && shared.dependency
                    && shared.fingerprint
                    && shared.references
                    && shared.endpoints
                    && shared.plan,
                "{}: {shared:?}",
                key.path
            );
        }
    }
    let measured: Vec<_> = built
        .progress
        .iter()
        .filter(|value| value.phase == "measuring")
        .map(|value| value.completed)
        .collect();
    assert_eq!(
        measured,
        [0, 3, 4],
        "one reused jump then ordered fresh commits"
    );
    let phases: Vec<_> = built.progress.iter().map(|value| value.phase).collect();
    assert!(
        phases
            .iter()
            .position(|phase| *phase == "resolving")
            .unwrap()
            < phases
                .iter()
                .position(|phase| *phase == "evaluating")
                .unwrap()
    );
    assert_eq!(
        built.core.canonical().unwrap(),
        store
            .architecture_build_for_tests(None, None)
            .unwrap()
            .core
            .canonical()
            .unwrap()
    );
    assert!(weak.is_alive());
    drop(base);
    assert!(
        !weak.is_alive(),
        "neither plans nor completed cores retain the base core"
    );
    assert!(built.core.canonical().is_ok());
}

#[test]
fn architecture_incremental_apps_interfaces() {
    let (_state, root, store) = incremental_checkout();
    let config = crate::rules::parse(RULES).unwrap();
    let base = store
        .architecture_build_for_tests(None, Some(&config))
        .unwrap();
    let before = base.core.canonical().unwrap();
    write(
        root.path(),
        "crates/alpha/src/main.rs",
        &SERVER.replace("/health", "/status"),
    );
    publish(&store, root.path());
    let built = store
        .architecture_build_for_tests(Some(&base.core), Some(&config))
        .unwrap();
    assert!(matches!(
        built.path,
        ArchitectureBuildPath::Incremental { .. }
    ));
    let after = built.core.canonical().unwrap();
    assert_eq!(after, core(&store).canonical().unwrap());
    assert_ne!(
        after["interfaces"], before["interfaces"],
        "routes are freshly extracted"
    );
    assert_eq!(after["appCandidates"], before["appCandidates"]);
    assert!(
        !after["rules"].is_null(),
        "full rule preparation still runs"
    );
}

#[test]
fn architecture_incremental_changed_sets_and_ordered_gates() {
    let (_state, _root, store) = incremental_checkout();
    let base = core(&store);
    let input = native_input(&store);
    let mut fingerprints = input.fingerprints.clone();
    let rust: Vec<_> = fingerprints
        .documents
        .keys()
        .filter(|key| key.language == "rust")
        .cloned()
        .collect();
    let script = fingerprints
        .documents
        .keys()
        .find(|key| key.language == "javascript")
        .unwrap()
        .clone();
    let reason = |baseline: Option<&dependencies::ArchitectureDependencies>| {
        dependencies::changed_documents(&input.snapshot, &fingerprints, baseline)
            .unwrap()
            .err()
    };
    assert_eq!(reason(None), Some(FullReason::NoBase));
    // Simultaneous later gate candidates cannot hide the first differing field.
    let mut old = base.dependencies.clone();
    Arc::make_mut(&mut old.order)[0].path.push('2');
    old.manifests[0] ^= 1;
    old.root_config[0] ^= 1;
    old.producers[0] ^= 1;
    old.selections[0] ^= 1;
    assert_eq!(reason(Some(&old)), Some(FullReason::DocumentSetChanged));
    old.order = base.dependencies.order.clone();
    assert_eq!(reason(Some(&old)), Some(FullReason::ManifestsChanged));
    old.manifests = base.dependencies.manifests;
    assert_eq!(reason(Some(&old)), Some(FullReason::RootTsconfigChanged));
    old.root_config = base.dependencies.root_config;
    assert_eq!(reason(Some(&old)), Some(FullReason::ProducerSetChanged));
    old.producers = base.dependencies.producers;
    assert_eq!(reason(Some(&old)), Some(FullReason::RawSelectionsChanged));
    old.selections = base.dependencies.selections;
    assert_eq!(reason(Some(&old)), None);

    for key in rust.iter().take(2) {
        fingerprints
            .documents
            .get_mut(key)
            .unwrap()
            .native
            .version_id
            .push('2');
    }
    let changed = dependencies::changed_documents(&input.snapshot, &fingerprints, Some(&old))
        .unwrap()
        .unwrap();
    assert_eq!(changed.dirty.len(), 2, "exactly half is eligible");
    fingerprints
        .documents
        .get_mut(&rust[2])
        .unwrap()
        .native
        .version_id
        .push('2');
    assert_eq!(
        dependencies::changed_documents(&input.snapshot, &fingerprints, Some(&old))
            .unwrap()
            .err(),
        Some(FullReason::DirtyMajority)
    );
    fingerprints
        .documents
        .get_mut(&script)
        .unwrap()
        .native
        .coverage_diagnostic = Some("changed".into());
    assert_eq!(
        dependencies::changed_documents(&input.snapshot, &fingerprints, Some(&old))
            .unwrap()
            .err(),
        Some(FullReason::NonRustChanged)
    );

    fingerprints = input.fingerprints.clone();
    fingerprints.documents.get_mut(&script).unwrap().semantic[0] ^= 1;
    let changed = dependencies::changed_documents(&input.snapshot, &fingerprints, Some(&old))
        .unwrap()
        .unwrap();
    assert_eq!(
        changed.dirty,
        std::collections::BTreeSet::from([script.clone()]),
        "non-Rust semantic-only changes stay incremental"
    );
    let util = rust
        .iter()
        .find(|key| key.path.ends_with("util.rs"))
        .unwrap();
    let main = rust
        .iter()
        .find(|key| key.path.ends_with("main.rs"))
        .unwrap();
    // #1310: a projection's current pin is rebased per core, so being projected is not
    // Dirty.
    fingerprints = input.fingerprints.clone();
    fingerprints.documents.get_mut(&script).unwrap().projected = true;
    let changed = dependencies::changed_documents(&input.snapshot, &fingerprints, Some(&old))
        .unwrap()
        .unwrap();
    assert!(changed.dirty.is_empty(), "a projection alone is not Dirty");
    // A projected source of producer `p` reads its endpoints' projections under `p` (#934),
    // whatever their default producer: only a change there makes it Dirty (Prime2, #1312).
    let projection = |state: u8| {
        let mut digest = [0; 32];
        digest[0] = state;
        digest
    };
    let mut base = old.clone();
    for document in &mut base.documents {
        let document = Arc::make_mut(document);
        Arc::make_mut(&mut document.fingerprint).producer_projections =
            std::collections::BTreeMap::from([
                ("p".into(), projection(1)),
                ("q".into(), projection(1)),
            ]);
    }
    // The projected source retains the endpoint whose eligibility its Read B read.
    let position = |key: &crate::native_records::DocumentKey| {
        base.order
            .iter()
            .position(|candidate| candidate == key)
            .unwrap()
    };
    let (main_at, util_at) = (position(main), position(util));
    Arc::make_mut(&mut Arc::make_mut(&mut base.documents[main_at]).references).eligibility =
        Arc::from([u32::try_from(util_at).unwrap()]);
    let mut projected = input.fingerprints.clone();
    for document in projected.documents.values_mut() {
        document.producer_projections = std::collections::BTreeMap::from([
            ("p".into(), projection(1)),
            ("q".into(), projection(1)),
        ]);
    }
    projected.documents.get_mut(main).unwrap().projected = true;
    projected
        .semantic_states
        .get_mut(main)
        .unwrap()
        .default_producer = Some("p".into());
    let dirty = |fingerprints: &crate::architecture_inputs::ArchitectureFingerprints| {
        dependencies::changed_documents(&input.snapshot, fingerprints, Some(&base))
            .unwrap()
            .unwrap()
            .dirty
    };
    assert!(dirty(&projected).is_empty(), "nothing changed");
    fingerprints = projected.clone();
    fingerprints.documents.get_mut(util).unwrap().semantic[0] ^= 1;
    assert_eq!(
        dirty(&fingerprints),
        std::collections::BTreeSet::from([util.clone()]),
        "an endpoint's default state is not what a projected source reads"
    );
    fingerprints = projected.clone();
    *fingerprints
        .documents
        .get_mut(util)
        .unwrap()
        .producer_projections
        .get_mut("q")
        .unwrap() = projection(2);
    assert!(
        dirty(&fingerprints).is_empty(),
        "another producer's projection"
    );
    fingerprints = projected.clone();
    *fingerprints
        .documents
        .get_mut(util)
        .unwrap()
        .producer_projections
        .get_mut("p")
        .unwrap() = projection(2);
    assert_eq!(
        dirty(&fingerprints),
        std::collections::BTreeSet::from([main.clone()]),
        "a projected source follows its endpoints' projections under its own producer"
    );
    fingerprints.documents.get_mut(main).unwrap().projected = false;
    assert!(
        dirty(&fingerprints).is_empty(),
        "a current source does not read its endpoints' eligibility"
    );
    fingerprints.documents.get_mut(main).unwrap().projected = true;
    fingerprints
        .documents
        .get_mut(util)
        .unwrap()
        .producer_projections
        .remove("p");
    assert_eq!(
        dirty(&fingerprints),
        std::collections::BTreeSet::from([main.clone()]),
        "a projection that appears or disappears is a change"
    );

    fingerprints = input.fingerprints.clone();
    fingerprints
        .documents
        .get_mut(util)
        .unwrap()
        .native
        .coverage_selected ^= true;
    let changed = dependencies::changed_documents(&input.snapshot, &fingerprints, Some(&old))
        .unwrap()
        .unwrap();
    assert_eq!(
        changed.native,
        std::collections::BTreeSet::from([util.clone()])
    );
    assert_eq!(
        changed.patched,
        std::collections::BTreeSet::from([main.clone()])
    );
    fingerprints.documents.get_mut(util).unwrap().sids[0] ^= 1;
    let changed = dependencies::changed_documents(&input.snapshot, &fingerprints, Some(&old))
        .unwrap()
        .unwrap();
    assert_eq!(
        changed.dirty,
        std::collections::BTreeSet::from([util.clone(), main.clone()])
    );
    assert!(
        changed.patched.is_empty(),
        "sid endpoints expand Dirty, not a new gate"
    );
}

#[test]
fn architecture_incremental_catalog_gate_and_margin() {
    let (_state, root, store) = incremental_checkout();
    let base = store.architecture_build_for_tests(None, None).unwrap();
    write(root.path(), "idle.rs", "pub fn renamed() { let _ = 1; }\n");
    publish(&store, root.path());
    let changed = store
        .architecture_build_for_tests(Some(&base.core), None)
        .unwrap();
    assert_eq!(
        changed.path,
        ArchitectureBuildPath::Full(FullReason::RustCatalogChanged)
    );
    assert_eq!(
        changed.core.canonical().unwrap(),
        store
            .architecture_build_for_tests(None, None)
            .unwrap()
            .core
            .canonical()
            .unwrap()
    );
    let many = SERVER.replace("util::u();", &"util::u();".repeat(30));
    write(root.path(), "crates/alpha/src/main.rs", &many);
    publish(&store, root.path());
    let mut changed = store.architecture_build_for_tests(None, None).unwrap();
    write(
        root.path(),
        "crates/alpha/src/main.rs",
        &format!("{many}// changed\n"),
    );
    publish(&store, root.path());
    let candidate = store
        .architecture_build_for_tests(Some(&changed.core), None)
        .unwrap();
    assert!(
        matches!(candidate.path, ArchitectureBuildPath::Incremental { .. }),
        "{:?}",
        candidate.path
    );
    let canonical = candidate.core.canonical().unwrap();
    let work = canonical["references"]["workUsed"].as_u64().unwrap() as usize;
    let dirty_base: usize = changed
        .core
        .0
        .dependencies
        .order
        .iter()
        .zip(&changed.core.0.dependencies.documents)
        .filter(|(key, _)| key.path.ends_with("main.rs"))
        .map(|(_, doc)| doc.references.work)
        .sum();
    let margin = candidate.references.executed_work + dirty_base;
    assert!(margin > work, "fixture crosses the margin before full Work");
    let mut work_limit = crate::references::set_reference_work_limit_for_tests(margin);
    let at = store
        .architecture_build_for_tests(Some(&changed.core), None)
        .unwrap();
    assert!(matches!(at.path, ArchitectureBuildPath::Incremental { .. }));
    work_limit.set(margin - 1);
    let below = store
        .architecture_build_for_tests(Some(&changed.core), None)
        .unwrap();
    assert_eq!(
        below.path,
        ArchitectureBuildPath::Full(FullReason::ReferenceWorkMargin)
    );
    assert_eq!(below.core.canonical().unwrap(), canonical);
    work_limit.set(work - 1);
    let incremental_error = store
        .architecture_build_for_tests(Some(&changed.core), None)
        .err()
        .unwrap();
    let full_error = store
        .architecture_build_for_tests(None, None)
        .err()
        .unwrap();
    assert_eq!(
        incremental_error.to_string(),
        full_error.to_string(),
        "full chooses the kept bound failure"
    );
    work_limit.set(usize::MAX);
    let base = Arc::get_mut(&mut changed.core.0).unwrap();
    let main = base
        .dependencies
        .order
        .iter()
        .position(|key| key.path.ends_with("main.rs"))
        .unwrap();
    Arc::make_mut(&mut Arc::make_mut(&mut base.dependencies.documents[main]).references).work =
        usize::MAX;
    let overflow = store
        .architecture_build_for_tests(Some(&changed.core), None)
        .unwrap();
    assert_eq!(
        overflow.path,
        ArchitectureBuildPath::Full(FullReason::IncrementalFailure),
        "overflow is an ordinary failed attempt, not a new gate"
    );
    assert_eq!(overflow.core.canonical().unwrap(), canonical);
}

#[test]
fn architecture_incremental_rules_presence_and_read_b_policy() {
    let (_state, _root, store) = incremental_checkout();
    let base = Arc::new(core(&store));
    let config = crate::rules::parse(RULES).unwrap();
    let empty = crate::rules::parse("version=1\n[roles.f]\nselect={name='*'}\n").unwrap();
    assert!(config.has_rules());
    assert!(!empty.has_rules(), "roles alone are not [[rules]]");
    for (config, selected, projected, pending, all) in [
        (Some(&config), true, false, false, true),
        (Some(&config), true, true, false, true),
        (Some(&config), true, true, true, false),
        (Some(&config), false, false, false, false),
        (Some(&empty), true, false, false, false),
        (None, true, false, false, false),
    ] {
        let mut input = native_input(&store);
        let key = input.fingerprints.documents.keys().next().unwrap().clone();
        let pin = input.snapshot.pin;
        let state = input.fingerprints.semantic_states.get_mut(&key).unwrap();
        state.tuple = Some(crate::architecture_inputs::SemanticTupleState {
            selected,
            state: "complete".into(),
            payload_id: Some(7),
        });
        if projected {
            state.projection = Some(crate::model::SemanticProjection {
                current_pin: pin,
                semantic_basis_pin: pin,
                basis_native_revision_id: "native".into(),
                semantic_status: if pending {
                    crate::model::SemanticStatus::CatchingUp
                } else {
                    crate::model::SemanticStatus::OlderBasis
                },
            });
        }
        let trace = Mutex::new(Default::default());
        let plan = prepare_architecture(
            ArchitectureReadA {
                input,
                store: &store,
                config,
                measure: true,
                progress: &|_| {},
                trace: &trace,
                work_limit: crate::references::ReferenceWorkLimitForTests::capture(),
            },
            Some(base.clone()),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(plan.semantic_keys().len(), if all { 4 } else { 0 });
    }
}

/// Captured native inputs with a compact admitted semantic row. This tests the pure
/// A/plan/B/finish seam; real producer imports belong to semantic conformance.
fn semantic_input(store: &Store, direct: bool) -> ArchitectureInput {
    use crate::architecture_inputs::{SemanticInputState, SemanticTupleState};
    use crate::references::{DocumentSemantic, SemanticAnchor, SemanticOutcome};
    let mut input = native_input(store);
    let source = input
        .snapshot
        .documents
        .iter()
        .position(|doc| doc.key.path.ends_with("main.rs"))
        .unwrap();
    let target = input
        .snapshot
        .documents
        .iter()
        .position(|doc| doc.key.path.ends_with("util.rs"))
        .unwrap();
    let sid = input.snapshot.documents[target]
        .declarations
        .iter()
        .find(|declaration| declaration.name.as_deref() == Some("u"))
        .unwrap()
        .syntax_id
        .clone();
    let call = input.snapshot.documents[source]
        .calls
        .iter()
        .find(|call| {
            input.snapshot.documents[source].file.text[call.range.start..call.range.end]
                .starts_with("util::u")
        })
        .unwrap()
        .clone();
    for (i, doc) in input.snapshot.documents.iter_mut().enumerate() {
        let mut state = SemanticInputState::default();
        if doc.key.language == "rust" {
            state.default_producer = Some("producer".into());
            doc.semantic = DocumentSemantic {
                producer_id: state.default_producer.clone(),
                ..Default::default()
            };
        }
        if i == source {
            state.tuple = Some(SemanticTupleState {
                selected: true,
                state: "complete".into(),
                payload_id: Some(7),
            });
            doc.semantic.complete = true;
            doc.semantic.coverage_state = Some("complete".into());
            doc.semantic.anchors = vec![SemanticAnchor {
                range: call.callee_range.clone().unwrap(),
                call_id: Some(call.id.clone()),
                type_role: false,
                direct,
                outcome: SemanticOutcome::Internal(target, sid.clone()),
                basis_site: true,
                provenance_id: "semantic-proof".into(),
                proof: crate::semantic_records::ProofIdentity::Literal("semantic-proof".into()),
                metadata: Default::default(),
            }];
        }
        input
            .fingerprints
            .documents
            .get_mut(&doc.key)
            .unwrap()
            .semantic = state.fingerprint();
        input
            .fingerprints
            .semantic_states
            .insert(doc.key.clone(), state);
        input.semantic_counts.sources.insert(
            doc.key.clone(),
            if i == source {
                dependencies::SemanticReadCounts {
                    rows: 1,
                    metadata: 2,
                    lookups: 3,
                }
            } else {
                Default::default()
            },
        );
    }
    input
}

#[test]
fn architecture_incremental_full_rules_and_direct_does_not_dirty_references() {
    let (_state, root, store) = incremental_checkout();
    let config = crate::rules::parse(RULES).unwrap();
    let base = Arc::new(
        ArchitectureCore::compute(
            &store,
            semantic_input(&store, true),
            Some(&config),
            true,
            &mut || Ok(()),
            &|_| {},
        )
        .unwrap(),
    );
    write(root.path(), "idle.rs", "pub fn idle() { let _ = 2; }\n");
    publish(&store, root.path());
    let mut current = semantic_input(&store, false);
    let oracle = ArchitectureCore::compute(
        &store,
        semantic_input(&store, false),
        Some(&config),
        true,
        &mut || Ok(()),
        &|_| {},
    )
    .unwrap();
    let documents = current
        .snapshot
        .documents
        .iter_mut()
        .map(|doc| std::mem::take(&mut doc.semantic))
        .collect();
    let counts = std::mem::take(&mut current.semantic_counts);
    let pin = current.snapshot.pin;
    let trace = Mutex::new(Default::default());
    let plan = prepare_architecture(
        ArchitectureReadA {
            input: current,
            store: &store,
            config: Some(&config),
            measure: true,
            progress: &|_| {},
            trace: &trace,
            work_limit: crate::references::ReferenceWorkLimitForTests::capture(),
        },
        Some(base.clone()),
        &mut || Ok(()),
    )
    .unwrap();
    let ArchitectureBuildPath::Incremental { dirty, patched, .. } = &plan.path else {
        panic!("{:?}", plan.path);
    };
    assert_eq!(
        dirty
            .iter()
            .map(|key| key.path.as_str())
            .collect::<Vec<_>>(),
        ["idle.rs"]
    );
    assert!(
        patched.is_empty(),
        "direct dispatch is not a reference dependency"
    );
    assert_eq!(
        plan.semantic_keys().len(),
        4,
        "nonempty rules use all B, including clean main"
    );
    let built = finish_architecture(
        plan,
        engine::ArchitectureReadB {
            pin,
            documents,
            counts,
        },
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(
        first_difference(&built.canonical().unwrap(), &oracle.canonical().unwrap()),
        None
    );
    let main = built
        .dependencies
        .order
        .iter()
        .position(|key| key.path.ends_with("main.rs"))
        .unwrap();
    assert!(built.references.source_shared(main, &base.references, main));
    assert!(Arc::ptr_eq(
        &built.dependencies.documents[main],
        &base.dependencies.documents[main]
    ));
    assert_eq!(
        trace
            .lock()
            .unwrap()
            .file_references
            .iter()
            .map(|key| key.path.as_str())
            .collect::<Vec<_>>(),
        ["idle.rs"]
    );
    assert_eq!(
        built.dependencies.documents[main].semantic,
        dependencies::SemanticReadCounts {
            rows: 1,
            metadata: 2,
            lookups: 3
        }
    );
}

#[test]
fn architecture_incremental_ordinary_failure_retries_full() {
    let (_state, _root, store) = incremental_checkout();
    let mut base = store.architecture_build_for_tests(None, None).unwrap();
    let core = Arc::get_mut(&mut base.core.0).unwrap();
    let index = core
        .dependencies
        .order
        .iter()
        .position(|key| key.path == "idle.rs")
        .unwrap();
    // Model an unusable retained recipe without changing any admitted current input.
    core.references.source_facts_mut_for_tests()[index].plan = None;
    let built = store
        .architecture_build_for_tests(Some(&base.core), None)
        .unwrap();
    assert_eq!(
        built.path,
        ArchitectureBuildPath::Full(FullReason::IncrementalFailure)
    );
    assert_eq!(
        built.core.canonical().unwrap(),
        store
            .architecture_build_for_tests(None, None)
            .unwrap()
            .core
            .canonical()
            .unwrap()
    );
    assert_eq!(built.semantic_sources.len(), 4);
    assert_eq!(built.references.file_references.len(), 4);

    let mut base = store.architecture_build_for_tests(None, None).unwrap();
    let core = Arc::get_mut(&mut base.core.0).unwrap();
    Arc::make_mut(&mut core.dependencies.documents[0])
        .semantic
        .rows = usize::MAX;
    let built = store
        .architecture_build_for_tests(Some(&base.core), None)
        .unwrap();
    assert_eq!(
        built.path,
        ArchitectureBuildPath::Full(FullReason::IncrementalFailure)
    );
    assert_eq!(
        built.core.canonical().unwrap(),
        store
            .architecture_build_for_tests(None, None)
            .unwrap()
            .core
            .canonical()
            .unwrap(),
        "selective admission failure discards retained counts and uses full B"
    );
}

#[test]
fn architecture_incremental_target_display_patch_matches_full() {
    let (_state, root, store) = incremental_checkout();
    write(
        root.path(),
        "crates/alpha/src/util.rs",
        "// basis\npub fn u() {}\n",
    );
    publish(&store, root.path());
    let base = Arc::new(
        ArchitectureCore::compute(
            &store,
            semantic_input(&store, true),
            None,
            true,
            &mut || Ok(()),
            &|_| {},
        )
        .unwrap(),
    );
    write(
        root.path(),
        "crates/alpha/src/util.rs",
        "// basis\n// moved\npub fn u() {}\n",
    );
    publish(&store, root.path());
    let oracle = ArchitectureCore::compute(
        &store,
        semantic_input(&store, true),
        None,
        true,
        &mut || Ok(()),
        &|_| {},
    )
    .unwrap();
    let mut input = semantic_input(&store, true);
    let pin = input.snapshot.pin;
    let mut documents: Vec<_> = input
        .snapshot
        .documents
        .iter_mut()
        .map(|doc| std::mem::take(&mut doc.semantic))
        .collect();
    for document in &mut documents {
        document.anchors.clear();
        document.relationships.clear();
    }
    let counts = std::mem::take(&mut input.semantic_counts);
    let trace = Mutex::new(Default::default());
    let plan = prepare_architecture(
        ArchitectureReadA {
            input,
            store: &store,
            config: None,
            measure: true,
            progress: &|_| {},
            trace: &trace,
            work_limit: crate::references::ReferenceWorkLimitForTests::capture(),
        },
        Some(base.clone()),
        &mut || Ok(()),
    )
    .unwrap();
    let ArchitectureBuildPath::Incremental { dirty, patched, .. } = &plan.path else {
        panic!("{:?}", plan.path);
    };
    assert_eq!(
        dirty
            .iter()
            .map(|key| key.path.as_str())
            .collect::<Vec<_>>(),
        ["crates/alpha/src/util.rs"]
    );
    assert_eq!(
        patched
            .iter()
            .map(|key| key.path.as_str())
            .collect::<Vec<_>>(),
        ["crates/alpha/src/main.rs"]
    );
    assert_eq!(plan.semantic_keys(), *dirty);
    let built = finish_architecture(
        plan,
        engine::ArchitectureReadB {
            pin,
            documents,
            counts,
        },
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(
        first_difference(&built.canonical().unwrap(), &oracle.canonical().unwrap()),
        None
    );
    let main = built
        .dependencies
        .order
        .iter()
        .position(|key| key.path.ends_with("main.rs"))
        .unwrap();
    assert!(
        !built.references.source_shared(main, &base.references, main),
        "Patch clones the source"
    );
    assert!(
        Arc::ptr_eq(
            &built.metrics().unwrap()[&built.dependencies.order[main]],
            &base.metrics().unwrap()[&base.dependencies.order[main]]
        ),
        "Patch does not remeasure its source"
    );
    assert_eq!(
        trace
            .lock()
            .unwrap()
            .file_references
            .iter()
            .map(|key| key.path.as_str())
            .collect::<Vec<_>>(),
        ["crates/alpha/src/util.rs"]
    );
}

#[test]
fn architecture_incremental_projected_global_state_and_native_only_metrics() {
    let (_state, root, store) = incremental_checkout();
    let base = Arc::new(
        ArchitectureCore::compute(
            &store,
            semantic_input(&store, true),
            None,
            true,
            &mut || Ok(()),
            &|_| {},
        )
        .unwrap(),
    );
    write(root.path(), "idle.rs", "pub fn idle() { let _ = 2; }\n");
    publish(&store, root.path());
    let projected = || {
        let mut input = semantic_input(&store, true);
        let pin = input.snapshot.pin;
        let doc = input
            .snapshot
            .documents
            .iter_mut()
            .find(|doc| doc.key.path.ends_with("main.rs"))
            .unwrap();
        let projection = crate::model::SemanticProjection {
            current_pin: pin,
            semantic_basis_pin: base.pin,
            basis_native_revision_id: "basis".into(),
            semantic_status: crate::model::SemanticStatus::OlderBasis,
        };
        doc.semantic.projection = Some(projection.clone());
        doc.semantic.anchors[0]
            .metadata
            .insert("projection".into(), json!(projection));
        let state = input
            .fingerprints
            .semantic_states
            .get_mut(&doc.key)
            .unwrap();
        state.projection = Some(projection);
        let fingerprint = input.fingerprints.documents.get_mut(&doc.key).unwrap();
        fingerprint.projected = true;
        fingerprint.semantic = state.fingerprint();
        input
    };
    let oracle =
        ArchitectureCore::compute(&store, projected(), None, true, &mut || Ok(()), &|_| {})
            .unwrap();
    let mut input = projected();
    let pin = input.snapshot.pin;
    let documents = input
        .snapshot
        .documents
        .iter_mut()
        .map(|doc| std::mem::take(&mut doc.semantic))
        .collect();
    let counts = std::mem::take(&mut input.semantic_counts);
    let trace = Mutex::new(Default::default());
    let plan = prepare_architecture(
        ArchitectureReadA {
            input,
            store: &store,
            config: None,
            measure: true,
            progress: &|_| {},
            trace: &trace,
            work_limit: crate::references::ReferenceWorkLimitForTests::capture(),
        },
        Some(base.clone()),
        &mut || Ok(()),
    )
    .unwrap();
    let keys = plan.semantic_keys();
    assert_eq!(keys.len(), 2, "native idle plus every current projection");
    let built = finish_architecture(
        plan,
        engine::ArchitectureReadB {
            pin,
            documents,
            counts,
        },
        &mut || Ok(()),
    )
    .unwrap();
    let canonical = built.canonical().unwrap();
    assert_eq!(
        first_difference(&canonical, &oracle.canonical().unwrap()),
        None
    );
    assert_eq!(canonical["references"]["documentStatus"], json!([1, 0, 0]));
    let main = built
        .dependencies
        .order
        .iter()
        .find(|key| key.path.ends_with("main.rs"))
        .unwrap();
    assert!(Arc::ptr_eq(
        &built.metrics().unwrap()[main],
        &base.metrics().unwrap()[main]
    ));
}

#[test]
fn architecture_incremental_replayed_work_replaces_only_the_dependency_wrapper() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, text) in [
        (
            "Cargo.toml",
            "[package]\nname='app'\n[dependencies]\nserde='1'\n[dev-dependencies]\nserde='1'\n",
        ),
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
    ] {
        write(root.path(), path, text);
    }
    let store =
        crate::testing::retry_admission(|| Store::open_for_tests(state.path(), root.path()));
    publish(&store, root.path());
    let base = store.architecture_build_for_tests(None, None).unwrap();
    let frozen = base.core.canonical().unwrap();
    write(
        root.path(),
        "src/a.rs",
        "pub fn a() { crate::shared::f(); serde::de::value(); }\n",
    );
    publish(&store, root.path());
    let built = store
        .architecture_build_for_tests(Some(&base.core), None)
        .unwrap();
    assert!(
        matches!(built.path, ArchitectureBuildPath::Incremental { .. }),
        "{:?}",
        built.path
    );
    let canonical = built.core.canonical().unwrap();
    assert_eq!(
        canonical,
        store
            .architecture_build_for_tests(None, None)
            .unwrap()
            .core
            .canonical()
            .unwrap()
    );
    let b = built
        .core
        .0
        .dependencies
        .order
        .iter()
        .position(|key| key.path == "src/b.rs")
        .unwrap();
    assert_ne!(
        base.core.0.dependencies.documents[b].references.work,
        built.core.0.dependencies.documents[b].references.work,
        "fresh q_s is not a retained total"
    );
    let sharing = built.core.sharing(&base.core);
    let shared = &sharing[&built.core.0.dependencies.order[b]];
    assert!(
        shared.source && shared.metric && shared.fingerprint && shared.endpoints && shared.plan,
        "{shared:?}"
    );
    assert!(
        !shared.references && !shared.dependency,
        "only q_s's wrappers change: {shared:?}"
    );
    assert_eq!(
        base.core.canonical().unwrap(),
        frozen,
        "earlier package provenance is immutable"
    );
    assert_ne!(
        canonical["references"]["externalPackages"], frozen["references"]["externalPackages"],
        "the new core owns current Cargo provenance despite whole-source sharing"
    );
}

#[test]
fn architecture_incremental_site_bound_counts_dirty_and_retained_before_sampling() {
    let (_state, root, store) = incremental_checkout();
    write(
        root.path(),
        "crates/alpha/src/util.rs",
        "pub fn u() { std::mem::drop(1); }\n",
    );
    publish(&store, root.path());
    let base = store.architecture_build_for_tests(None, None).unwrap();
    let sites: usize = base
        .core
        .0
        .dependencies
        .documents
        .iter()
        .map(|doc| doc.references.sites)
        .sum();
    assert!(sites > 1);
    let clean = base
        .core
        .0
        .dependencies
        .order
        .iter()
        .position(|key| key.path.ends_with("util.rs"))
        .unwrap();
    assert!(
        base.core.0.dependencies.documents[clean].references.sites > 0,
        "clean source contributes sites"
    );
    write(
        root.path(),
        "crates/alpha/src/main.rs",
        &format!("{SERVER}// changed\n"),
    );
    publish(&store, root.path());
    let oracle = store
        .architecture_build_for_tests(None, None)
        .unwrap()
        .core
        .canonical()
        .unwrap();
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            crate::references::set_site_limit_for_tests(250_000);
        }
    }
    let _reset = Reset;
    for limit in [sites, sites + 1] {
        crate::references::set_site_limit_for_tests(limit);
        let built = store
            .architecture_build_for_tests(Some(&base.core), None)
            .unwrap();
        assert!(matches!(
            built.path,
            ArchitectureBuildPath::Incremental { .. }
        ));
        assert_eq!(built.core.canonical().unwrap(), oracle);
        let selected = built
            .core
            .0
            .references(&[], &[], 1, &mut || Ok(()))
            .unwrap();
        let coverage: usize = selected
            .coverage
            .values()
            .map(|coverage| coverage.sites)
            .sum();
        assert_eq!(
            coverage, sites,
            "sampling cannot reduce accepted-site admission"
        );
    }
    crate::references::set_site_limit_for_tests(sites - 1);
    let attempted = store
        .architecture_build_for_tests(Some(&base.core), None)
        .err()
        .unwrap();
    let full = store
        .architecture_build_for_tests(None, None)
        .err()
        .unwrap();
    assert_eq!(attempted.to_string(), full.to_string());
    assert_eq!(full.to_string(), "bounded_work: reference site count");
}

#[test]
fn architecture_incremental_work_context_applies_on_prepare_and_finish_workers() {
    use crate::references::{ReferenceWorkLimitForTests, set_reference_work_limit_for_tests};
    let (_state, _root, store) = incremental_checkout();
    let base = Arc::new(core(&store));
    let captured = {
        let _requester = set_reference_work_limit_for_tests(0);
        ReferenceWorkLimitForTests::capture()
    };
    let trace = Mutex::new(Default::default());
    let input = native_input(&store);
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _worker = set_reference_work_limit_for_tests(1_000);
                let error = prepare_architecture(
                    ArchitectureReadA {
                        input,
                        store: &store,
                        config: None,
                        measure: true,
                        progress: &|_| {},
                        trace: &trace,
                        work_limit: captured,
                    },
                    Some(base),
                    &mut || Ok(()),
                )
                .err()
                .unwrap();
                assert!(error.to_string().starts_with("bounded_work:"), "{error}");
                assert_eq!(
                    crate::references::work::reference_limit(1, 0),
                    1_000,
                    "preparation applies the captured zero and restores the worker's budget"
                );
            })
            .join()
            .unwrap();
    });
    let mut plan = prepare_architecture(
        ArchitectureReadA {
            input: native_input(&store),
            store: &store,
            config: None,
            measure: true,
            progress: &|_| {},
            trace: &trace,
            work_limit: captured,
        },
        None,
        &mut || Ok(()),
    )
    .unwrap();
    let response = store.evidence_response().unwrap();
    let read_b = response
        .architecture_plan_read_b(&mut plan, &mut || Ok(()))
        .unwrap();
    let read_b = response.finish(read_b).unwrap();
    drop(response);
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _worker = set_reference_work_limit_for_tests(1_000);
                let error = finish_architecture(plan, read_b, &mut || Ok(()))
                    .err()
                    .unwrap();
                assert!(error.to_string().starts_with("bounded_work:"), "{error}");
                assert_eq!(
                    crate::references::work::reference_limit(1, 0),
                    1_000,
                    "finish applies the captured zero and restores the worker's budget"
                );
            })
            .join()
            .unwrap();
    });
    store.architecture_build_for_tests(None, None).unwrap();
}

#[test]
fn architecture_incremental_retry_excludes_cancelled_and_contended_errors() {
    assert!(engine::retryable_incremental_error(&anyhow::anyhow!(
        "bounded_work: preparation"
    )));
    let integrity = SelectedIntegrity("missing semantic freshness".into());
    assert!(!engine::retryable_incremental_error(
        &anyhow::Error::from(integrity).context("cancelled: read B")
    ));
    assert!(!engine::retryable_incremental_error(
        &crate::store::topology::StorageBusy.into()
    ));
}
