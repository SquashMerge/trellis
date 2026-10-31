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
                semantic: Default::default(),
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
        manifests: vec![],
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

// Exercise assignment through the admitted snapshot and public rules evaluator.
fn specificity_snapshot(path: &str) -> Snapshot {
    let mut module = format!(
        "pub mod {};",
        path.rsplit('/').next().unwrap().trim_end_matches(".rs")
    );
    let parts: Vec<_> = path.strip_prefix("src/").unwrap().split('/').collect();
    for directory in parts[..parts.len() - 1].iter().rev() {
        module = format!("pub mod {directory} {{ {module} }}");
    }
    let root = format!("mod destination; {module}");
    snapshot(&[
        ("src/lib.rs", &root),
        ("src/destination.rs", "pub fn run() {}"),
        (path, "pub fn run() { crate::destination::run(); }"),
        ("other/unassigned.rs", "pub fn unused() {}"),
    ])
}

fn specificity_config(layers: &[(&str, &[&str])], reverse: bool) -> String {
    let mut text = String::from(
        "version=1\n[layers.destination]\norder=9\npaths=['src/lib.rs','src/destination.rs']\n",
    );
    let mut layers = layers.to_vec();
    if reverse {
        layers.reverse();
    }
    for (name, patterns) in layers {
        let mut patterns = patterns.to_vec();
        if reverse {
            patterns.reverse();
        }
        let patterns = patterns
            .iter()
            .map(|pattern| format!("'{pattern}'"))
            .collect::<Vec<_>>()
            .join(",");
        text.push_str(&format!("[layers.{name}]\norder=1\npaths=[{patterns}]\n"));
    }
    text
}

#[test]
fn rules_layer_specificity_selects_exact_and_proper_glob_subsets_independent_of_order() {
    for (path, alpha, beta, winner) in [
        (
            "src/store.rs",
            vec!["src/store.rs"],
            vec!["src/*.rs"],
            "alpha",
        ),
        (
            "src/store/search/item.rs",
            vec!["src/store/search/**"],
            vec!["src/store/**"],
            "alpha",
        ),
        ("src/a.rs", vec!["src/?.rs"], vec!["src/*.rs"], "alpha"),
        ("src/ab.rs", vec!["src/*??*"], vec!["src/*"], "alpha"),
        (
            "src/ab/b.rs",
            vec!["src/*/*.rs"],
            vec!["src/**/*.rs"],
            "alpha",
        ),
        // ** also accepts zero directories; it must not defeat the narrower glob.
        ("src/a.rs", vec!["src/*.rs"], vec!["src/**/*.rs"], "alpha"),
        (
            "src/store.rs",
            vec!["src/store.rs", "src/*.rs", "src/store.rs"],
            vec!["src/**"],
            "alpha",
        ),
        // An unmatched exact pattern cannot promote its layer's broad match.
        (
            "src/a.rs",
            vec!["src/absent.rs", "src/*"],
            vec!["src/?.rs"],
            "beta",
        ),
        ("src/é.rs", vec!["src/é*.rs"], vec!["src/*.rs"], "alpha"),
    ] {
        let input = specificity_snapshot(path);
        let layers = [("alpha", alpha.as_slice()), ("beta", beta.as_slice())];
        let data = evaluate(&specificity_config(&layers, false), &input);
        let row = pair(&data, path, "src/destination.rs");
        assert_eq!(row["sourceLayer"], winner, "{path}");
        assert_eq!(row["destinationLayer"], "destination", "{path}");
        assert_eq!(row["status"], "conforms", "{path}");
        let reversed = evaluate(&specificity_config(&layers, true), &input);
        assert_eq!(data["instances"], reversed["instances"], "{path}");
        assert_eq!(data["unassigned"], reversed["unassigned"], "{path}");
    }
}

#[test]
fn rules_layer_specificity_ignores_lower_rank_ties_under_one_exact_winner() {
    let input = specificity_snapshot("src/store.rs");
    let layers: &[(&str, &[&str])] = &[
        ("alpha", &["src/*.rs", "src/*"]),
        ("beta", &["src/*", "src/*.rs"]),
        ("winner", &["src/store.rs", "src/absent.rs"]),
    ];
    for reverse in [false, true] {
        let data = evaluate(&specificity_config(layers, reverse), &input);
        assert_eq!(
            pair(&data, "src/store.rs", "src/destination.rs")["sourceLayer"],
            "winner"
        );
    }
}

#[test]
fn rules_layer_specificity_equal_and_incomparable_matches_are_located_sorted_ambiguities() {
    for (path, alpha, zeta) in [
        ("src/store.rs", vec!["src/store.rs"], vec!["src/store.rs"]),
        ("src/a.rs", vec!["src/*.rs"], vec!["src/*.rs"]),
        ("src/a.rs", vec!["src/*?*"], vec!["src/?*"]),
        ("src/ab/b.rs", vec!["src/a*/b.rs"], vec!["src/ab/*.rs"]),
        (
            "src/a.rs",
            vec!["src/?*", "src/*?*", "src/absent.rs"],
            vec!["src/*?*", "src/?*", "src/missing.rs"],
        ),
    ] {
        let input = specificity_snapshot(path);
        let refs = references::project(&input, &[], 5, &mut || Ok(())).unwrap();
        let layers = [("zeta", zeta.as_slice()), ("alpha", alpha.as_slice())];
        let mut messages = Vec::new();
        for reverse in [false, true] {
            let text = specificity_config(&layers, reverse);
            let config = rules::parse(&text).unwrap();
            let error = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap_err();
            let diagnostic = rules::diagnostic(&error).unwrap();
            assert_eq!(diagnostic["code"], "invalid_rules");
            assert_eq!(diagnostic["kind"], "ambiguity");
            assert_eq!(diagnostic["path"], rules::RULES_PATH);
            assert_eq!(diagnostic["attribution"], "token");
            let start = diagnostic["range"]["start"].as_u64().unwrap() as usize;
            let end = diagnostic["range"]["end"].as_u64().unwrap() as usize;
            let table = text.find("[layers.alpha]").unwrap();
            let body = table + "[layers.alpha]\n".len();
            let next = text[body..]
                .find("[layers.")
                .map_or(text.len(), |i| body + i);
            assert!(
                start >= table && start < next && end <= next,
                "{diagnostic}"
            );
            assert_eq!(
                diagnostic["line"],
                text[..start].bytes().filter(|b| *b == b'\n').count() + 1
            );
            let message = diagnostic["message"].as_str().unwrap();
            assert!(message.contains(path), "{diagnostic}");
            assert!(
                message.find("alpha").unwrap() < message.find("zeta").unwrap(),
                "{diagnostic}"
            );
            let mut patterns = alpha
                .iter()
                .chain(&zeta)
                .copied()
                .filter(|p| !p.contains("absent") && !p.contains("missing"))
                .collect::<Vec<_>>();
            patterns.sort_unstable();
            patterns.dedup();
            for pattern in &patterns {
                assert!(message.contains(pattern), "{diagnostic}");
            }
            messages.push(message.to_owned());
        }
        assert_eq!(messages[0], messages[1], "{path}");
    }
}

#[test]
fn rules_layer_specificity_preserves_modes_roles_filters_and_unassigned_paths() {
    let input = snapshot(&[
        ("src/lib.rs", "mod store; mod destination;"),
        (
            "src/store.rs",
            "pub fn run() { crate::destination::run(); } #[cfg(test)] mod tests { fn check() { crate::destination::run(); } }",
        ),
        ("src/destination.rs", "pub fn run() {}"),
        ("other/unassigned.rs", "pub fn unused() {}"),
    ]);
    for (mode, policy, status) in [
        ("order=1", "", "conforms"),
        ("shared=true", "", "deviates"),
        (
            "shared=true",
            "[layer_policy]\nshared_outbound='allow'\n",
            "conforms",
        ),
    ] {
        let text = format!(
            "version=1\n{policy}[layers.destination]\norder=9\npaths=['src/lib.rs','src/destination.rs']\n[layers.broad]\norder=99\npaths=['src/**']\n[layers.selected]\n{mode}\npaths=['src/store.rs']\n"
        );
        let config = rules::parse(&text).unwrap();
        for paths in [
            vec![],
            vec!["src/store.rs".into()],
            vec!["absent.rs".into()],
        ] {
            let refs = references::project(&input, &paths, 5, &mut || Ok(())).unwrap();
            let data = rules::evaluate(&config, &input, &refs, &paths, &mut || Ok(())).unwrap();
            assert_eq!(data["unassigned"].as_array().unwrap().len(), 1);
            assert_eq!(data["unassigned"][0]["path"], "other/unassigned.rs");
            if paths.first().is_some_and(|p| p == "absent.rs") {
                assert!(data["instances"].as_array().unwrap().is_empty());
                assert!(data["testOnly"].as_array().unwrap().is_empty());
                continue;
            }
            let row = pair(&data, "src/store.rs", "src/destination.rs");
            assert_eq!(row["sourceLayer"], "selected");
            assert_eq!(row["status"], status);
            assert_eq!(row["referenceCount"], 1);
            let test = data["testOnly"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["target"]["path"] == "src/store.rs")
                .unwrap();
            assert_eq!(test["sourceLayer"], "selected");
            assert_eq!(test["status"], status);
            assert_eq!(test["referenceCount"], 1);
            assert_eq!(test["unresolved"], json!(["test_only"]));
        }
    }
    // A directory glob must not match a same-stem file.
    let data = evaluate(
        "version=1\n[layers.directory]\norder=1\npaths=['src/store/**']",
        &input,
    );
    assert!(
        data["unassigned"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["path"] == "src/store.rs")
    );
}

#[test]
fn rules_source_filter_keeps_full_snapshot_assignments_and_unassigned_catalog() {
    let input = snapshot(&[
        ("web/a.js", "import './b.js';"),
        ("web/b.js", "export const value = 1;"),
        ("web/unrelated.js", ""),
    ]);
    let config = rules::parse("version=1\n[layers.entry]\norder=1\npaths=[\"web/a.js\"]\n[layers.core]\norder=2\npaths=[\"web/b.js\"]\n").unwrap();
    for paths in [vec!["web/a.js".into()], vec!["absent.js".into()]] {
        let refs = references::project(&input, &paths, 1, &mut || Ok(())).unwrap();
        let data = rules::evaluate(&config, &input, &refs, &paths, &mut || Ok(())).unwrap();
        assert_eq!(data["unassigned"].as_array().unwrap().len(), 1);
        assert_eq!(data["unassigned"][0]["path"], "web/unrelated.js");
        if paths[0] == "web/a.js" {
            assert_eq!(pair(&data, "web/a.js", "web/b.js")["status"], "conforms");
        } else {
            assert!(data["instances"].as_array().unwrap().is_empty());
        }
    }
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
            "mod service; mod entry; mod peer; mod shared; fn f(){crate::service::run();crate::peer::run();crate::shared::run();}",
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
        assert_eq!(pair(&data, s, t)["status"], "deviates");
        assert_eq!(pair(&data, s, t)["unresolved"], json!([]));
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
fn rules_absent_policy_keys_default_to_forbid_with_independent_allow_overrides() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod peer; mod shared; fn run(){crate::peer::run();}",
        ),
        ("src/peer.rs", "pub fn run(){}"),
        ("src/shared.rs", "pub fn run(){crate::peer::run();}"),
    ]);
    let text = r#"version=1
[layers.entry]
order=1
paths=["src/lib.rs"]
[layers.peer]
order=1
paths=["src/peer.rs"]
[layers.shared]
shared=true
paths=["src/shared.rs"]
"#;
    for (policy, peer_status, shared_status) in [
        ("", "deviates", "deviates"),
        ("[layer_policy]", "deviates", "deviates"),
        ("[layer_policy]\npeers='allow'", "conforms", "deviates"),
        (
            "[layer_policy]\nshared_outbound='allow'",
            "deviates",
            "conforms",
        ),
    ] {
        let data = evaluate(&format!("{text}\n{policy}"), &input);
        for (source, expected) in [
            ("src/lib.rs", peer_status),
            ("src/shared.rs", shared_status),
        ] {
            let row = pair(&data, source, "src/peer.rs");
            assert_eq!(row["status"], expected, "{policy}: {source}");
            assert_eq!(row["unresolved"], json!([]));
        }
    }
}
#[test]
fn rules_layer_exceptions_are_exact_edge_pairs_and_tests_stay_separate() {
    let input = snapshot(&[
        (
            "src/lib.rs",
            "mod store; mod entry; mod other; pub fn root(){}",
        ),
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
            "{{language='{}',path='{}'}}",
            v["language"].as_str().unwrap(),
            v["path"].as_str().unwrap()
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
    assert_eq!(data["testOnly"][0]["excepted"], true);
    assert_eq!(
        data["testOnly"][0]["exceptionReason"],
        "Reviewed existing edge"
    );
    assert_eq!(data["testOnly"][0]["unresolved"], json!(["test_only"]));
    let explained = rules::explain(
        data.clone(),
        "$layers",
        target,
        Some(destination),
        &mut || Ok(()),
    )
    .unwrap();
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
    let text = "version=1\n[layers.a]\norder=1\npaths=['src/lib.rs']\n[layers.b]\norder=2\npaths=['src/lib.rs']";
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
    let config = rules::parse(text).unwrap();
    let refs = references::project(&input, &[], 5, &mut || Ok(())).unwrap();
    #[cfg(feature = "test-support")]
    let data = {
        let serial =
            rules::evaluate_with_workers(&config, &input, &refs, &[], 1, &mut || Ok(())).unwrap();
        let parallel =
            rules::evaluate_with_workers(&config, &input, &refs, &[], 4, &mut || Ok(())).unwrap();
        assert_eq!(
            serde_json::to_vec(&serial).unwrap(),
            serde_json::to_vec(&parallel).unwrap()
        );
        serial
    };
    #[cfg(not(feature = "test-support"))]
    let data = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap();
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
#[cfg(feature = "test-support")]
#[test]
fn rules_parallel_preparation_failures_are_typed_and_atomic() {
    let mut input = snapshot(&[
        ("A.java", "class A extends Base { int value; }"),
        ("B.java", "class B extends Base { int value; }"),
        ("C.java", "class C extends Base { int value; }"),
        ("Z.java", "class Z extends Base { int value; }"),
    ]);
    let refs = references::project(&input, &[], 5, &mut || Ok(())).unwrap();
    let config = rules::parse(
        r#"version=1
[roles.inherited]
select={extends="Base"}
[roles.any]
select={paths=["**"]}
[[rules]]
id="inherited-calls"
for_each="inherited"
require=[{calls="any"}]
"#,
    )
    .unwrap();
    let last = input
        .documents
        .iter()
        .position(|doc| doc.key.path == "Z.java")
        .unwrap();
    for (source, code) in [
        ("class Z extends {".to_owned(), "invalid_source_syntax"),
        (" ".repeat(2 * 1024 * 1024 + 1), "bounded_work"),
    ] {
        input.documents[last].file.text = source;
        for workers in [1, 4] {
            // Evaluation must fail rather than publish assignments for A/B/C.
            let error =
                rules::evaluate_with_workers(&config, &input, &refs, &[], workers, &mut || Ok(()))
                    .unwrap_err();
            assert!(
                error.to_string().starts_with(code),
                "workers={workers}: {error:#}"
            );
        }
    }
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

#[test]
fn rules_external_selectors_are_literal_closed_and_allow_external_only_layers() {
    let valid = r#"version=1
[external_policy]
deny=[{language="rust",package="legacy-db",stdlib=false}]
[layers.registry]
order=2
external=[{language="rust",package="serde_json",stdlib=false}]
[layers.foundation]
shared=true
paths=[]
external=[{language="rust",package="std",stdlib=true}]
"#;
    assert!(rules::parse(valid).is_ok());
    for selector in [
        r#"{language="rust",package="serde"}"#,
        r#"{language="rust",package="serde",stdlib=false,version="1"}"#,
        r#"{language="rust",package="ser*",stdlib=false}"#,
        r#"{language="rust",package="alias::item",stdlib=false}"#,
        r#"{language="python",package="Some_Package",stdlib=false}"#,
        r#"{language="javascript",package="@scope",stdlib=false}"#,
        r#"{language="java",package="artifact",stdlib=false}"#,
        r#"{language="kotlin",package="stdlib",stdlib=true}"#,
    ] {
        let text = format!("version=1\n[external_policy]\ndeny=[{selector}]");
        let d = diagnostic(&text);
        assert_eq!(d["code"], "invalid_rules", "{selector}");
        assert!(d["line"].as_u64().unwrap() >= 2, "{d}");
    }
    for text in [
        "version=1\n[layers.empty]\norder=1",
        "version=1\n[layers.empty]\norder=1\npaths=[]\nexternal=[]",
        "version=1\n[external_policy]\nallow=[]",
        "version=1\n[roles.a]\nselect={name='a'}\n[[rules]]\nid='$external'\nfor_each='a'\nrequire=[{calls='a'}]",
    ] {
        assert_eq!(diagnostic(text)["code"], "invalid_rules");
    }
}

#[test]
fn rules_external_selector_collections_use_existing_limits() {
    let selector = "{language='rust',package='serde',stdlib=false}";
    let selectors = vec![selector; 1025].join(",");
    for text in [
        format!("version=1\n[external_policy]\ndeny=[{selectors}]"),
        format!("version=1\n[layers.a]\norder=1\nexternal=[{selectors}]"),
    ] {
        assert_eq!(diagnostic(&text)["kind"], "too_large");
    }
    assert!(rules::parse("version=1\n[external_policy]\ndeny=[]").is_ok());
}

// Policy fixtures start with admitted native source/site evidence and substitute only
// the projected package target. Manifest proof is exercised by references tests.
fn projected_package(input: &Snapshot, package: &str, stdlib: bool) -> references::Projection {
    let mut refs = references::project(input, &[], 1, &mut || Ok(())).unwrap();
    let document = input
        .documents
        .iter()
        .find(|d| d.key.path == "src/lib.rs")
        .unwrap();
    let module = document
        .declarations
        .iter()
        .find(|d| d.kind == "module" && d.ancestors.is_empty())
        .unwrap();
    let source = json!({"sourceSetId":document.key.source_set_id,"language":document.key.language,
        "path":document.key.path,"sid":module.syntax_id});
    let target = json!({"kind":"externalPackage","sourceSetId":source["sourceSetId"],
        "language":"rust","package":package,"standardLibrary":stdlib});
    let site =
        json!({"range":{"start":0,"end":1},"ownerSyntaxId":null,"callId":null,"role":"production"});
    refs.data["production"] = json!([{"source":source,"target":target,"count":3,"sites":[site]}]);
    refs.data["tests"] = json!([{"source":source,"target":target,"count":2,"sites":[{
        "range":{"start":0,"end":1},"ownerSyntaxId":null,"callId":null,"role":"test"}]}]);
    refs.data["unresolved"] = json!([]);
    refs.data["externalPackages"] = json!([{"identity":target,"ecosystem":"cargo","evidence":[{
        "kind":"policyFixture","importRoot":package}]}]);
    refs
}

#[test]
fn rules_external_bans_ignore_source_assignment_and_preserve_counts_samples_and_roles() {
    let input = snapshot(&[("src/lib.rs", "pub fn run(){}")]);
    let refs = projected_package(&input, "legacy-db", false);
    let config = rules::parse(
        "version=1\n[external_policy]\ndeny=[{language='rust',package='legacy-db',stdlib=false}]",
    )
    .unwrap();
    let data = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap();
    assert_eq!(data["rulesVersion"], "rules:v2");
    assert_eq!(data["instances"].as_array().unwrap().len(), 1);
    let row = &data["instances"][0];
    assert_eq!(row["rule"], "$external");
    assert_eq!(row["status"], "deviates");
    assert_eq!(row["sourceLayer"], Value::Null);
    assert_eq!(row["referenceCount"], 3);
    assert_eq!(row["evidence"].as_array().unwrap().len(), 1);
    assert_eq!(row["excepted"], false);
    assert_eq!(data["counts"]["deviates"], 1);
    assert_eq!(data["testCounts"]["deviates"], 1);
    assert_eq!(data["testOnly"][0]["rule"], "$external");
    assert_eq!(data["testOnly"][0]["referenceCount"], 2);
    assert_eq!(data["testOnly"][0]["unresolved"], json!(["test_only"]));
    assert_eq!(data["externalPackages"], refs.data["externalPackages"]);
    assert!(
        data["unassigned"]
            .as_array()
            .unwrap()
            .contains(&row["destination"])
    );
    for text in [
        "version=1",
        "version=1\n[external_policy]\ndeny=[]",
        "version=1\n[external_policy]\ndeny=[{language='rust',package='other',stdlib=false}]",
    ] {
        let config = rules::parse(text).unwrap();
        let data = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap();
        assert!(data["instances"].as_array().unwrap().is_empty());
        assert!(data["testOnly"].as_array().unwrap().is_empty());
    }
    let config = rules::parse(
        "version=1\n[external_policy]\ndeny=[{language='rust',package='legacy-db',stdlib=false}]",
    )
    .unwrap();
    let data = rules::evaluate(
        &config,
        &input,
        &refs,
        &["absent.rs".into()],
        &mut || Ok(()),
    )
    .unwrap();
    assert!(data["instances"].as_array().unwrap().is_empty());
}

#[test]
fn rules_external_layers_and_bans_are_distinct_policies_with_closed_descriptor_explain() {
    let input = snapshot(&[("src/lib.rs", "pub fn run(){}")]);
    let refs = projected_package(&input, "legacy-db", false);
    let text = "version=1\n[external_policy]\ndeny=[{language='rust',package='legacy-db',stdlib=false}]\n[layers.source]\norder=2\npaths=['src/lib.rs']\n[layers.package]\norder=1\nexternal=[{language='rust',package='legacy-db',stdlib=false},{language='rust',package='legacy-db',stdlib=false}]";
    let config = rules::parse(text).unwrap();
    let data = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap();
    assert_eq!(data["instances"].as_array().unwrap().len(), 2);
    for row in data["instances"].as_array().unwrap() {
        assert_eq!(row["referenceCount"], 3);
        assert_eq!(row["status"], "deviates");
        assert_eq!(row["sourceLayer"], "source");
        assert_eq!(row["destinationLayer"], "package");
    }
    let row = &data["instances"][0];
    let explained = rules::explain(
        data.clone(),
        "$external",
        &row["target"],
        Some(&row["destination"]),
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(explained["instances"].as_array().unwrap().len(), 1);
    assert_eq!(explained["externalPackages"], refs.data["externalPackages"]);
    assert_eq!(explained["testOnly"].as_array().unwrap().len(), 1);
    let invalid = format!(
        "{text}\n[layers.conflict]\nshared=true\nexternal=[{{language='rust',package='legacy-db',stdlib=false}}]"
    );
    let error = match rules::parse(&invalid) {
        Err(error) => error,
        Ok(config) => rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap_err(),
    };
    assert_eq!(rules::diagnostic(&error).unwrap()["kind"], "ambiguity");
    let mut missing = refs;
    missing.data["externalPackages"] = json!([]);
    assert!(
        rules::evaluate(&config, &input, &missing, &[], &mut || Ok(()))
            .unwrap_err()
            .to_string()
            .starts_with("store_unavailable:")
    );
}

#[test]
fn rules_external_explain_propagates_package_closure_cancellation() {
    let input = snapshot(&[("src/lib.rs", "pub fn run(){}")]);
    let refs = projected_package(&input, "std", true);
    let config = rules::parse(
        "version=1\n[external_policy]\ndeny=[{language='rust',package='std',stdlib=true}]",
    )
    .unwrap();
    let data = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap();
    let row = &data["instances"][0];
    let error = rules::explain(
        data.clone(),
        "$external",
        &row["target"],
        Some(&row["destination"]),
        &mut || anyhow::bail!("cancelled"),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
}

#[test]
fn rules_external_stdlib_and_dependency_names_are_disjoint_and_unknowns_are_not_bans() {
    let input = snapshot(&[("src/lib.rs", "pub fn run(){}")]);
    let refs = projected_package(&input, "std", false);
    let config = rules::parse(
        "version=1\n[external_policy]\ndeny=[{language='rust',package='std',stdlib=true}]",
    )
    .unwrap();
    let data = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap();
    assert!(data["instances"].as_array().unwrap().is_empty());
    let refs = projected_package(&input, "std", true);
    let data = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap();
    assert_eq!(data["instances"][0]["rule"], "$external");
    let mut unknown = refs;
    unknown.data["unresolved"] = json!([{"source":unknown.data["production"][0]["source"],
        "site":{"range":{"start":0,"end":1},"ownerSyntaxId":null,"callId":null,"role":"production"},
        "reason":"module_path_not_indexed"}]);
    unknown.data["production"] = json!([]);
    unknown.data["tests"] = json!([]);
    unknown.data["externalPackages"] = json!([]);
    let data = rules::evaluate(&config, &input, &unknown, &[], &mut || Ok(())).unwrap();
    assert!(data["instances"].as_array().unwrap().is_empty());
    assert!(data["externalPackages"].as_array().unwrap().is_empty());
}

#[test]
fn rules_external_bans_have_no_exception_syntax() {
    for text in [
        "version=1\n[[exceptions]]\nrule='$external'\ntarget={source_set='set',language='rust',path='src/lib.rs',sid='sid:v1:00000000000000000000000000000000'}\nreason='no'",
        "version=1\n[[exceptions]]\nrule='$layers'\ntarget={source_set='set',language='rust',path='src/lib.rs',sid='sid:v1:00000000000000000000000000000000'}\ndestination={language='rust',package='std',stdlib=true}\nreason='no'",
    ] {
        assert_eq!(diagnostic(text)["code"], "invalid_rules");
    }
}

#[test]
fn rules_external_explain_preserves_unassigned_and_trims_only_unused_assigned_descriptors() {
    let input = snapshot(&[("src/lib.rs", "pub fn run(){}")]);
    let mut refs = projected_package(&input, "std", true);
    let mut unused = refs.data["externalPackages"][0].clone();
    unused["identity"]["package"] = json!("core");
    refs.data["externalPackages"]
        .as_array_mut()
        .unwrap()
        .push(unused);
    let config = rules::parse("version=1\n[external_policy]\ndeny=[{language='rust',package='std',stdlib=true}]\n[layers.foundation]\nshared=true\nexternal=[{language='rust',package='core',stdlib=true}]").unwrap();
    let data = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap();
    assert_eq!(data["externalPackages"].as_array().unwrap().len(), 1);
    let ban = data["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["rule"] == "$external")
        .unwrap();
    let explained = rules::explain(
        data.clone(),
        "$external",
        &ban["target"],
        Some(&ban["destination"]),
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(explained["unassigned"], data["unassigned"]);
    assert_eq!(explained["externalPackages"], data["externalPackages"]);
    for destination in [
        json!({"kind":"externalPackage","sourceSetId":"s","language":"rust","package":"std","standardLibrary":true,"extra":true}),
        json!({"kind":"externalPackage","sourceSetId":"s","language":"rust","package":"std"}),
        json!({"kind":"externalPackage","sourceSetId":"s","language":"rust","package":"*","standardLibrary":true}),
    ] {
        let error = rules::explain(
            data.clone(),
            "$external",
            &ban["target"],
            Some(&destination),
            &mut || Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().starts_with("invalid_request:"), "{error}");
    }
}

#[test]
fn rules_external_layers_keep_order_shared_peer_and_source_filter_policies() {
    let input = snapshot(&[("src/lib.rs", "pub fn run(){}")]);
    let refs = projected_package(&input, "serde", false);
    for (source_mode, destination_mode, policy, expected) in [
        ("order=1", "order=2", "", "conforms"),
        ("order=2", "order=1", "", "deviates"),
        ("order=1", "order=1", "", "deviates"),
        (
            "order=1",
            "order=1",
            "[layer_policy]\npeers='allow'",
            "conforms",
        ),
        ("order=1", "shared=true", "", "conforms"),
        ("shared=true", "order=1", "", "deviates"),
        (
            "shared=true",
            "order=1",
            "[layer_policy]\nshared_outbound='allow'",
            "conforms",
        ),
    ] {
        let config = rules::parse(&format!("version=1\n{policy}\n[layers.source]\n{source_mode}\npaths=['src/lib.rs']\n[layers.destination]\n{destination_mode}\nexternal=[{{language='rust',package='serde',stdlib=false}}]")).unwrap();
        let data = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap();
        assert_eq!(data["instances"][0]["status"], expected);
        assert_eq!(data["instances"][0]["rule"], "$layers");
        assert!(data["unassigned"].as_array().unwrap().is_empty());
        let filtered =
            rules::evaluate(
                &config,
                &input,
                &refs,
                &["absent.rs".into()],
                &mut || Ok(()),
            )
            .unwrap();
        assert!(filtered["instances"].as_array().unwrap().is_empty());
        assert!(filtered["externalPackages"].as_array().unwrap().is_empty());
    }
}

const PORTABLE_LAYERS: &str = "version=1\n[layers.high]\norder=1\npaths=['web/b.js','web/c.js']\n[layers.low]\norder=2\npaths=['web/a.js','web/other.js','web/renamed.js']\n";
const PORTABLE_PAIR: &str = "[[exceptions]]\nrule='$layers'\ntarget={language='javascript',path='web/a.js'}\ndestination={language='javascript',path='web/b.js'}\nreason='Reviewed literal pair'\n";

#[test]
fn rules_portable_exception_closed_context_grammar_and_literal_paths() {
    let portable = "{language='javascript',path='web/a.js'}";
    let full = "{source_set='set',language='javascript',path='web/a.js',sid='sid:v1:00000000000000000000000000000000'}";
    for language in ["rust", "java", "python", "javascript"] {
        assert!(rules::parse(&format!(
            "version=1\n[[exceptions]]\nrule='$layers'\ntarget={{language='{language}',path='src/a'}}\ndestination={{language='{language}',path='src/b'}}\nreason='reviewed'"
        )).is_ok());
    }
    let valid = format!("{PORTABLE_LAYERS}{PORTABLE_PAIR}");
    let named = "version=1\n[roles.source]\nselect={name='Resource'}\n[[rules]]\nid='flow'\nfor_each='source'\nrequire=[{calls='source'}]\n";
    let mut invalid = vec![
        valid.replace("reason='Reviewed literal pair'", "reason='  '"),
        format!(
            "{valid}{}",
            PORTABLE_PAIR.replace("Reviewed literal pair", "Different reason")
        ),
        valid.replace("rule='$layers'", "rule='$external'"),
        valid.replace("rule='$layers'", "rule='missing'"),
        valid.replace("destination={language='javascript',path='web/b.js'}\n", ""),
        valid.replace(
            "target={language='javascript',path='web/a.js'}",
            &format!("target={full}"),
        ),
        valid.replace(
            "destination={language='javascript',path='web/b.js'}",
            &format!("destination={full}"),
        ),
        format!(
            "version=1\n[[exceptions]]\nrule='$layers'\ntarget={full}\ndestination={full}\nreason='reviewed'"
        ),
        format!("{named}[[exceptions]]\nrule='flow'\ntarget={portable}\nreason='reviewed'"),
        format!(
            "{named}[[exceptions]]\nrule='flow'\ntarget={full}\ndestination={full}\nreason='reviewed'"
        ),
    ];
    for endpoint in [
        "{path='web/a.js'}",
        "{language='javascript'}",
        "{language='javascript',path='web/a.js',sid='sid:v1:00000000000000000000000000000000'}",
        "{language='javascript',path='web/a.js',source_set='set'}",
        "{language='javascript',path='web/a.js',kind='module'}",
        "{language='javascript',package='pkg',stdlib=false}",
        "{language='kotlin',path='web/a.js'}",
        "{language=3,path='web/a.js'}",
        "{language='javascript',path=3}",
    ] {
        invalid.push(valid.replace(portable, endpoint));
    }
    for path in [
        "",
        "*",
        "web/**",
        "web/a?.js",
        "/web/a.js",
        "web\\a.js",
        "../a.js",
        "web/./a.js",
        "web//a.js",
        "web/a:b.js",
    ] {
        for endpoint in ["web/a.js", "web/b.js"] {
            invalid.push(valid.replace(endpoint, path));
        }
    }
    for text in invalid {
        let d = diagnostic(&text);
        assert_eq!(d["code"], "invalid_rules", "{text}");
        assert_eq!(d["path"], rules::RULES_PATH);
        assert!(d["line"].as_u64().unwrap() > 0, "{d}");
        assert!(d["range"]["end"].as_u64().unwrap() <= text.len() as u64);
    }
}

#[test]
fn rules_portable_scope_uses_full_snapshot_documents_and_absent_manifests_before_filters() {
    let config = rules::parse(&format!("{PORTABLE_LAYERS}{PORTABLE_PAIR}")).unwrap();
    for manifest_only in [false, true] {
        let mut input = snapshot(&[
            ("web/a.js", "import './b.js';"),
            ("web/b.js", "export const b = 1;"),
        ]);
        let mut unrelated = snapshot(&[("unrelated.py", "class Unrelated:\n    pass\n")]);
        let second_scope = unrelated.documents[0].key.source_set_id.clone();
        assert_ne!(input.documents[0].key.source_set_id, second_scope);
        if manifest_only {
            input.manifests.push(references::external::ManifestInput {
                source_set_id: second_scope,
                native_revision_id: "retained-revision".into(),
                dependency_hash: "retained-dependency".into(),
                path: "Cargo.toml".into(),
                content_hash: None,
                bytes: None,
            });
        } else {
            input.documents.append(&mut unrelated.documents);
        }
        for paths in [vec![], vec!["web/a.js".into()], vec!["absent.js".into()]] {
            let refs = references::project(&input, &paths, 1, &mut || Ok(())).unwrap();
            let error =
                rules::evaluate(&config, &input, &refs, &paths, &mut || Ok(())).unwrap_err();
            let d = rules::diagnostic(&error).unwrap();
            assert_eq!(d["code"], "invalid_rules");
            assert_eq!(d["kind"], "ambiguity");
            assert!(
                d["line"].as_u64().unwrap() >= 8,
                "attribute scope error to exception: {d}"
            );
            assert!(d["range"]["end"].as_u64().unwrap() <= config.text.len() as u64);
        }
    }
    // Two source sets with no observed reference edges are still ambiguous.
    let mut empty = snapshot(&[("web/a.js", ""), ("web/b.js", "")]);
    empty
        .documents
        .extend(snapshot(&[("unrelated.py", "")]).documents);
    let refs = references::project(&empty, &[], 1, &mut || Ok(())).unwrap();
    assert!(refs.data["production"].as_array().unwrap().is_empty());
    let error = rules::evaluate(&config, &empty, &refs, &[], &mut || Ok(())).unwrap_err();
    assert_eq!(rules::diagnostic(&error).unwrap()["kind"], "ambiguity");
}

#[test]
fn rules_portable_pairs_are_literal_and_allow_many_languages_in_one_scope() {
    let input = snapshot(&[
        (
            "web/a.js",
            "import './b.js'; import './c.js'; import './b.js';",
        ),
        ("web/other.js", "import './b.js';"),
        ("web/b.js", "export const b = 1;"),
        ("web/c.js", "export const c = 1;"),
        ("src/lib.rs", "pub fn run() {}"),
        ("Example.java", "class Example {}"),
        ("example.py", "class Example:\n    pass\n"),
    ]);
    let scopes: std::collections::BTreeSet<_> = input
        .documents
        .iter()
        .map(|d| &d.key.source_set_id)
        .collect();
    assert_eq!(scopes.len(), 1);
    let data = evaluate(&format!("{PORTABLE_LAYERS}{PORTABLE_PAIR}"), &input);
    assert_eq!(pair(&data, "web/a.js", "web/b.js")["excepted"], true);
    assert_eq!(pair(&data, "web/a.js", "web/b.js")["referenceCount"], 2);
    for (source, destination) in [("web/a.js", "web/c.js"), ("web/other.js", "web/b.js")] {
        assert_eq!(pair(&data, source, destination)["excepted"], false);
    }
    for wrong in [
        PORTABLE_PAIR.replacen("language='javascript'", "language='rust'", 1),
        PORTABLE_PAIR.replace(
            "destination={language='javascript'",
            "destination={language='python'",
        ),
        PORTABLE_PAIR.replace("web/b.js", "web/B.js"),
        PORTABLE_PAIR.replace("web/a.js", "a.js"),
        PORTABLE_PAIR.replace("web/b.js", "missing.js"),
    ] {
        let data = evaluate(&format!("{PORTABLE_LAYERS}{wrong}"), &input);
        assert!(
            data["instances"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["excepted"] == false)
        );
    }
}

#[test]
fn rules_portable_pair_does_not_except_the_reversed_pair() {
    let input = snapshot(&[
        ("web/a.js", "import './b.js';"),
        ("web/b.js", "import './a.js';"),
    ]);
    let data = evaluate(&format!("{PORTABLE_LAYERS}{PORTABLE_PAIR}"), &input);
    assert_eq!(pair(&data, "web/a.js", "web/b.js")["excepted"], true);
    let reversed = pair(&data, "web/b.js", "web/a.js");
    assert_eq!(reversed["excepted"], false);
    assert!(reversed["exceptionReason"].is_null());
    assert!(!reversed["evidence"].as_array().unwrap().is_empty());
}

#[test]
fn rules_portable_pairs_cover_changed_native_sids_but_not_renamed_paths_or_missing_edges() {
    let mut input = snapshot(&[
        ("web/a.js", "import './b.js';"),
        ("web/b.js", "export const b = 1;"),
    ]);
    let text = format!("{PORTABLE_LAYERS}{PORTABLE_PAIR}");
    let before = evaluate(&text, &input);
    // Policy fixture: replace only a native module SID, keeping the admitted scope/path.
    let module = input
        .documents
        .iter_mut()
        .find(|d| d.key.path == "web/a.js")
        .unwrap()
        .declarations
        .iter_mut()
        .find(|d| d.kind == "module" && d.ancestors.is_empty())
        .unwrap();
    assert_ne!(module.syntax_id, "sid:v1:00000000000000000000000000000000");
    module.syntax_id = "sid:v1:00000000000000000000000000000000".into();
    let after = evaluate(&text, &input);
    assert_ne!(
        pair(&before, "web/a.js", "web/b.js")["target"]["sid"],
        pair(&after, "web/a.js", "web/b.js")["target"]["sid"]
    );
    assert_eq!(pair(&after, "web/a.js", "web/b.js")["excepted"], true);
    for files in [
        vec![
            ("web/renamed.js", "import './b.js';"),
            ("web/b.js", "export const b = 1;"),
        ],
        vec![
            ("web/a.js", "import './c.js';"),
            ("web/c.js", "export const c = 1;"),
        ],
        vec![("web/a.js", ""), ("web/b.js", "export const b = 1;")],
    ] {
        let data = evaluate(&text, &snapshot(&files));
        assert!(
            data["instances"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["excepted"] == false)
        );
        assert_eq!(data["counts"]["excepted"], 0);
    }
}

#[test]
fn rules_named_declaration_exceptions_keep_exact_full_native_identity() {
    let input = snapshot(&[("src/lib.rs", "fn Resource(){Service();} fn Service(){}")]);
    let text = "version=1\n[roles.resource]\nselect={name='Resource'}\n[roles.service]\nselect={name='Service'}\n[[rules]]\nid='flow'\nfor_each='resource'\nrequire=[{calls='service'}]\n";
    let before = evaluate(text, &input);
    let row = &before["instances"][0];
    let target = &row["target"];
    let full = format!(
        "{{source_set='{}',language='{}',path='{}',sid='{}'}}",
        target["sourceSetId"].as_str().unwrap(),
        target["language"].as_str().unwrap(),
        target["path"].as_str().unwrap(),
        target["sid"].as_str().unwrap()
    );
    let exception =
        format!("[[exceptions]]\nrule='flow'\ntarget={full}\nreason='Reviewed declaration'\n");
    let accepted = evaluate(&format!("{text}{exception}"), &input);
    let mut expected = row.clone();
    expected["excepted"] = json!(true);
    expected["exceptionReason"] = json!("Reviewed declaration");
    assert_eq!(accepted["instances"], json!([expected]));
    assert_eq!(accepted["counts"]["unknown"], 0);
    assert_eq!(accepted["counts"]["excepted"], 1);
    let explained = rules::explain(accepted.clone(), "flow", target, None, &mut || Ok(())).unwrap();
    assert_eq!(explained["instances"], accepted["instances"]);
    for changed in [
        exception.replace(
            target["sourceSetId"].as_str().unwrap(),
            "another-source-set",
        ),
        exception.replace(
            target["sid"].as_str().unwrap(),
            "sid:v1:00000000000000000000000000000000",
        ),
    ] {
        let data = evaluate(&format!("{text}{changed}"), &input);
        assert_eq!(data["instances"][0]["excepted"], false);
        assert_eq!(data["counts"]["unknown"], 1);
    }
    let mut two_scopes = input;
    two_scopes
        .documents
        .extend(snapshot(&[("other.py", "")]).documents);
    assert_eq!(
        evaluate(&format!("{text}{exception}"), &two_scopes)["counts"]["excepted"],
        1,
        "full declaration exceptions do not require portable scope resolution"
    );
}

#[test]
fn rules_portable_pairs_do_not_except_unknown_or_external_destinations() {
    let input = snapshot(&[
        ("src/lib.rs", "pub fn run(){ crate::missing::run(); }"),
        ("src/other.rs", "pub fn other(){}"),
    ]);
    let text = "version=1\n[layers.source]\norder=2\npaths=['src/lib.rs']\n[layers.destination]\norder=1\npaths=['src/other.rs']\n[[exceptions]]\nrule='$layers'\ntarget={language='rust',path='src/lib.rs'}\ndestination={language='rust',path='src/other.rs'}\nreason='Reviewed native pair'";
    let unknown = evaluate(text, &input);
    assert!(
        unknown["instances"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["status"] == "unknown" && row["destination"].is_null())
    );
    assert!(
        unknown["instances"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["excepted"] == false)
    );
    let refs = projected_package(&input, "std", true);
    let config = rules::parse(&format!("{text}\n[external_policy]\ndeny=[{{language='rust',package='std',stdlib=true}}]\n[layers.foundation]\norder=1\nexternal=[{{language='rust',package='std',stdlib=true}}]")).unwrap();
    let data = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap();
    for rule in ["$layers", "$external"] {
        let row = data["instances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["rule"] == rule && row["destination"]["kind"] == "externalPackage")
            .unwrap();
        assert_eq!(row["status"], "deviates");
        assert_eq!(row["excepted"], false);
    }
}

#[test]
fn rules_portable_scope_accepts_retained_manifest_without_source_documents() {
    let mut input = snapshot(&[("web/a.js", "")]);
    let scope = input.documents[0].key.source_set_id.clone();
    input.documents.clear();
    input.manifests.push(references::external::ManifestInput {
        source_set_id: scope,
        native_revision_id: "retained-revision".into(),
        dependency_hash: "retained-dependency".into(),
        path: "Cargo.toml".into(),
        content_hash: None,
        bytes: None,
    });
    let text = format!("{PORTABLE_LAYERS}{PORTABLE_PAIR}");
    let data = evaluate(&text, &input);
    assert!(data["instances"].as_array().unwrap().is_empty());
    assert_eq!(data["counts"]["excepted"], 0);
    input.manifests.clear();
    let refs = references::project(&input, &[], 1, &mut || Ok(())).unwrap();
    let config = rules::parse(&text).unwrap();
    let error = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap_err();
    let d = rules::diagnostic(&error).unwrap();
    assert_eq!(d["code"], "invalid_rules");
    assert_eq!(d["kind"], "reference");
}

#[test]
fn rules_portable_module_pair_does_not_require_equal_endpoint_languages() {
    let input = snapshot(&[
        ("web/a.js", "import './b.js';"),
        ("web/b.js", "export const b = 1;"),
        ("target.py", "class Target:\n    pass\n"),
    ]);
    let mut refs = references::project(&input, &[], 1, &mut || Ok(())).unwrap();
    let destination = input
        .documents
        .iter()
        .find(|d| d.key.path == "target.py")
        .unwrap();
    let module = destination
        .declarations
        .iter()
        .find(|d| d.kind == "module" && d.ancestors.is_empty())
        .unwrap();
    let native = json!({"sourceSetId":destination.key.source_set_id,"language":destination.key.language,"path":destination.key.path,"sid":module.syntax_id});
    // Exercise policy for a structural cross-language pair without claiming a resolver emits it.
    refs.data["production"][0]["target"] = native.clone();
    let text = format!(
        "{}{}",
        PORTABLE_LAYERS.replace("'web/b.js'", "'target.py'"),
        PORTABLE_PAIR.replace(
            "destination={language='javascript',path='web/b.js'}",
            "destination={language='python',path='target.py'}"
        )
    );
    let config = rules::parse(&text).unwrap();
    let data = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap();
    let row = pair(&data, "web/a.js", "target.py");
    assert_eq!(row["destination"], native);
    assert_eq!(row["excepted"], true);
    assert_eq!(row["status"], "deviates");
}

#[test]
fn rules_read_inheritance_of_an_impl_inside_a_measured_macro() {
    let input = snapshot(&[(
        "src/lib.rs",
        "trait Port {}\nstruct Resource;\nfn local() {\n    assert!({\n        impl Port for Resource {}\n        true\n    });\n}\n",
    )]);
    let text = r#"version=1
[roles.implementing]
select={implements="Port",language=["rust"]}
[roles.any]
select={paths=["**"]}
[[rules]]
id="implements-check"
for_each="implementing"
require=[{calls="any"}]
"#;
    let config = rules::parse(text).unwrap();
    let refs = references::project(&input, &[], 5, &mut || Ok(())).unwrap();
    let data = rules::evaluate(&config, &input, &refs, &[], &mut || Ok(())).unwrap();
    let rows = data["instances"].as_array().unwrap();
    assert!(
        rows.iter()
            .any(|r| r["rule"] == "implements-check" && r["target"]["path"] == "src/lib.rs"),
        "{data}"
    );
}
