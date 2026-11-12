#![cfg(feature = "test-support")]
//! #851: apps in `trellis_workspace_describe`, the `app` filter, and `trellis_endpoints`,
//! through the daemon-socket MCP session, each answer validated against its catalog schema.
use crate::mcp_protocol::{
    architecture_built, modern, next_reply, published_inactive_checkout, socket_session, tool,
    validate_structured_content,
};
use serde_json::{Value, json};
use std::{fs, io::Write};
use trellis::daemon::registry::CheckoutRegistry;

const SERVER: &str = "async fn health() {}\n\
async fn item() {}\n\
async fn orphan() {}\n\
fn router() -> Router {\n    Router::new()\n        .route(\"/health\", get(health))\n        \
.route(\"/items/{id}\", get(item))\n        .route(\"/orphan\", post(orphan))\n}\n\
fn main() {}\n";

/// Five `/health` calls, one template call and one call no route answers.
const WEB: &str = "import { util } from './util.js';\n\
async function load(api, id) {\n  await api(\"/health\");\n  await api(\"/health\");\n  \
await api(\"/health\");\n  await api(\"/health\");\n  await api(\"/health\");\n  \
await api(`/items/${id}`);\n  await api(\"/missing\");\n  util();\n}\n";

const ADMIN: &str = "import { view } from './view.js';\n\
async function poll(api) {\n  await api(\"/health\");\n  view();\n}\n";

/// A Rust server app, a web app and an admin app, each with its own manifest.
fn files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "Cargo.toml",
            "[package]\nname = \"srv\"\nversion = \"0.1.0\"\n",
        ),
        ("src/main.rs", SERVER),
        ("web/package.json", "{\"name\":\"web\"}\n"),
        ("web/app.js", WEB),
        ("web/util.js", "export function util() {}\n"),
        ("admin/package.json", "{\"name\":\"admin\"}\n"),
        ("admin/panel.js", ADMIN),
        ("admin/view.js", "export function view() {}\n"),
    ]
}

/// One MCP session over a published checkout: `ask` sends a tool call and returns its
/// envelope after checking its summary and code; every envelope is validated on drop.
struct Session {
    client: std::os::unix::net::UnixStream,
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    thread: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    structured: Vec<Value>,
    next: i64,
}

impl Session {
    fn ask(&mut self, name: &str, arguments: Value, code: Option<&str>) -> Value {
        self.next += 1;
        writeln!(
            self.client,
            "{}",
            modern(
                json!(self.next),
                "tools/call",
                json!({"name":name,"arguments":arguments})
            )
        )
        .unwrap();
        let reply = next_reply(&mut self.reader);
        tool(&reply, code, true);
        let envelope = reply["result"]["structuredContent"].clone();
        self.structured
            .push(json!({"name":name,"structuredContent":envelope}));
        envelope
    }

    fn finish(mut self) {
        validate_structured_content(&self.structured);
        let thread = self.thread.take().unwrap();
        drop(self);
        thread.join().unwrap().unwrap();
    }
}

struct Checkout {
    _temp: tempfile::TempDir,
    identity: trellis::store::topology::WorkspaceIdentity,
    registry: std::sync::Arc<tokio::sync::Mutex<CheckoutRegistry>>,
    session: u64,
}

impl Checkout {
    async fn publish(files: &[(&str, &str)], rules: Option<&str>, session: u64) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let (identity, topology) = published_inactive_checkout(temp.path(), files);
        if let Some(rules) = rules {
            fs::create_dir(identity.root.join(".trellis")).unwrap();
            fs::write(identity.root.join(".trellis/rules.toml"), rules).unwrap();
        }
        let mut registry = CheckoutRegistry::with_roots(topology);
        registry.attach_launch(session, &identity).unwrap();
        let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
        architecture_built(&registry, &identity).await;
        Self {
            _temp: temp,
            identity,
            registry,
            session,
        }
    }

    fn session(&self) -> Session {
        let (client, reader, thread) =
            socket_session(&self.identity, &self.registry, self.session, None);
        Session {
            client,
            reader,
            thread: Some(thread),
            structured: Vec::new(),
            next: 0,
        }
    }
}

/// The rules file of the rules-check and authored-app tests: `[apps.console]` takes the admin
/// directory from its manifest, and the server and web files each have their app's layer.
const RULES: &str = "version = 1\n\
[apps.console]\nroots = [\"admin\"]\n\
[layers.server]\napp = \"srv\"\norder = 1\npaths = [\"src/**\"]\n\
[layers.web]\napp = \"web\"\norder = 1\npaths = [\"web/**\"]\n";

fn names(apps: &Value) -> Vec<(String, String)> {
    apps["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|app| {
            (
                app["name"].as_str().unwrap().to_owned(),
                app["source"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// Describe lists the effective inventory: every detected manifest without a rules file, and
/// an authored app, first and shadowing the manifest under its root, with one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn describe_lists_detected_and_authored_apps() {
    let checkout = Checkout::publish(&files(), None, 8511).await;
    let mut session = checkout.session();
    let described = session.ask(
        "trellis_workspace_describe",
        json!({"schemaVersion":1}),
        None,
    );
    let apps = &described["data"]["apps"];
    assert_eq!(apps["state"], "ready", "{apps}");
    assert_eq!(
        names(apps),
        [
            ("srv".to_owned(), "manifest".to_owned()),
            ("admin".to_owned(), "manifest".to_owned()),
            ("web".to_owned(), "manifest".to_owned()),
        ]
    );
    let srv = &apps["items"][0];
    assert_eq!(
        (
            &srv["kind"],
            &srv["manifest"],
            &srv["roots"],
            &srv["entries"]
        ),
        (
            &json!("cargo"),
            &json!("Cargo.toml"),
            &json!(["."]),
            &json!(["srv"])
        )
    );
    assert_eq!(srv["languages"], json!(["rust"]));
    assert_eq!(srv["fileCount"], 1);
    let web = &apps["items"][2];
    assert_eq!(
        (
            &web["kind"],
            &web["roots"],
            &web["languages"],
            &web["fileCount"]
        ),
        (
            &json!("npm"),
            &json!(["web"]),
            &json!(["javascript"]),
            &json!(2)
        )
    );
    // The root Cargo app contains every file, so none is in no app.
    assert_eq!(apps["noApp"], json!({"fileCount":0}));
    session.finish();

    let checkout = Checkout::publish(&files(), Some(RULES), 8512).await;
    let mut session = checkout.session();
    let described = session.ask(
        "trellis_workspace_describe",
        json!({"schemaVersion":1}),
        None,
    );
    let apps = &described["data"]["apps"];
    assert_eq!(apps["state"], "ready", "{apps}");
    assert_eq!(
        names(apps),
        [
            ("console".to_owned(), "override".to_owned()),
            ("srv".to_owned(), "manifest".to_owned()),
            ("web".to_owned(), "manifest".to_owned()),
        ]
    );
    let console = &apps["items"][0];
    assert_eq!(
        (
            &console["kind"],
            &console["manifest"],
            &console["roots"],
            &console["entries"],
            &console["fileCount"]
        ),
        (
            &json!(null),
            &json!(null),
            &json!(["admin"]),
            &json!([]),
            &json!(2)
        )
    );
    // The same inventory rules check counts.
    let check = session.ask("trellis_rules_check", json!({"schemaVersion":1}), None);
    let checked: Vec<_> = check["data"]["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|app| app["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(checked, ["console", "srv", "web"]);
    session.finish();
}

/// #909: describe's `languages` leave out an app's files in authored hidden layers (here Java
/// and Python fixtures), which still count in its `fileCount`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn describe_languages_leave_out_hidden_fixture_layers() {
    let mut files = files();
    files.push((
        "tests/fixtures/java/App.java",
        "class App { void run() {} }\n",
    ));
    files.push(("tests/fixtures/python/app.py", "def run():\n    pass\n"));
    let rules = format!(
        "{RULES}[layers.fixtures]\napp = \"srv\"\ndisplay = \"hidden\"\norder = 0\n\
        paths = [\"tests/fixtures/**\"]\n"
    );
    let checkout = Checkout::publish(&files, Some(&rules), 8519).await;
    let mut session = checkout.session();
    let described = session.ask(
        "trellis_workspace_describe",
        json!({"schemaVersion":1}),
        None,
    );
    let apps = &described["data"]["apps"];
    assert_eq!(apps["state"], "ready", "{apps}");
    let srv = &apps["items"][1];
    assert_eq!(srv["name"], "srv");
    assert_eq!(
        (&srv["languages"], &srv["fileCount"]),
        (&json!(["rust"]), &json!(3))
    );
    session.finish();
}

/// The `app` filter of references, check, find, search and the call graph: each keeps what
/// the rules check assigns to that app, an app may be named by its manifest path, and an
/// unknown app is a typed `not_found` on every tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_app_filter_follows_the_rules_check_assignment() {
    let checkout = Checkout::publish(&files(), Some(RULES), 8513).await;
    let mut session = checkout.session();

    // References: only the app's sources are scanned.
    let sources = |envelope: &Value| -> Vec<String> {
        let mut paths: Vec<String> = envelope["data"]["production"]
            .as_array()
            .unwrap()
            .iter()
            .map(|edge| edge["source"]["path"].as_str().unwrap().to_owned())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    };
    let all = session.ask("trellis_references", json!({"schemaVersion":1}), None);
    assert!(sources(&all).len() >= 2, "{all}");
    for (app, prefix) in [
        ("web", "web/"),
        ("web/package.json", "web/"),
        ("console", "admin/"),
    ] {
        let scoped = session.ask(
            "trellis_references",
            json!({"schemaVersion":1,"app":app}),
            None,
        );
        let found = sources(&scoped);
        assert!(!found.is_empty(), "{app}: {scoped}");
        assert!(
            found.iter().all(|path| path.starts_with(prefix)),
            "{app}: {found:?}"
        );
        assert_eq!(scoped["data"]["counts"]["scannedDocuments"], 2, "{app}");
    }

    // Rules check: the instances `apps[].counts` counts under the app, whole counts.
    let check = session.ask(
        "trellis_rules_check",
        json!({"schemaVersion":1,"limit":50}),
        None,
    );
    let total = check["data"]["counts"]["comparable"].as_u64().unwrap();
    let mut listed = 0;
    for app in check["data"]["apps"].as_array().unwrap() {
        let name = app["name"].as_str().unwrap();
        let filtered = session.ask(
            "trellis_rules_check",
            json!({"schemaVersion":1,"limit":50,"app":name}),
            None,
        );
        let instances = filtered["data"]["instances"].as_array().unwrap().len() as u64;
        assert_eq!(
            instances,
            app["counts"]["comparable"].as_u64().unwrap(),
            "{name}"
        );
        assert!(instances > 0, "{name}: {filtered}");
        assert_eq!(filtered["data"]["counts"], check["data"]["counts"]);
        let paged: usize = ["instances", "testOnly", "unassigned", "externalPackages"]
            .iter()
            .map(|field| filtered["data"][*field].as_array().unwrap().len())
            .sum();
        assert_eq!(filtered["data"]["page"]["total"], paged as u64);
        listed += instances;
        let unassigned: Vec<_> = filtered["data"]["unassigned"]
            .as_array()
            .unwrap()
            .iter()
            .map(|identity| identity["path"].as_str().unwrap().to_owned())
            .collect();
        if name == "console" {
            assert_eq!(unassigned, ["admin/panel.js", "admin/view.js"]);
        } else {
            assert!(unassigned.is_empty(), "{name}: {unassigned:?}");
        }
    }
    let no_app = check["data"]["noApp"]["counts"]["comparable"]
        .as_u64()
        .unwrap();
    assert_eq!(listed + no_app, total);

    // Find and search keep the declarations in the app's files.
    let find = |session: &mut Session, query: &str, app: &str| -> Vec<String> {
        session.ask(
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":query,"app":app,"compact":true}),
            None,
        )["data"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(find(&mut session, "poll", "console"), ["poll"]);
    assert!(find(&mut session, "poll", "web").is_empty());
    assert_eq!(find(&mut session, "load", "web/package.json"), ["load"]);
    let search = |session: &mut Session, app: &str| -> Vec<String> {
        session.ask(
            "trellis_search_symbols",
            json!({"schemaVersion":1,"query":"poll","app":app,"compact":true}),
            None,
        )["data"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["path"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    let console_hits = search(&mut session, "console");
    assert!(!console_hits.is_empty());
    assert!(
        console_hits.iter().all(|path| path.starts_with("admin/")),
        "{console_hits:?}"
    );
    assert!(
        search(&mut session, "srv")
            .iter()
            .all(|path| !path.starts_with("admin/"))
    );

    // The call graph takes the filter; its root is the caller's own choice.
    let load = session.ask(
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"load"}),
        None,
    )["data"]["symbols"][0]["symbolId"]
        .clone();
    session.ask(
        "trellis_call_graph",
        json!({"schemaVersion":1,"symbolId":load,"app":"web"}),
        None,
    );

    // An unknown app is a typed `not_found` everywhere, with the same static message.
    for (name, arguments) in [
        (
            "trellis_references",
            json!({"schemaVersion":1,"app":"nope"}),
        ),
        (
            "trellis_rules_check",
            json!({"schemaVersion":1,"app":"nope"}),
        ),
        (
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":"load","app":"nope"}),
        ),
        (
            "trellis_search_symbols",
            json!({"schemaVersion":1,"query":"load","app":"nope"}),
        ),
        (
            "trellis_call_graph",
            json!({"schemaVersion":1,"symbolId":load,"app":"nope"}),
        ),
        ("trellis_endpoints", json!({"schemaVersion":1,"app":"nope"})),
        // A shadowed manifest names no app.
        (
            "trellis_endpoints",
            json!({"schemaVersion":1,"app":"admin/package.json"}),
        ),
    ] {
        let failed = session.ask(name, arguments, Some("not_found"));
        assert_eq!(
            failed["error"]["message"],
            "No app of this checkout has that name or manifest path; trellis_workspace_describe lists them",
            "{name}"
        );
    }
    // A malformed `app` is an invalid request that names the field.
    for app in [json!(""), json!(7), json!("a".repeat(4097))] {
        let failed = session.ask(
            "trellis_references",
            json!({"schemaVersion":1,"app":app}),
            Some("invalid_request"),
        );
        assert!(
            failed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("`app`")
        );
    }
    session.finish();
}

/// `trellis_endpoints` without a rules file: every route with its matched call sites, the
/// one-sided rows, paging, the single-endpoint form, the app filter and the bounds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoints_list_routes_with_their_callers_and_the_one_sided_rows() {
    let checkout = Checkout::publish(&files(), None, 8514).await;
    let mut session = checkout.session();
    let listed = session.ask("trellis_endpoints", json!({"schemaVersion":1}), None);
    let data = &listed["data"];
    assert_eq!(data["evidence"], "structural");
    assert_eq!(data["extracted"], true);
    assert_eq!(data["limitations"].as_array().unwrap().len(), 1);
    assert_eq!(
        data["page"],
        json!({"offset":0,"nextOffset":null,"total":4})
    );
    let rows: Vec<_> = data["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["list"].as_str().unwrap().to_owned(),
                row["method"].as_str().unwrap().to_owned(),
                row["pathTemplate"].as_str().unwrap().to_owned(),
                row["app"].as_str().unwrap().to_owned(),
                row["callSiteCount"].as_u64().unwrap(),
            )
        })
        .collect();
    let row = |list: &str, method: &str, path: &str, app: &str, sites: u64| {
        (
            list.to_owned(),
            method.to_owned(),
            path.to_owned(),
            app.to_owned(),
            sites,
        )
    };
    assert_eq!(
        rows,
        [
            row("matched", "GET", "/health", "srv", 6),
            row("matched", "GET", "/items/{id}", "srv", 1),
            row("serverOnly", "POST", "/orphan", "srv", 0),
            row("clientOnly", "GET", "/missing", "web", 1),
        ]
    );
    let health = &data["endpoints"][0];
    assert_eq!(health["handler"]["kind"], "declaration");
    assert_eq!(health["handler"]["path"], "src/main.rs");
    assert!(health["handler"]["stableLookupKey"].is_string());
    // A list row carries five of its six call sites.
    let sites = health["callSites"].as_array().unwrap();
    assert_eq!(sites.len(), 5);
    assert!(sites.iter().all(|site| site["confidence"] == "exact"));
    let site = sites
        .iter()
        .find(|site| site["path"] == "admin/panel.js")
        .unwrap();
    assert_eq!(site["app"], "admin");
    assert_eq!(site["caller"]["name"], "poll");
    assert_eq!(
        data["endpoints"][1]["callSites"][0]["confidence"],
        "template"
    );
    assert_eq!(data["endpoints"][2]["callSites"], json!([]));
    let client_only = &data["endpoints"][3];
    assert_eq!(client_only["handler"], json!(null));
    assert_eq!(client_only["callSites"][0]["confidence"], "unresolved");
    assert_eq!(client_only["callSites"][0]["caller"]["name"], "load");

    // Pages of one row continue in order.
    let mut paged = Vec::new();
    let mut offset = json!(0);
    while !offset.is_null() {
        let page = session.ask(
            "trellis_endpoints",
            json!({"schemaVersion":1,"limit":1,"offset":offset}),
            None,
        );
        assert_eq!(page["data"]["endpoints"].as_array().unwrap().len(), 1);
        paged.push(page["data"]["endpoints"][0].clone());
        offset = page["data"]["page"]["nextOffset"].clone();
    }
    assert_eq!(json!(paged), data["endpoints"]);

    // The single-endpoint form keeps every call site of the one route.
    let single = session.ask(
        "trellis_endpoints",
        json!({"schemaVersion":1,"method":"GET","pathTemplate":"/health"}),
        None,
    );
    let rows = single["data"]["endpoints"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["callSites"].as_array().unwrap().len(), 6);
    assert_eq!(rows[0]["handler"], health["handler"]);
    // A client-only call has its own single form; an endpoint nothing names answers no
    // rows, which proves nothing.
    let missing = session.ask(
        "trellis_endpoints",
        json!({"schemaVersion":1,"pathTemplate":"/missing"}),
        None,
    );
    assert_eq!(missing["data"]["endpoints"][0]["list"], "clientOnly");
    let none = session.ask(
        "trellis_endpoints",
        json!({"schemaVersion":1,"method":"DELETE","pathTemplate":"/health"}),
        None,
    );
    assert_eq!(none["data"]["endpoints"], json!([]));
    assert_eq!(none["data"]["page"]["total"], 0);

    // The app filter keeps rows with either end in the app.
    let lists = |envelope: &Value| -> Vec<String> {
        envelope["data"]["endpoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                format!(
                    "{} {}",
                    row["list"].as_str().unwrap(),
                    row["pathTemplate"].as_str().unwrap()
                )
            })
            .collect()
    };
    let admin = session.ask(
        "trellis_endpoints",
        json!({"schemaVersion":1,"app":"admin"}),
        None,
    );
    assert_eq!(lists(&admin), ["matched /health"]);
    let web = session.ask(
        "trellis_endpoints",
        json!({"schemaVersion":1,"app":"web/package.json"}),
        None,
    );
    assert_eq!(
        lists(&web),
        [
            "matched /health",
            "matched /items/{id}",
            "clientOnly /missing"
        ]
    );
    let srv = session.ask(
        "trellis_endpoints",
        json!({"schemaVersion":1,"app":"srv"}),
        None,
    );
    assert_eq!(lists(&srv).len(), 3);

    // Bounds and closed arguments.
    session.ask(
        "trellis_endpoints",
        json!({"schemaVersion":1,"limit":51}),
        Some("range_too_large"),
    );
    session.ask(
        "trellis_endpoints",
        json!({"schemaVersion":1,"offset":5}),
        Some("range_too_large"),
    );
    for arguments in [
        json!({"schemaVersion":1,"method":"get"}),
        json!({"schemaVersion":1,"method":"TRACE"}),
        json!({"schemaVersion":1,"pathTemplate":""}),
        json!({"schemaVersion":1,"list":"matched"}),
    ] {
        session.ask("trellis_endpoints", arguments, Some("invalid_request"));
    }
    session.finish();
}

/// With fewer than two apps nothing is extracted, and the answer says so instead of
/// returning a bare empty list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoints_with_one_app_say_they_were_not_extracted() {
    let checkout = Checkout::publish(
        &[
            ("web/package.json", "{\"name\":\"web\"}\n"),
            ("web/app.js", WEB),
            ("src/lib.rs", SERVER),
        ],
        None,
        8515,
    )
    .await;
    let mut session = checkout.session();
    let answer = session.ask("trellis_endpoints", json!({"schemaVersion":1}), None);
    assert_eq!(answer["data"]["extracted"], false);
    assert_eq!(answer["data"]["endpoints"], json!([]));
    assert_eq!(
        answer["data"]["limitations"][1],
        "not extracted: fewer than two apps"
    );
    session.finish();
}

/// #851 review (Prime, PR #868): one detected app and one authored app extract interfaces;
/// once the rules file is removed, an endpoints read waits for a core built without rules
/// instead of answering the ruled core's rows beside `extracted:false`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoints_after_the_rules_file_is_removed_answer_the_unruled_core() {
    let files = [
        ("web/package.json", "{\"name\":\"web\"}\n"),
        ("web/app.js", WEB),
        ("src/lib.rs", SERVER),
    ];
    let checkout = Checkout::publish(
        &files,
        Some("version = 1\n[apps.server]\nroots = [\"src\"]\n"),
        8516,
    )
    .await;
    let mut session = checkout.session();
    let ruled = session.ask("trellis_endpoints", json!({"schemaVersion":1}), None);
    assert_eq!(ruled["data"]["extracted"], true, "{ruled}");
    assert!(!ruled["data"]["endpoints"].as_array().unwrap().is_empty());
    fs::remove_file(checkout.identity.root.join(".trellis/rules.toml")).unwrap();
    let removed = session.ask("trellis_endpoints", json!({"schemaVersion":1}), None);
    assert_eq!(removed["data"]["extracted"], false, "{removed}");
    assert_eq!(removed["data"]["endpoints"], json!([]), "{removed}");
    assert_eq!(
        removed["data"]["limitations"][1],
        "not extracted: fewer than two apps"
    );
    session.finish();
}
