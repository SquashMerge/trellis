use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};
use trellis::{
    indexer::{IndexOptions, index_workspace_with_native},
    model::IndexPin,
    references::{self, DocumentInput, Snapshot},
    rules,
};
fn snapshot(files: &[(&str, &str)]) -> Snapshot {
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let (graph, native) = index_workspace_with_native(
        &IndexOptions::new(root.path().to_owned()),
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let documents = graph
        .files
        .into_iter()
        .map(|file| {
            let key = native
                .revision
                .documents
                .iter()
                .find(|d| d.key.path == file.path)
                .unwrap()
                .key
                .clone();
            let declarations = native
                .declarations
                .iter()
                .filter(|d| d.document == key)
                .cloned()
                .collect();
            let calls = native
                .calls
                .iter()
                .filter(|c| c.document == key)
                .cloned()
                .collect();
            DocumentInput {
                key,
                file,
                declarations,
                calls,
            }
        })
        .collect();
    Snapshot {
        pin: IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 4,
        },
        documents,
    }
}
fn evaluate(text: &str, snapshot: &Snapshot) -> Value {
    let config = rules::parse(text).unwrap();
    let refs = references::project(snapshot, &[], 5, &mut || Ok(())).unwrap();
    rules::evaluate(&config, snapshot, &refs, &[], &mut || Ok(())).unwrap()
}
fn pair<'a>(data: &'a Value, source: &str, destination: &str) -> &'a Value {
    data["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| {
            r["rule"] == "$layers"
                && r["target"]["path"] == source
                && r["destination"]["path"] == destination
        })
        .unwrap()
}
fn diagnostic(text: &str) -> Value {
    let error = match rules::parse(text) {
        Ok(_) => panic!("expected invalid rules"),
        Err(e) => e,
    };
    rules::diagnostic(&error).unwrap()
}
#[test]
fn rules_closed_parser_rejects_malformed_unknown_and_invalid_references_with_locations() {
    for text in [
        "version = [",
        "version = 2",
        "version = 1\nextra = true",
        "version=1\n[roles.x]\nselect={annotation=\"Service\"}",
        "version=1\n[roles.x]\nselect={name=\"X\"}\n[[rules]]\nid=\"x\"\nfor_each=\"x\"\nrequire=[{publishes=\"x\"}]",
        "version=1\n[layers.x]\norder=1\nshared=true\npaths=[\"src/**\"]",
        "version=1\n[layers.x]\nshared=false\npaths=[\"src/**\"]",
        "version=1\n[roles.x]\nselect={paths=[\"../src/**\"]}",
        "version=1\n[roles.x]\nselect={paths=[\"src/{a,b}\"]}",
        "version=1\n[[rules]]\nid=\"x\"\nfor_each=\"missing\"\nrequire=[{calls=\"missing\"}]",
        "version=1\n[[exceptions]]\nrule=\"$layers\"\ntarget={source_set=\"set\",language=\"rust\",path=\"src/x.rs\",sid=\"not-sid\"}\nreason=\"ok\"",
    ] {
        let d = diagnostic(text);
        assert_eq!(d["code"], "invalid_rules");
        assert_eq!(d["path"], rules::RULES_PATH);
        assert!(d["line"].as_u64().unwrap() >= 1);
        assert!(d["column"].as_u64().unwrap() >= 1);
        assert!(d["range"]["end"].as_u64().unwrap() <= text.len() as u64);
    }
    let d = diagnostic("# é\r\nversion=1\r\n[roles.x]\r\nselect={language=[\"kotlin\"]}");
    assert_eq!(d["line"], 4);
    assert_eq!(d["kind"], "selector");
}
#[test]
fn rules_parser_bounds_all_toml_nesting_shapes_and_collections() {
    let oversized = " ".repeat(2 * 1024 * 1024 + 1);
    assert_eq!(diagnostic(&oversized)["kind"], "too_large");
    for text in [
        format!("version=1\nx={}1{}", "[".repeat(200), "]".repeat(200)),
        format!("version=1\nx={}1{}", "{x=".repeat(200), "}".repeat(200)),
        format!("version=1\n{}=1", vec!["a"; 200].join(".")),
        format!("version=1\n[{}]\nx=1", vec!["a"; 200].join(".")),
    ] {
        let d = diagnostic(&text);
        assert_eq!(d["code"], "invalid_rules");
        assert_eq!(d["kind"], "too_deep", "{d}");
    }
    let mut text = "version=1\n".to_owned();
    for i in 0..1025 {
        text.push_str(&format!("[roles.r{i}]\nselect={{name=\"X\"}}\n"));
    }
    assert_eq!(diagnostic(&text)["kind"], "too_large");
    assert!(
        rules::parse("# [[{{ fake nesting\nversion=1\n[roles.x]\nselect={extends='[[{{X'}").is_ok()
    );
}
#[test]
fn rules_layer_order_shared_and_explicit_policy_are_not_table_order() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "fn f(){crate::service::run();crate::peer::run();crate::shared::run();}",
        ),
        ("src/service.rs", "pub fn run(){crate::entry::run();}"),
        ("src/entry.rs", "pub fn run(){}"),
        ("src/peer.rs", "pub fn run(){}"),
        ("src/shared.rs", "pub fn run(){crate::entry::run();}"),
        ("src/unassigned.rs", "pub fn run(){}"),
    ]);
    let text = r#"version=1
[layers.storage]
order=3
paths=["src/service.rs"]
[layers.entry]
order=1
paths=["src/lib.rs","src/entry.rs"]
[layers.peer]
order=1
paths=["src/peer.rs"]
[layers.shared]
shared=true
paths=["src/shared.rs"]
"#;
    let data = evaluate(text, &input);
    assert_eq!(
        pair(&data, "src/lib.rs", "src/service.rs")["status"],
        "conforms"
    );
    assert_eq!(
        pair(&data, "src/service.rs", "src/entry.rs")["status"],
        "deviates"
    );
    assert_eq!(
        pair(&data, "src/lib.rs", "src/shared.rs")["status"],
        "conforms"
    );
    for (s, t) in [
        ("src/lib.rs", "src/peer.rs"),
        ("src/shared.rs", "src/entry.rs"),
    ] {
        assert_eq!(pair(&data, s, t)["status"], "unknown");
        assert_eq!(
            pair(&data, s, t)["unresolved"],
            json!(["layer_policy_unspecified"])
        );
    }
    assert!(
        data["unassigned"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["path"] == "src/unassigned.rs")
    );
    let forbidden = evaluate(
        &format!("{text}\n[layer_policy]\npeers='forbid'\nshared_outbound='forbid'"),
        &input,
    );
    assert_eq!(
        pair(&forbidden, "src/lib.rs", "src/peer.rs")["status"],
        "deviates"
    );
    let allowed = evaluate(
        &format!("{text}\n[layer_policy]\npeers='allow'\nshared_outbound='allow'"),
        &input,
    );
    assert_eq!(
        pair(&allowed, "src/shared.rs", "src/entry.rs")["status"],
        "conforms"
    );
    let reordered = text.replace(
        "[layers.storage]\norder=3\npaths=[\"src/service.rs\"]\n",
        "",
    ) + "\n[layers.storage]\norder=3\npaths=[\"src/service.rs\"]\n";
    assert_eq!(data["instances"], evaluate(&reordered, &input)["instances"]);
}
#[test]
fn rules_layer_exceptions_are_exact_edge_pairs_and_tests_stay_separate() {
    let input = snapshot(&[
        ("src/lib.rs", "pub fn root(){}"),
        (
            "src/store.rs",
            "pub fn run(){crate::entry::run();crate::other::run();}\n#[cfg(test)] mod tests{fn f(){crate::entry::run();}}\n#[cfg(feature=\"test-support\")] fn seam(){crate::entry::run();}",
        ),
        ("src/entry.rs", "pub fn run(){}"),
        ("src/other.rs", "pub fn run(){}"),
    ]);
    let text = "version=1\n[layers.entry]\norder=1\npaths=['src/entry.rs','src/other.rs','src/lib.rs']\n[layers.store]\norder=3\npaths=['src/store.rs']";
    let first = evaluate(text, &input);
    let row = pair(&first, "src/store.rs", "src/entry.rs");
    let target = &row["target"];
    let destination = &row["destination"];
    let config_target = |v: &Value| {
        format!(
            "{{source_set='{}',language='{}',path='{}',sid='{}'}}",
            v["sourceSetId"].as_str().unwrap(),
            v["language"].as_str().unwrap(),
            v["path"].as_str().unwrap(),
            v["sid"].as_str().unwrap()
        )
    };
    let text = format!(
        "{text}\n[[exceptions]]\nrule='$layers'\ntarget={}\ndestination={}\nreason='Reviewed existing edge'",
        config_target(target),
        config_target(destination)
    );
    let data = evaluate(&text, &input);
    let row = pair(&data, "src/store.rs", "src/entry.rs");
    assert_eq!(row["status"], "deviates");
    assert_eq!(row["excepted"], true);
    assert_eq!(row["referenceCount"], 2);
    assert_eq!(row["exceptionReason"], "Reviewed existing edge");
    assert_eq!(
        pair(&data, "src/store.rs", "src/other.rs")["excepted"],
        false
    );
    assert_eq!(data["counts"]["deviates"], 1);
    assert_eq!(data["testOnly"].as_array().unwrap().len(), 1);
    assert_eq!(data["testOnly"][0]["status"], "deviates");
    assert_eq!(data["testOnly"][0]["unresolved"], json!(["test_only"]));
    let explained = rules::explain(data.clone(), "$layers", target, Some(destination)).unwrap();
    assert_eq!(explained["instances"], json!([row]));
    let config = rules::parse(&text).unwrap();
    let refs = references::project(&input, &[], 5, &mut || Ok(())).unwrap();
    let selected = rules::evaluate(
        &config,
        &input,
        &refs,
        &["src/store.rs".into()],
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(selected["instances"], data["instances"]);
}
#[test]
fn rules_overlapping_layers_fail_atomically_and_no_edges_does_not_prove_calls() {
    let input = snapshot(&[("src/lib.rs", "fn Resource(){}")]);
    let text = "version=1\n[layers.a]\norder=1\npaths=['src/**']\n[layers.b]\norder=2\npaths=['src/lib.rs']";
    let config = rules::parse(text).unwrap();
    let refs = references::project(&input, &[], 5, &mut || Ok(())).unwrap();
    let error = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap_err();
    assert_eq!(rules::diagnostic(&error).unwrap()["kind"], "ambiguity");
    let text = "version=1\n[roles.resource]\nselect={name='*Resource',language=['rust'],paths=['src/**']}\n[roles.service]\nselect={name='*Service'}\n[[rules]]\nid='flow'\nfor_each='resource'\nrequire=[{calls='service'}]\nforbid=[{extends='service'},{implements='service'}]";
    let data = evaluate(text, &input);
    assert_eq!(data["counts"]["comparable"], 1);
    assert_eq!(data["counts"]["unknown"], 1);
    assert_eq!(data["counts"]["deviates"], 0);
    assert!(
        data["instances"][0]["constraints"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["status"] == "unknown")
    );
}
#[test]
fn rules_syntax_inheritance_selectors_distinguish_extends_and_implements_without_edges() {
    let input = snapshot(&[
        (
            "A.java",
            "interface Port {} class Base {} class Resource extends Base implements Port {void call(){missing();}}",
        ),
        ("a.py", "class Resource(Base):\n    pass\n"),
        ("a.js", "class Resource extends Base {}"),
        (
            "src/lib.rs",
            "trait Port{} struct Resource; impl Port for Resource{}",
        ),
    ]);
    let text = r#"version=1
[roles.extending]
select={extends="Base",name="Resource"}
[roles.implementing]
select={implements="Port",name="Resource"}
[roles.any]
select={paths=["**"]}
[[rules]]
id="extends-check"
for_each="extending"
require=[{calls="any"}]
[[rules]]
id="implements-check"
for_each="implementing"
forbid=[{implements="any"}]
"#;
    let data = evaluate(text, &input);
    let rows = data["instances"].as_array().unwrap();
    for path in ["A.java", "a.py", "a.js"] {
        assert!(
            rows.iter()
                .any(|r| r["rule"] == "extends-check" && r["target"]["path"] == path)
        );
    }
    assert!(
        rows.iter()
            .any(|r| r["rule"] == "implements-check" && r["target"]["path"] == "A.java")
    );
    assert!(rows.iter().all(|r| r["status"] == "unknown"));
    assert!(rows.iter().any(|r| {
        r["rule"] == "implements-check"
            && r["target"]["path"] == "a.py"
            && r["unresolved"]
                .as_array()
                .unwrap()
                .contains(&json!("selector_relationship_unavailable"))
    }));
    let java = rows
        .iter()
        .find(|r| r["rule"] == "extends-check" && r["target"]["path"] == "A.java")
        .unwrap();
    assert_eq!(
        java["constraints"][0]["unresolved"],
        json!(["call_binding_unavailable"])
    );
}
#[test]
fn rules_config_load_hash_and_cancellation_are_atomic() {
    let root = tempfile::tempdir().unwrap();
    assert!(rules::load(root.path()).unwrap().is_none());
    std::fs::create_dir(root.path().join(".trellis")).unwrap();
    std::fs::write(root.path().join(rules::RULES_PATH), "version=1").unwrap();
    let config = rules::load(root.path()).unwrap().unwrap();
    assert_eq!(rules::descriptor(&config)["source"], "checkout-config");
    assert_eq!(config.text, "version=1");
    let input = snapshot(&[("src/lib.rs", "fn f(){}")]);
    let refs = references::project(&input, &[], 5, &mut || Ok(())).unwrap();
    let error = rules::evaluate(&config, &input, &refs, &[], &mut || {
        anyhow::bail!("cancelled")
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
}

#[test]
fn rules_unresolved_calls_preserve_actual_occurrences_not_name_match_bindings() {
    let input = snapshot(&[("src/lib.rs", "fn Resource(){Service();} fn Service(){}")]);
    let text = "version=1\n[roles.source]\nselect={name='Resource'}\n[roles.target]\nselect={name='Service'}\n[[rules]]\nid='flow'\nfor_each='source'\nforbid=[{calls='target'}]";
    let data = evaluate(text, &input);
    let row = &data["instances"][0];
    assert_eq!(row["status"], "unknown");
    assert!(
        row["constraints"][0]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "call" && e["callId"].as_str().unwrap().starts_with("occ:v2:"))
    );
    assert_eq!(data["counts"]["deviates"], 0);
}

#[test]
fn rules_without_layers_do_not_enable_unrelated_layer_instances() {
    let input = snapshot(&[
        ("src/lib.rs", "fn f(){crate::store::run();}"),
        ("src/store.rs", "pub fn run(){}"),
    ]);
    let data = evaluate("version=1", &input);
    assert!(data["instances"].as_array().unwrap().is_empty());
    assert!(data["testOnly"].as_array().unwrap().is_empty());
    assert_eq!(data["unassigned"].as_array().unwrap().len(), 2);
    let config = rules::parse("version=1").unwrap();
    let refs = references::project(&input, &[], 5, &mut || Ok(())).unwrap();
    let error =
        rules::evaluate(&config, &input, &refs, &["../bad".into()], &mut || Ok(())).unwrap_err();
    assert!(error.to_string().starts_with("invalid_request:"));
    assert!(rules::diagnostic(&error).is_none());
}

#[test]
fn rules_large_valid_glob_compilation_returns_located_error() {
    let text = format!(
        "version=1\n[roles.x]\nselect={{name=\"{}\"}}",
        "a?".repeat(500_000)
    );
    let d = diagnostic(&text);
    assert_eq!(d["code"], "invalid_rules");
    assert_eq!(d["kind"], "glob");
    assert_eq!(d["attribution"], "token");
    assert_eq!(d["line"], 3);
    assert!(d["range"]["end"].as_u64().unwrap() > 1_000_000);
}
