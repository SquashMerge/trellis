#![cfg(feature = "test-support")]
//! HTTP interface evidence (#788) through the published store: the summary's `interfaces` and
//! the `kind=interfaces` detail pages.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    architecture_details::Query,
    indexer::{IndexOptions, index_workspace_bundle},
    model::CancelFlag,
    store::{MetricsOptions, Store},
};

struct Published {
    _state: tempfile::TempDir,
    _root: tempfile::TempDir,
    store: Store,
    summary: Value,
}

fn publish(files: &[(&str, &str)]) -> Published {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let store =
        trellis::testing::retry_admission(|| Store::open_for_tests(state.path(), root.path()));
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().into()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let pin = store
        .publish_native(&graph, &capture, &native, &leader, baseline, &cancel)
        .unwrap();
    drop(leader);
    let response = store.evidence_response().unwrap();
    let summary = response
        .architecture_view(
            &MetricsOptions {
                pin: Some(pin),
                aggregate: "max".into(),
                ..Default::default()
            },
            &mut || Ok(()),
        )
        .unwrap();
    let summary = response.finish(summary).unwrap();
    Published {
        _state: state,
        _root: root,
        store,
        summary,
    }
}

fn query(
    summary: &Value,
    fields: &[(&str, &str)],
) -> Result<Query, trellis::architecture_details::InvalidQuery> {
    let mut all: BTreeMap<String, String> = [
        ("kind", "interfaces".to_owned()),
        (
            "indexGeneration",
            summary["indexGeneration"].as_str().unwrap().to_owned(),
        ),
        ("indexRevision", summary["indexRevision"].to_string()),
        (
            "rulesHash",
            summary["rulesFile"]["contentHash"]
                .as_str()
                .unwrap()
                .to_owned(),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    all.extend(
        fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    Query::from_fields(all)
}

/// Every item of one filtered read, following `nextOffset` with `limit` per page.
fn details(published: &Published, filters: &[(&str, &str)], limit: usize) -> Vec<Value> {
    let (mut offset, mut items) = (0usize, Vec::new());
    loop {
        let (offset_text, limit_text) = (offset.to_string(), limit.to_string());
        let mut fields = vec![
            ("offset", offset_text.as_str()),
            ("limit", limit_text.as_str()),
        ];
        fields.extend_from_slice(filters);
        let query = query(&published.summary, &fields).unwrap();
        let response = published.store.evidence_response().unwrap();
        let page = response
            .architecture_details(&query, &mut || Ok(()))
            .unwrap();
        response.finish(()).unwrap();
        assert_eq!(page["kind"], "interfaces");
        let rows = page["items"].as_array().unwrap();
        assert!(rows.len() <= limit);
        items.extend(rows.iter().cloned());
        match page["nextOffset"].as_u64() {
            Some(next) => offset = next as usize,
            None => {
                assert_eq!(page["total"], items.len());
                return items;
            }
        }
    }
}

const RULES: &str = "version = 1\n\
[layers.server]\napp = 'srv'\norder = 1\npaths = ['src/**']\n\
[layers.client]\napp = 'web-app'\norder = 1\npaths = ['web/app.js']\n";

const SERVER: &str = r#"
async fn health() {}
async fn item() {}
async fn unused() {}
fn router() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/items/{id}", get(item))
        .route("/unused", post(unused))
}
fn main() {}
"#;

const CLIENT: &str = r#"
async function load(api, id) {
  await api("/health");
  await api(`/items/${id}`);
  await api("/missing");
}
async function again(api) {
  await api("/health");
}
"#;

const LOOSE: &str = "async function loose(api) {\n  await api(\"/health\");\n}\n";

fn two_apps() -> Published {
    publish(&[
        (".trellis/rules.toml", RULES),
        ("Cargo.toml", "[package]\nname = \"srv\"\n"),
        ("src/main.rs", SERVER),
        ("web/package.json", "{\"name\":\"web-app\"}\n"),
        ("web/app.js", CLIENT),
        ("web/loose.js", LOOSE),
    ])
}

/// AC18: the DTO and the layer that owns each end.
#[test]
fn dto_shape_and_end_layers() {
    let published = two_apps();
    let interfaces = published.summary["interfaces"].as_array().unwrap();
    assert_eq!(interfaces.len(), 1, "{interfaces:#?}");
    let interface = &interfaces[0];
    assert_eq!(
        (
            &interface["client"],
            &interface["server"],
            &interface["kind"]
        ),
        (&json!("web-app"), &json!("srv"), &json!("http"))
    );
    assert_eq!(
        interface["ends"],
        json!([
            {"clientLayer": "client", "serverLayer": "server", "routes": 2, "callSites": 3},
            {"clientLayer": null, "serverLayer": "server", "routes": 1, "callSites": 1},
        ])
    );
    assert_eq!(
        interface["wrappers"],
        json!({"callees": ["api", "request", "api.request", "owner.request"],
            "prefix": {"client": "/api/", "server": "/api/checkouts/{root}/"}})
    );
    assert_eq!(
        interface["counts"],
        json!({"matchedRoutes": 2, "callSites": 4, "serverOnly": 1, "clientOnly": 1,
            "confidence": {"exact": 3, "template": 1, "wrapperInferred": 0, "unresolved": 1}})
    );

    let matched = details(&published, &[("list", "matched")], 100);
    let health = matched
        .iter()
        .find(|item| item["entry"]["pathTemplate"] == "/health")
        .unwrap();
    assert_eq!(health["client"], "web-app");
    assert_eq!(health["server"], "srv");
    assert_eq!(health["list"], "matched");
    let entry = &health["entry"];
    assert_eq!(entry["method"], "GET");
    assert_eq!(entry["confidence"], "exact");
    let handler = &entry["handler"];
    assert_eq!(handler["kind"], "declaration");
    assert_eq!(handler["path"], "src/main.rs");
    assert_eq!(handler["layer"], "server");
    assert_eq!(handler["lines"], json!({"start": 2, "end": 2}));
    assert_eq!(handler["variant"], Value::Null);
    assert!(handler["syntaxId"].is_string());
    assert_eq!(handler["stableLookupKey"], "src/main.rs::health");
    let sites = entry["callSites"].as_array().unwrap();
    assert_eq!(sites.len(), 3);
    let layers: Vec<_> = sites.iter().map(|site| site["layer"].clone()).collect();
    assert_eq!(layers, [json!("client"), json!("client"), Value::Null]);
    let site = &sites[0];
    assert_eq!(site["document"]["path"], "web/app.js");
    assert_eq!(site["document"]["language"], "javascript");
    assert!(site["document"]["sourceSetId"].is_string());
    assert!(site["range"]["start"].as_u64() < site["range"]["end"].as_u64());
    assert_eq!(site["lines"], json!({"start": 3, "end": 3}));
    assert_eq!(site["confidence"], "exact");
    assert!(site["callId"].is_string());
    assert_eq!(site["caller"]["name"], "load");
    assert_eq!(site["caller"]["document"]["path"], "web/app.js");
    let item = matched
        .iter()
        .find(|item| item["entry"]["pathTemplate"] == "/items/{id}")
        .unwrap();
    assert_eq!(item["entry"]["confidence"], "template");
    assert_eq!(item["entry"]["callSites"][0]["confidence"], "template");
}

/// AC19 and AC21: totals equal `counts`, the filters select, and both unmatched sides are
/// reported against the pair.
#[test]
fn interfaces_summary_counts_and_detail_pages() {
    let published = two_apps();
    let counts = published.summary["interfaces"][0]["counts"].clone();
    let all = details(&published, &[], 100);
    let matched = details(&published, &[("list", "matched")], 100);
    let server_only = details(&published, &[("list", "serverOnly")], 100);
    let client_only = details(&published, &[("list", "clientOnly")], 100);
    assert_eq!(matched.len(), counts["matchedRoutes"]);
    assert_eq!(server_only.len(), counts["serverOnly"]);
    assert_eq!(client_only.len(), counts["clientOnly"]);
    let sites: usize = matched
        .iter()
        .map(|item| item["entry"]["callSites"].as_array().unwrap().len())
        .sum();
    assert_eq!(sites, counts["callSites"]);
    let expected: Vec<Value> = [matched, server_only.clone(), client_only.clone()].concat();
    assert_eq!(all, expected, "the lists concatenate in order");

    assert_eq!(
        server_only[0],
        json!({"client": "web-app", "server": "srv", "list": "serverOnly",
            "entry": {"method": "POST", "pathTemplate": "/unused",
                "handler": server_only[0]["entry"]["handler"]}})
    );
    assert_eq!(server_only[0]["entry"]["handler"]["layer"], "server");
    let entry = &client_only[0]["entry"];
    assert_eq!(entry["pathTemplate"], "/missing");
    assert_eq!(entry["confidence"], "unresolved");
    assert_eq!(entry["callSite"]["confidence"], "unresolved");
    assert_eq!(entry["callSite"]["layer"], "client");
    assert_eq!(entry["callSite"]["caller"]["name"], "load");

    let pair = [("client", "web-app"), ("server", "srv")];
    assert_eq!(details(&published, &pair, 100), all);
    for other in [
        [("client", "srv"), ("server", "web-app")],
        [("client", "web-app"), ("server", "nobody")],
    ] {
        assert!(details(&published, &other, 100).is_empty());
    }

    let paged = details(&published, &[], 1);
    assert_eq!(paged, all, "one-item pages concatenate to the whole");

    for bad in [
        vec![("confidence", "exact")],
        vec![("list", "unmatched")],
        vec![("list", "")],
        vec![("path", "src/main.rs")],
    ] {
        assert!(query(&published.summary, &bad).is_err(), "{bad:?}");
    }
    let stale = query(&published.summary, &[("rulesHash", "other")]).unwrap();
    let response = published.store.evidence_response().unwrap();
    let error = response
        .architecture_details(&stale, &mut || Ok(()))
        .unwrap_err();
    assert!(error.to_string().starts_with("rules_conflict"), "{error:#}");
}

#[test]
fn server_only_routes_are_listed_per_client_and_a_server_is_not_a_client() {
    let both = publish(&[
        (".trellis/rules.toml", RULES),
        ("Cargo.toml", "[package]\nname = \"srv\"\n"),
        ("src/main.rs", SERVER),
        ("web/package.json", "{\"name\":\"web-app\"}\n"),
        (
            "web/app.js",
            "async function load(api) { await api(\"/health\"); }\nfunction ping() {}\n",
        ),
    ]);
    assert_eq!(both.summary["interfaces"].as_array().unwrap().len(), 1);
    let rows = details(&both, &[("client", "web-app"), ("list", "serverOnly")], 100);
    let routes: Vec<_> = rows
        .iter()
        .map(|row| row["entry"]["pathTemplate"].as_str().unwrap())
        .collect();
    assert_eq!(routes, ["/items/{id}", "/unused"]);
    assert!(details(&both, &[("client", "srv")], 100).is_empty());
}

#[test]
fn one_app_and_no_calls_have_no_interfaces() {
    let single = publish(&[
        (
            ".trellis/rules.toml",
            "version = 1\n[layers.all]\norder = 1\npaths = ['**']\n",
        ),
        ("Cargo.toml", "[package]\nname = \"srv\"\n"),
        ("src/main.rs", SERVER),
    ]);
    assert_eq!(single.summary["interfaces"], json!([]));
    assert!(details(&single, &[], 100).is_empty());

    let unmatched = publish(&[
        (".trellis/rules.toml", RULES),
        ("Cargo.toml", "[package]\nname = \"srv\"\n"),
        ("src/main.rs", SERVER),
        ("web/package.json", "{\"name\":\"web-app\"}\n"),
        (
            "web/app.js",
            "async function load(api) { await api(\"/missing\"); }\n",
        ),
    ]);
    assert_eq!(unmatched.summary["interfaces"], json!([]));
}

/// AC16 and AC21 on Trellis's own router and client. Calls and routes are located by
/// content, never by line or by total.
#[test]
fn trellis_real_router_and_client() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let read = |path: &str| fs::read_to_string(format!("{manifest}/{path}")).unwrap();
    let texts: Vec<(String, String)> = [
        ".trellis/rules.toml",
        "Cargo.toml",
        "web/package.json",
        "src/http.rs",
        "web/app.js",
        "web/architecture.js",
        "web/classes.js",
        "web/navigation.js",
        "tests/daemon/cli.rs",
    ]
    .into_iter()
    .map(|path| (path.to_owned(), read(path)))
    .collect();
    let mut files: Vec<(&str, &str)> = texts
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    files.push(("src/main.rs", "fn main() {}\n"));
    let published = publish(&files);
    let interfaces = published.summary["interfaces"].as_array().unwrap();
    assert_eq!(interfaces.len(), 1, "{interfaces:#?}");
    assert_eq!(interfaces[0]["client"], "trellis-web");
    assert_eq!(interfaces[0]["server"], "trellis");

    let matched = details(&published, &[("list", "matched")], 100);
    let route = |template: &str| {
        matched
            .iter()
            .find(|item| item["entry"]["pathTemplate"] == template)
            .unwrap_or_else(|| panic!("no matched route {template}"))["entry"]
            .clone()
    };
    let status = route("/api/checkouts/{root_key}/status");
    assert_eq!(status["method"], "GET");
    assert_eq!(status["handler"]["kind"], "dispatchArm");
    assert_eq!(status["handler"]["variant"], "Status");
    assert_eq!(status["handler"]["path"], "src/http.rs");
    assert_eq!(status["handler"]["layer"], "entry");
    let through_prefix: Vec<&Value> = status["callSites"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|site| site["document"]["path"] == "web/app.js")
        .collect();
    assert!(!through_prefix.is_empty());
    assert!(through_prefix.iter().all(|site| site["layer"] == "web"));
    assert_eq!(status["confidence"], "wrapperInferred");
    let checkouts = route("/api/checkouts");
    assert_eq!(checkouts["confidence"], "exact");
    assert_eq!(checkouts["handler"]["kind"], "declaration");
    assert_eq!(
        checkouts["handler"]["stableLookupKey"],
        "src/http.rs::provisioned_checkouts"
    );

    let server_only = details(&published, &[("list", "serverOnly")], 100);
    let client_only = details(&published, &[("list", "clientOnly")], 100);
    assert!(!server_only.is_empty() && !client_only.is_empty());
    let everything = [matched.as_slice(), &server_only, &client_only].concat();
    for item in &everything {
        let entry = &item["entry"];
        assert_ne!(entry["pathTemplate"], "/api/status", "test-support route");
        assert_ne!(entry["pathTemplate"], "/app.js", "static asset");
        assert_ne!(entry["handler"]["path"], "tests/daemon/cli.rs");
        assert_ne!(entry["callSite"]["document"]["path"], "tests/daemon/cli.rs");
    }
    assert_eq!(
        interfaces[0]["counts"]["serverOnly"],
        server_only.len(),
        "summary counts equal the pages"
    );
}

/// A valid retained nested manifest of 5.1 MB is within the budget rules evaluation sizes
/// from the admitted snapshot; app detection for interfaces uses the same budget, so its
/// interface evidence is published (#843 review).
#[test]
fn a_large_nested_manifest_keeps_its_interfaces() {
    let manifest = format!(
        "{{\"name\":\"web-app\",\"padding\":\"{}\"}}",
        "x".repeat(5_100_000)
    );
    let published = publish(&[
        (".trellis/rules.toml", RULES),
        ("Cargo.toml", "[package]\nname = \"srv\"\n"),
        ("src/main.rs", SERVER),
        ("web/package.json", &manifest),
        ("web/app.js", "function go(api) { api('/health'); }"),
    ]);
    assert_eq!(published.summary["state"], "ready");
    assert_eq!(published.summary["apps"].as_array().unwrap().len(), 2);
    let interfaces = published.summary["interfaces"].as_array().unwrap();
    assert_eq!(interfaces.len(), 1, "{interfaces:#?}");
    assert_eq!(interfaces[0]["counts"]["callSites"], 1);
}

/// `//health` keeps its empty leading segment, so it is not the literal route `/health`
/// (#843 review): it matches nothing and is `unresolved`.
#[test]
fn a_repeated_leading_slash_is_not_the_single_slash_route() {
    let published = publish(&[
        (".trellis/rules.toml", RULES),
        ("Cargo.toml", "[package]\nname = \"srv\"\n"),
        ("src/main.rs", SERVER),
        ("web/package.json", "{\"name\":\"web-app\"}\n"),
        (
            "web/app.js",
            "function go(api) { api('/health'); api('//health'); }",
        ),
    ]);
    assert_eq!(published.summary["interfaces"][0]["counts"]["callSites"], 1);
    let matched = details(&published, &[("list", "matched")], 100);
    let health = matched
        .iter()
        .find(|item| item["entry"]["pathTemplate"] == "/health")
        .unwrap();
    assert_eq!(health["entry"]["confidence"], "exact");
    assert_eq!(health["entry"]["callSites"].as_array().unwrap().len(), 1);
    let client_only = details(&published, &[("list", "clientOnly")], 100);
    let double: Vec<&Value> = client_only
        .iter()
        .filter(|item| item["entry"]["pathTemplate"] == "//health")
        .collect();
    assert_eq!(double.len(), 1, "{client_only:#?}");
    assert_eq!(double[0]["entry"]["confidence"], "unresolved");
}

/// A `method` accessor in the options object is a present, non-literal method: unreadable,
/// so never matched and `unresolved`, not a default `GET` (#843 review).
#[test]
fn a_method_accessor_is_unreadable_not_default_get() {
    let published = publish(&[
        (".trellis/rules.toml", RULES),
        ("Cargo.toml", "[package]\nname = \"srv\"\n"),
        ("src/main.rs", SERVER),
        ("web/package.json", "{\"name\":\"web-app\"}\n"),
        (
            "web/app.js",
            "function go(api) { api('/health'); \
             api.request('/health', { get method() { return 'POST'; } }); }",
        ),
    ]);
    assert_eq!(published.summary["interfaces"][0]["counts"]["callSites"], 1);
    let client_only = details(&published, &[("list", "clientOnly")], 100);
    assert_eq!(client_only.len(), 1, "{client_only:#?}");
    assert_eq!(client_only[0]["entry"]["method"], Value::Null);
    assert_eq!(client_only[0]["entry"]["pathTemplate"], "/health");
    assert_eq!(client_only[0]["entry"]["confidence"], "unresolved");
}
