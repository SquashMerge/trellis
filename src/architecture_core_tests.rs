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
    assert!(at.starts_with("/interfaces/routes/0"), "{at}");
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
