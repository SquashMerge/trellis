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
        json!({"callees": ["api", "request", "api.request", "owner.request", "fetch", "axios"],
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

/// #909: a standalone `package.json` nested under a root Cargo app, in no workspace and named
/// by no manifest, is an app of its own, so its `fetch` calls are the nested app's client calls
/// of the Cargo app's routes: through the nested app's layer, or, with no layer for its files,
/// through its root, the most specific one containing them. A hidden fixture layer's Java and
/// Python files count toward the root app's files but add no language.
#[test]
fn a_nested_standalone_package_is_an_app_whose_fetch_calls_are_its_own() {
    const MOD_CLIENT: &str = "export async function poll() {\n  await fetch(\"/health\");\n  await fetch(\"/items/7\");\n}\n";
    let server_and_fixtures = "version = 1\n\
        [layers.server]\napp = 'srv'\norder = 1\npaths = ['src/**']\n\
        [layers.fixtures]\napp = 'srv'\ndisplay = 'hidden'\norder = 0\npaths = ['tests/fixtures/**']\n";
    let with_mod_layer = format!(
        "{server_and_fixtures}[layers.mod]\napp = 'mod-app'\norder = 1\npaths = ['mod/**']\n"
    );
    for (rules, client_layer) in [
        (server_and_fixtures.to_owned(), Value::Null),
        (with_mod_layer, json!("mod")),
    ] {
        let published = publish(&[
            (".trellis/rules.toml", rules.as_str()),
            ("Cargo.toml", "[package]\nname = \"srv\"\n"),
            ("src/main.rs", SERVER),
            (
                "mod/package.json",
                "{\"name\":\"mod-app\",\"private\":true}\n",
            ),
            ("mod/src/client.js", MOD_CLIENT),
            (
                "tests/fixtures/java/App.java",
                "class App { void run() {} }\n",
            ),
            ("tests/fixtures/python/app.py", "def run():\n    pass\n"),
        ]);
        let apps: Vec<(&Value, &Value)> = published.summary["apps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|app| (&app["name"], &app["manifest"]))
            .collect();
        assert_eq!(
            apps,
            [
                (&json!("srv"), &json!("Cargo.toml")),
                (&json!("mod-app"), &json!("mod/package.json")),
            ],
            "{client_layer}"
        );
        let interfaces = published.summary["interfaces"].as_array().unwrap();
        assert_eq!(interfaces.len(), 1, "{client_layer}: {interfaces:#?}");
        assert_eq!(
            (&interfaces[0]["client"], &interfaces[0]["server"]),
            (&json!("mod-app"), &json!("srv"))
        );
        let matched = details(&published, &[("list", "matched")], 100);
        let sites: Vec<(&Value, &Value)> = matched
            .iter()
            .flat_map(|item| item["entry"]["callSites"].as_array().unwrap())
            .map(|site| (&site["document"]["path"], &site["layer"]))
            .collect();
        assert_eq!(
            sites,
            [(&json!("mod/src/client.js"), &client_layer); 2],
            "{client_layer}"
        );
        let srv = &published.summary["apps"][0];
        assert_eq!(
            (&srv["languages"], &srv["fileCount"]),
            (&json!(["rust"]), &json!(3)),
            "{client_layer}"
        );
        assert_eq!(
            published.summary["apps"][1]["languages"],
            json!(["javascript"])
        );
    }
}

/// #794 (U3): a front end embedded in a Rust crate, with no manifest of its own, is one
/// detected app; an authored app at its root makes the crate's routes and its calls an
/// interface between two apps.
#[test]
fn an_authored_app_without_a_manifest_has_interfaces() {
    let files = |rules: &'static str| {
        publish(&[
            (".trellis/rules.toml", rules),
            ("Cargo.toml", "[package]\nname = \"srv\"\n"),
            ("src/main.rs", SERVER),
            ("web/app.js", CLIENT),
        ])
    };
    let authored = files(
        "version = 1\n[apps.web-app]\nroots = ['web']\n\
        [layers.server]\napp = 'srv'\norder = 1\npaths = ['src/**']\n\
        [layers.client]\napp = 'web-app'\norder = 1\npaths = ['web/**']\n",
    );
    let interfaces = authored.summary["interfaces"].as_array().unwrap();
    assert_eq!(interfaces.len(), 1, "{interfaces:#?}");
    assert_eq!(interfaces[0]["client"], "web-app");
    assert_eq!(interfaces[0]["server"], "srv");
    assert!(!details(&authored, &[], 100).is_empty());
    let names: Vec<&Value> = authored.summary["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|app| &app["name"])
        .collect();
    assert_eq!(names, [&json!("web-app"), &json!("srv")]);
    // Negative control: without the authored app the checkout is one app, with no interfaces.
    let detected = files("version = 1\n[layers.all]\norder = 1\npaths = ['**']\n");
    assert_eq!(detected.summary["interfaces"], json!([]));
    assert_eq!(detected.summary["apps"].as_array().unwrap().len(), 1);
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
        "herdr-mod/package.json",
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
    // A source keeps the Herdr mod's manifest captured, so its app (#909) is detected.
    files.push(("herdr-mod/src/stub.js", "export const a = 1;\n"));
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

/// A dispatch-table route whose answer serves a static asset is excluded like a direct
/// registration of that handler (#843 review), whether the arm delegates to a function that
/// embeds the asset or embeds it inline.
#[test]
fn a_dispatch_table_route_that_serves_an_asset_is_excluded() {
    let server = r#"
async fn core(m: Method, s: String) { answer(Core::parse(&m, &s).unwrap()).await; }
async fn asset() -> &'static str { include_str!("asset.txt") }
async fn data() {}
async fn answer(route: Core) {
    match route {
        Core::Asset => asset().await,
        Core::Inline => { include_bytes!("inline.bin"); }
        Core::Data => data().await,
    }
}
impl Core {
    fn parse(method: &Method, suffix: &str) -> Option<Self> {
        match (method, suffix) {
            (&Method::GET, "asset") => Some(Self::Asset),
            (&Method::GET, "inline") => Some(Self::Inline),
            (&Method::GET, "data") => Some(Self::Data),
            _ => None,
        }
    }
}
fn router() -> Router { Router::new().route("/api/{*rest}", get(core)) }
fn main() {}
"#;
    let published = publish(&[
        (".trellis/rules.toml", RULES),
        ("Cargo.toml", "[package]\nname = \"srv\"\n"),
        ("src/main.rs", server),
        ("web/package.json", "{\"name\":\"web-app\"}\n"),
        (
            "web/app.js",
            "function go(api) { api('/api/asset'); api('/api/inline'); api('/api/data'); }",
        ),
    ]);
    let rows = details(&published, &[], 100);
    let routes: Vec<&str> = rows
        .iter()
        .filter(|item| item["list"] != "clientOnly")
        .map(|item| item["entry"]["pathTemplate"].as_str().unwrap())
        .collect();
    assert_eq!(routes, ["/api/data"], "{rows:#?}");
}

/// #858: an actix-web server and a reqwest client, each its own Cargo app. Both one-sided
/// lists are reported against the pair, and a reqwest call never takes the wrapper prefix.
#[test]
fn an_actix_server_and_a_reqwest_client_are_an_interface() {
    let rules = "version = 1\n\
[layers.server]\napp = 'api'\norder = 1\npaths = ['server/**']\n\
[layers.client]\napp = 'cli'\norder = 1\npaths = ['tool/**']\n";
    let server = r#"
use actix_web::{get, web, App};
#[get("/health")]
async fn health() {}
async fn show() {}
async fn unused() {}
fn app() -> App {
    App::new().service(health).service(
        web::scope("/api")
            .service(web::resource("/users/{id}").route(web::get().to(show)))
            .route("/unused", web::post().to(unused)),
    )
}
fn main() {}
"#;
    let client = r#"
use reqwest::Client;
const BASE: &str = "http://localhost:8080";
pub async fn run(client: &Client, id: u32) {
    client.get(format!("{BASE}/health")).send().await;
    client.get(format!("{BASE}/api/users/{id}")).send().await;
    client.get(format!("{BASE}/api/missing")).send().await;
}
fn main() {}
"#;
    let published = publish(&[
        (".trellis/rules.toml", rules),
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"server\", \"tool\"]\n",
        ),
        ("server/Cargo.toml", "[package]\nname = \"api\"\n"),
        ("server/src/main.rs", server),
        ("tool/Cargo.toml", "[package]\nname = \"cli\"\n"),
        ("tool/src/main.rs", client),
    ]);
    let interfaces = published.summary["interfaces"].as_array().unwrap();
    assert_eq!(interfaces.len(), 1, "{interfaces:#?}");
    assert_eq!(
        (&interfaces[0]["client"], &interfaces[0]["server"]),
        (&json!("cli"), &json!("api"))
    );
    assert_eq!(
        interfaces[0]["counts"],
        json!({"matchedRoutes": 2, "callSites": 2, "serverOnly": 1, "clientOnly": 1,
            "confidence": {"exact": 1, "template": 1, "wrapperInferred": 0, "unresolved": 1}})
    );
    let matched = details(&published, &[("list", "matched")], 100);
    let users = matched
        .iter()
        .find(|item| item["entry"]["pathTemplate"] == "/api/users/{id}")
        .unwrap();
    assert_eq!(users["entry"]["confidence"], "template");
    assert_eq!(
        users["entry"]["handler"]["stableLookupKey"],
        "server/src/main.rs::show"
    );
    let site = &users["entry"]["callSites"][0];
    assert_eq!(site["document"]["language"], "rust");
    assert_eq!(site["caller"]["name"], "run");
    let server_only = details(&published, &[("list", "serverOnly")], 100);
    assert_eq!(server_only.len(), 1);
    assert_eq!(server_only[0]["entry"]["method"], "POST");
    assert_eq!(server_only[0]["entry"]["pathTemplate"], "/api/unused");
    let client_only = details(&published, &[("list", "clientOnly")], 100);
    assert_eq!(client_only.len(), 1);
    assert_eq!(client_only[0]["entry"]["pathTemplate"], "/api/missing");
    assert_eq!(client_only[0]["entry"]["confidence"], "unresolved");
}

/// #856: a Flask server and a requests/httpx client, each its own Python project. Both
/// one-sided lists are reported against the pair, and an httpx base URL path infers.
#[test]
fn a_flask_server_and_a_python_client_are_an_interface() {
    let rules = "version = 1\n\
[layers.server]\napp = 'api'\norder = 1\npaths = ['server/**']\n\
[layers.client]\napp = 'cli'\norder = 1\npaths = ['tool/**']\n";
    let server = r#"
from flask import Flask, Blueprint

app = Flask(__name__)
users = Blueprint("users", __name__, url_prefix="/api/users")

@app.route("/health")
def health():
    return "ok"

@users.route("/<int:user_id>")
def show(user_id):
    return ""

@users.route("/<int:user_id>", methods=["DELETE"])
def remove(user_id):
    return ""

app.register_blueprint(users)
"#;
    let client = r#"
import httpx
import requests

BASE = "http://localhost:8000"

def run(user_id):
    requests.get(f"{BASE}/health")
    with httpx.Client(base_url=f"{BASE}/api") as client:
        client.get(f"/users/{user_id}")
    requests.get(BASE + "/api/missing")
"#;
    let published = publish(&[
        (".trellis/rules.toml", rules),
        ("server/pyproject.toml", "[project]\nname = \"api\"\n"),
        ("server/api/app.py", server),
        ("tool/pyproject.toml", "[project]\nname = \"cli\"\n"),
        ("tool/cli/main.py", client),
    ]);
    let interfaces = published.summary["interfaces"].as_array().unwrap();
    assert_eq!(interfaces.len(), 1, "{interfaces:#?}");
    assert_eq!(
        (&interfaces[0]["client"], &interfaces[0]["server"]),
        (&json!("cli"), &json!("api"))
    );
    assert_eq!(
        interfaces[0]["counts"],
        json!({"matchedRoutes": 2, "callSites": 2, "serverOnly": 1, "clientOnly": 1,
            "confidence": {"exact": 1, "template": 0, "wrapperInferred": 1, "unresolved": 1}})
    );
    let matched = details(&published, &[("list", "matched")], 100);
    let show = matched
        .iter()
        .find(|item| item["entry"]["pathTemplate"] == "/api/users/{user_id}")
        .unwrap();
    assert_eq!(show["entry"]["method"], "GET");
    assert_eq!(show["entry"]["confidence"], "wrapperInferred");
    assert_eq!(show["entry"]["handler"]["kind"], "declaration");
    let site = &show["entry"]["callSites"][0];
    assert_eq!(site["document"]["language"], "python");
    assert_eq!(site["caller"]["name"], "run");
    let server_only = details(&published, &[("list", "serverOnly")], 100);
    assert_eq!(server_only.len(), 1);
    assert_eq!(server_only[0]["entry"]["method"], "DELETE");
    assert_eq!(
        server_only[0]["entry"]["pathTemplate"],
        "/api/users/{user_id}"
    );
    let client_only = details(&published, &[("list", "clientOnly")], 100);
    assert_eq!(client_only.len(), 1);
    assert_eq!(client_only[0]["entry"]["pathTemplate"], "/api/missing");
    assert_eq!(client_only[0]["entry"]["confidence"], "unresolved");
}

/// #857: a Spring server and a Java client, each an authored app. Both one-sided lists are
/// reported against the pair, and a `WebClient` under a readable base URL is
/// `wrapperInferred`.
#[test]
fn a_spring_server_and_a_java_client_are_an_interface() {
    let rules = "version = 1\n\
[apps.users]\nroots = ['api']\n\
[apps.portal]\nroots = ['portal']\n\
[layers.server]\napp = 'users'\norder = 1\npaths = ['api/**']\n\
[layers.client]\napp = 'portal'\norder = 1\npaths = ['portal/**']\n";
    let server = r#"package com.example.users;

import org.springframework.web.bind.annotation.*;

@RestController
@RequestMapping("/api/users")
public class UserController {
    @GetMapping("/{id}")
    public String show(@PathVariable long id) { return ""; }

    @GetMapping("/health")
    public String health() { return "ok"; }

    @DeleteMapping("/{id}")
    public void remove(@PathVariable long id) {}
}
"#;
    let client = r#"package com.example.portal;

import org.springframework.web.client.RestTemplate;
import org.springframework.web.reactive.function.client.WebClient;

public class UsersGateway {
    private static final String HOST = "http://users:8080";
    private final RestTemplate rest = new RestTemplate();
    private final WebClient users = WebClient.builder().baseUrl(HOST + "/api/users").build();

    public void load(long id) {
        rest.getForObject(HOST + "/api/users/{id}", String.class, id);
        users.get().uri("/health").retrieve();
        rest.getForObject(HOST + "/api/orders", String.class);
    }
}
"#;
    let published = publish(&[
        (".trellis/rules.toml", rules),
        (
            "api/src/main/java/com/example/users/UserController.java",
            server,
        ),
        (
            "portal/src/main/java/com/example/portal/UsersGateway.java",
            client,
        ),
    ]);
    let interfaces = published.summary["interfaces"].as_array().unwrap();
    assert_eq!(interfaces.len(), 1, "{interfaces:#?}");
    assert_eq!(
        (&interfaces[0]["client"], &interfaces[0]["server"]),
        (&json!("portal"), &json!("users"))
    );
    assert_eq!(
        interfaces[0]["counts"],
        json!({"matchedRoutes": 2, "callSites": 2, "serverOnly": 1, "clientOnly": 1,
            "confidence": {"exact": 0, "template": 1, "wrapperInferred": 1, "unresolved": 1}})
    );
    let matched = details(&published, &[("list", "matched")], 100);
    let show = matched
        .iter()
        .find(|item| item["entry"]["pathTemplate"] == "/api/users/{id}")
        .unwrap();
    assert_eq!(show["entry"]["confidence"], "template");
    assert_eq!(
        show["entry"]["handler"]["stableLookupKey"],
        "api/src/main/java/com/example/users/UserController.java::UserController::show"
    );
    let site = &show["entry"]["callSites"][0];
    assert_eq!(site["document"]["language"], "java");
    assert_eq!(site["caller"]["name"], "load");
    let health = matched
        .iter()
        .find(|item| item["entry"]["pathTemplate"] == "/api/users/health")
        .unwrap();
    assert_eq!(health["entry"]["confidence"], "wrapperInferred");
    let server_only = details(&published, &[("list", "serverOnly")], 100);
    assert_eq!(server_only.len(), 1);
    assert_eq!(server_only[0]["entry"]["method"], "DELETE");
    assert_eq!(server_only[0]["entry"]["pathTemplate"], "/api/users/{id}");
    let client_only = details(&published, &[("list", "clientOnly")], 100);
    assert_eq!(client_only.len(), 1);
    assert_eq!(client_only[0]["entry"]["pathTemplate"], "/api/orders");
    assert_eq!(client_only[0]["entry"]["confidence"], "unresolved");
}

// #855: JavaScript and TypeScript servers and clients.

const JS_RULES: &str = "version = 1\n\
[layers.api]\napp = 'api'\norder = 1\npaths = ['api/**']\n\
[layers.web]\napp = 'web'\norder = 1\npaths = ['web/**']\n\
[layers.site]\napp = 'site'\norder = 1\npaths = ['site/**']\n";

const JS_API: &str = r#"
const express = require("express");
const app = express();
const users = express.Router();
function listUsers(req, res) {}
function showUser(req, res) {}
users.get("/", listUsers);
users.get("/:id", showUser);
app.use("/api/users", users);
app.post("/api/audit", (req, res) => {});
module.exports = app;
"#;

const JS_SITE_ROUTE: &str = "export async function GET(request) { return Response.json([]); }\n";

const JS_WEB: &str = r#"
import axios from "axios";
const http = axios.create({ baseURL: "/api" });
export async function load(id) {
  await fetch("/api/users");
  await http.get(`/users/${id}`);
  await fetch("/products/7");
  await axios.delete("/api/users/7");
}
"#;

fn js_apps(api: &str) -> Published {
    publish(&[
        (".trellis/rules.toml", JS_RULES),
        (
            "api/package.json",
            "{\"name\":\"api\",\"dependencies\":{\"express\":\"4.21.0\"}}\n",
        ),
        ("api/server.js", api),
        (
            "site/package.json",
            "{\"name\":\"site\",\"dependencies\":{\"next\":\"15.0.0\"}}\n",
        ),
        ("site/app/products/[id]/route.js", JS_SITE_ROUTE),
        (
            "web/package.json",
            "{\"name\":\"web\",\"dependencies\":{\"axios\":\"1.7.0\"}}\n",
        ),
        ("web/client.js", JS_WEB),
    ])
}

/// A browser client reaches an express server and a Next.js app: one interface per pair,
/// each with its own confidence counts, and both one-sided lists.
#[test]
fn a_javascript_client_reaches_express_and_next_servers() {
    let published = js_apps(JS_API);
    let interfaces = published.summary["interfaces"].as_array().unwrap();
    let pairs: Vec<(&str, &str)> = interfaces
        .iter()
        .map(|i| (i["client"].as_str().unwrap(), i["server"].as_str().unwrap()))
        .collect();
    assert_eq!(pairs, [("web", "api"), ("web", "site")], "{interfaces:#?}");
    let api = &interfaces[0];
    assert_eq!(
        api["counts"],
        json!({"matchedRoutes": 2, "callSites": 2, "serverOnly": 1, "clientOnly": 1,
            "confidence": {"exact": 1, "template": 0, "wrapperInferred": 1, "unresolved": 1}})
    );
    assert!(
        api["wrappers"]["callees"]
            .as_array()
            .unwrap()
            .contains(&json!("axios"))
    );
    let site = &interfaces[1];
    assert_eq!(site["counts"]["matchedRoutes"], 1);
    assert_eq!(site["counts"]["confidence"]["template"], 1);

    let matched = details(
        &published,
        &[("client", "web"), ("server", "api"), ("list", "matched")],
        10,
    );
    let entries: Vec<(&str, &str, &str, &str)> = matched
        .iter()
        .map(|item| {
            let entry = &item["entry"];
            (
                entry["method"].as_str().unwrap(),
                entry["pathTemplate"].as_str().unwrap(),
                entry["confidence"].as_str().unwrap(),
                entry["handler"]["stableLookupKey"].as_str().unwrap_or("-"),
            )
        })
        .collect();
    assert_eq!(
        entries,
        [
            ("GET", "/api/users", "exact", "api/server.js::listUsers"),
            (
                "GET",
                "/api/users/{id}",
                "wrapperInferred",
                "api/server.js::showUser"
            ),
        ]
    );
    // One-sided, both ways: the server's unmatched route, and the client's call with no route.
    let server_only = details(
        &published,
        &[("client", "web"), ("server", "api"), ("list", "serverOnly")],
        10,
    );
    assert_eq!(server_only.len(), 1);
    assert_eq!(server_only[0]["entry"]["method"], "POST");
    assert_eq!(server_only[0]["entry"]["pathTemplate"], "/api/audit");
    let client_only = details(
        &published,
        &[("client", "web"), ("server", "api"), ("list", "clientOnly")],
        10,
    );
    assert_eq!(client_only.len(), 1);
    assert_eq!(client_only[0]["entry"]["method"], "DELETE");
    assert_eq!(client_only[0]["entry"]["pathTemplate"], "/api/users/7");
    assert_eq!(client_only[0]["entry"]["confidence"], "unresolved");
    let site_matched = details(
        &published,
        &[("client", "web"), ("server", "site"), ("list", "matched")],
        10,
    );
    assert_eq!(site_matched[0]["entry"]["pathTemplate"], "/products/{id}");
    assert_eq!(site_matched[0]["entry"]["handler"]["kind"], "declaration");
}

/// Negative control: the same server source without the express import registers nothing, so
/// the web app's only interface is with the Next.js app.
#[test]
fn a_server_without_a_framework_import_has_no_routes() {
    let published = js_apps(&JS_API.replace("require(\"express\")", "require(\"./local\")"));
    let interfaces = published.summary["interfaces"].as_array().unwrap();
    let pairs: Vec<(&str, &str)> = interfaces
        .iter()
        .map(|i| (i["client"].as_str().unwrap(), i["server"].as_str().unwrap()))
        .collect();
    assert_eq!(pairs, [("web", "site")], "{interfaces:#?}");
}

/// #855 on Trellis itself: with `fetch`, axios and the JavaScript frameworks read, every
/// JavaScript and TypeScript source under `web/` and `herdr-mod/` and every build manifest the
/// capture walk admits give one interface per client app: `trellis-herdr-mod` (#909) and
/// `trellis-web`, each to `trellis`.
#[test]
fn trellis_web_and_herdr_mod_clients_each_have_one_interface() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut owned: Vec<(String, String)> = Vec::new();
    let walk = ignore::WalkBuilder::new(root)
        .require_git(false)
        .follow_links(false)
        .hidden(true)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git" | "node_modules" | ".venv" | "target" | "dist" | "build")
            )
        })
        .build();
    for entry in walk {
        let entry = entry.unwrap();
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = entry.path().strip_prefix(root).unwrap().to_str().unwrap();
        let name = entry.file_name().to_str().unwrap_or_default();
        let source = (relative.starts_with("web/") || relative.starts_with("herdr-mod/"))
            && [".js", ".mjs", ".cjs", ".ts"]
                .iter()
                .any(|ext| name.ends_with(ext));
        let manifest = matches!(name, "Cargo.toml" | "package.json" | "pyproject.toml");
        if source || manifest {
            owned.push((
                relative.to_owned(),
                fs::read_to_string(entry.path()).unwrap(),
            ));
        }
    }
    let stubs: Vec<(String, String)> = owned
        .iter()
        .filter_map(|(path, text)| {
            if let Some(dir) = path.strip_suffix("pyproject.toml") {
                return Some((format!("{dir}stub.py"), "A = 1\n".to_owned()));
            }
            let dir = path.strip_suffix("Cargo.toml")?;
            text.contains("[package]")
                .then(|| (format!("{dir}src/main.rs"), "fn main() {}\n".to_owned()))
        })
        .collect();
    owned.extend(stubs);
    for path in [".trellis/rules.toml", "src/http.rs"] {
        owned.push((
            path.to_owned(),
            fs::read_to_string(root.join(path)).unwrap(),
        ));
    }
    owned.push((
        "src/bin/trellis-producer-stub.rs".into(),
        "fn main() {}\n".into(),
    ));
    owned.sort();
    owned.dedup_by(|a, b| a.0 == b.0);
    let files: Vec<(&str, &str)> = owned
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let published = publish(&files);
    let interfaces = published.summary["interfaces"].as_array().unwrap();
    let pairs: Vec<(&str, &str)> = interfaces
        .iter()
        .map(|i| (i["client"].as_str().unwrap(), i["server"].as_str().unwrap()))
        .collect();
    assert_eq!(
        pairs,
        [("trellis-herdr-mod", "trellis"), ("trellis-web", "trellis")],
        "{interfaces:#?}"
    );
    // The `api` wrapper's own `fetch` is its implementation, not a call of its own.
    let client_only = details(&published, &[("list", "clientOnly")], 200);
    assert!(!client_only.is_empty());
    assert!(
        client_only
            .iter()
            .all(|item| item["entry"]["callSite"]["caller"]["name"] != "api"),
        "{client_only:#?}"
    );
}

/// #855 (owner rulings 2026-11-10): a wrapper authored in `.trellis/rules.toml`, scoped to a
/// root or to an app, reaches the published interfaces and is listed in `wrappers.callees`; an
/// invalid one is a rules error.
#[test]
fn a_rules_file_wrapper_reaches_the_published_interfaces() {
    let layers = "version = 1\n\
        [layers.api]\napp = 'api'\norder = 1\npaths = ['api/**']\n\
        [layers.web]\napp = 'web'\norder = 1\npaths = ['web/**']\n";
    for scope in ["root = \"web\"", "app = \"web\""] {
        let rules = format!(
            "{layers}[[interfaces.wrappers]]\ncallee = \"apiFetch\"\nprefix = \"/api\"\n{scope}\n"
        );
        let published = publish(&[
            (".trellis/rules.toml", rules.as_str()),
            (
                "api/package.json",
                "{\"name\":\"api\",\"dependencies\":{\"express\":\"4.21.0\"}}\n",
            ),
            ("api/server.js", JS_API),
            ("web/package.json", "{\"name\":\"web\"}\n"),
            (
                "web/client.js",
                "export const go = () => apiFetch(\"/users\");\n",
            ),
        ]);
        let interfaces = published.summary["interfaces"].as_array().unwrap();
        assert_eq!(interfaces.len(), 1, "{interfaces:#?}");
        assert_eq!(interfaces[0]["counts"]["confidence"]["wrapperInferred"], 1);
        assert!(
            interfaces[0]["wrappers"]["callees"]
                .as_array()
                .unwrap()
                .contains(&json!("apiFetch"))
        );
    }
    let error = trellis::rules::parse(&format!(
        "{JS_RULES}[[interfaces.wrappers]]\ncallee = \"a.b.c\"\n"
    ))
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("wrapper callee"), "{error:#}");
}
