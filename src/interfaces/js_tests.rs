//! #855: JavaScript and TypeScript server routes and client calls, one fixture per framework
//! and client form.
use super::{Confidence, Extraction, HandlerKind, extract};
use crate::references::{DocumentInput, Snapshot, external::ManifestInput, work::Work};
use std::sync::{Arc, atomic::AtomicBool};

fn snapshot(files: &[(&str, &str)]) -> Snapshot {
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let identity =
        crate::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let (graph, native) = crate::indexer::index_workspace_with_native(
        &crate::indexer::IndexOptions::new(root.path().to_owned()),
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
            DocumentInput {
                semantic: Default::default(),
                declarations: native
                    .declarations
                    .iter()
                    .filter(|d| d.document == key)
                    .cloned()
                    .collect(),
                calls: native
                    .calls
                    .iter()
                    .filter(|c| c.document == key)
                    .cloned()
                    .collect(),
                key,
                file,
            }
        })
        .collect();
    let manifests = files
        .iter()
        .filter(|(path, _)| path.ends_with("package.json"))
        .map(|(path, text)| ManifestInput {
            source_set_id: String::new(),
            native_revision_id: String::new(),
            dependency_hash: String::new(),
            path: (*path).to_owned(),
            content_hash: None,
            bytes: Some(text.as_bytes().to_vec()),
        })
        .collect();
    Snapshot {
        pin: crate::model::IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 1,
        },
        documents,
        manifests,
        root_tsconfig: None,
        evidence_gaps: Default::default(),
    }
}

fn run(files: &[(&str, &str)]) -> (Snapshot, Extraction) {
    run_with("", files)
}

/// The extraction with the `[[interfaces.wrappers]]` of a rules file's text (#855).
fn run_with(rules: &str, files: &[(&str, &str)]) -> (Snapshot, Extraction) {
    let config = crate::rules::parse(&format!("version = 1\n{rules}")).unwrap();
    let shot = snapshot(files);
    let found = extract(
        &shot,
        config.interface_wrappers(),
        &mut Work::new(usize::MAX),
        &mut || Ok(()),
    )
    .unwrap();
    (shot, found)
}

/// Each route as `METHOD template <- handler`, the handler a declaration name or `closure`.
fn routes(files: &[(&str, &str)]) -> Vec<String> {
    let (shot, found) = run(files);
    found
        .routes
        .iter()
        .map(|route| {
            let handler = &route.handler;
            let name = match handler.kind {
                HandlerKind::Declaration => shot
                    .documents
                    .iter()
                    .flat_map(|d| &d.declarations)
                    .find(|d| Some(&d.syntax_id) == handler.syntax_id.as_ref())
                    .and_then(|d| d.name.clone())
                    .unwrap_or_else(|| "?".into()),
                _ => "closure".into(),
            };
            format!(
                "{} {} <- {}:{name}",
                route.method, route.path_template, handler.document.path
            )
        })
        .collect()
}

/// Each client call as `METHOD path -> route confidence`.
fn calls(files: &[(&str, &str)]) -> Vec<String> {
    calls_with("", files)
}

fn calls_with(rules: &str, files: &[(&str, &str)]) -> Vec<String> {
    let (_, found) = run_with(rules, files);
    found
        .calls
        .iter()
        .zip(&found.matched)
        .map(|(call, matched)| {
            let text = format!(
                "{} {}",
                call.method.as_deref().unwrap_or("?"),
                call.path_template.as_deref().unwrap_or("?")
            );
            match matched {
                Some(m) => format!(
                    "{text} -> {} {} {:?}",
                    found.routes[m.route].method, found.routes[m.route].path_template, m.confidence
                ),
                None => format!("{text} -> unresolved"),
            }
        })
        .collect()
}

const EXPRESS_APP: &str = r#"
const express = require("express");
const users = require("./users");
import { admin } from "./admin";
const app = express();
function health(req, res) { res.send("ok"); }
app.get("/health", health);
app.post("/items/:id", auth, (req, res) => res.send());
app.all("/any", health);
app.use("/api", users);
app.use("/admin", requireAdmin, admin);
app.route("/book").get(health).put(health);
app.listen(3000);
"#;

const EXPRESS_USERS: &str = r#"
const { Router } = require("express");
const nested = require("./nested").default;
const router = Router();
const list = async (req, res) => res.json([]);
router.get("/", list);
router.get("/users/:id", list);
router.use("/deep", nested);
module.exports = router;
"#;

const EXPRESS_NESTED: &str = r#"
import express from "express";
import { show } from "./handlers";
const router = express.Router();
router.delete("/x/*rest", show);
export default router;
"#;

const EXPRESS_HANDLERS: &str = "export function show(req, res) {}\n";

const EXPRESS_ADMIN: &str = r#"
import express from "express";
export const admin = express.Router();
admin.patch("/settings", (req, res) => {});
"#;

#[test]
fn express_routes_follow_mounts_across_files_and_name_their_handlers() {
    assert_eq!(
        routes(&[
            ("srv/app.js", EXPRESS_APP),
            ("srv/users.js", EXPRESS_USERS),
            ("srv/nested.js", EXPRESS_NESTED),
            ("srv/handlers.js", EXPRESS_HANDLERS),
            ("srv/admin.js", EXPRESS_ADMIN),
        ]),
        [
            "PATCH /admin/settings <- srv/admin.js:closure",
            "GET /health <- srv/app.js:health",
            "POST /items/{id} <- srv/app.js:closure",
            "ANY /any <- srv/app.js:health",
            "GET /book <- srv/app.js:health",
            "PUT /book <- srv/app.js:health",
            "DELETE /api/deep/x/{*rest} <- srv/handlers.js:show",
            "GET /api <- srv/users.js:list",
            "GET /api/users/{id} <- srv/users.js:list",
        ]
    );
}

#[test]
fn an_unmounted_router_and_a_non_framework_receiver_register_nothing() {
    let orphan = r#"
const express = require("express");
const router = express.Router();
router.get("/orphan", (req, res) => {});
const cache = new Map();
cache.get("/not-a-route", 1);
"#;
    let unrelated = r#"
const store = makeStore();
store.get("/nothing", () => 1);
"#;
    assert_eq!(
        routes(&[("srv/orphan.js", orphan), ("srv/unrelated.js", unrelated)]),
        Vec::<String>::new()
    );
}

#[test]
fn a_router_mounted_under_an_unreadable_prefix_registers_nothing() {
    let app = r#"
const express = require("express");
const app = express();
const router = express.Router();
router.get("/x", (req, res) => {});
app.use(process.env.PREFIX, router);
app.use(`/v${version}`, router);
"#;
    assert_eq!(routes(&[("srv/app.js", app)]), Vec::<String>::new());
}

const FASTIFY_SERVER: &str = r#"
import Fastify from "fastify";
import items from "./items.js";
const fastify = Fastify({ logger: true });
fastify.get("/ping", async () => "pong");
fastify.route({ method: ["GET", "POST"], url: "/forms/:id", handler: submit });
async function submit(request, reply) {}
fastify.register(items, { prefix: "/v1" });
fastify.register(require("./plain"));
"#;

const FASTIFY_ITEMS: &str = r#"
import fp from "fastify-plugin";
async function items(app, opts) {
  app.get("/items", { schema: {} }, list);
  app.put("/items/:id", { handler: update });
}
function list() {}
function update() {}
export default fp(items);
"#;

const FASTIFY_PLAIN: &str = r#"
module.exports = async function (instance) {
  instance.delete("/plain", async () => {});
};
"#;

#[test]
fn fastify_routes_route_options_and_registered_plugins() {
    assert_eq!(
        routes(&[
            ("api/server.ts", FASTIFY_SERVER),
            ("api/items.ts", FASTIFY_ITEMS),
            ("api/plain.js", FASTIFY_PLAIN),
        ]),
        [
            "GET /items <- api/items.ts:list",
            "PUT /items/{id} <- api/items.ts:update",
            "DELETE /plain <- api/plain.js:closure",
            "GET /ping <- api/server.ts:closure",
            "GET /forms/{id} <- api/server.ts:submit",
            "POST /forms/{id} <- api/server.ts:submit",
        ]
    );
}

const KOA_APP: &str = r#"
const Koa = require("koa");
const Router = require("@koa/router");
const app = new Koa();
const api = new Router({ prefix: "/api" });
const nested = new Router();
const later = new Router();
later.prefix("/later");
api.get("/users/:id", show);
api.del("/users/:id", show);
api.get("named", "/named", show);
nested.post("/inner", (ctx) => {});
api.use("/nested", nested.routes());
later.get("/", show);
const unused = new Router();
unused.get("/unused", show);
function show(ctx) {}
app.use(api.routes()).use(api.allowedMethods());
app.use(later.routes());
"#;

#[test]
fn koa_router_prefixes_nesting_and_named_routes() {
    assert_eq!(
        routes(&[("srv/app.js", KOA_APP)]),
        [
            "GET /api/users/{id} <- srv/app.js:show",
            "DELETE /api/users/{id} <- srv/app.js:show",
            "GET /api/named <- srv/app.js:show",
            "POST /api/nested/inner <- srv/app.js:closure",
            "GET /later <- srv/app.js:show",
        ]
    );
}

const NEXT_PACKAGE: &str = r#"{"name":"site","dependencies":{"next":"15.0.0","react":"19.0.0"}}"#;

#[test]
fn next_pages_and_app_router_files_are_routes_only_in_a_next_app() {
    let files = [
        ("site/package.json", NEXT_PACKAGE),
        (
            "site/pages/api/users/[id].ts",
            "export default function handler(req, res) { res.status(200).json({}); }\n",
        ),
        (
            "site/src/pages/api/index.js",
            "export default async (req, res) => { res.end(); };\n",
        ),
        (
            "site/app/(shop)/items/[...slug]/route.ts",
            "export async function GET(request: Request) { return Response.json([]); }\nexport const POST = async () => new Response();\nexport function helper() {}\n",
        ),
        (
            "site/app/docs/[[...page]]/route.js",
            "function handle() {}\nexport { handle as PUT };\n",
        ),
        (
            "site/app/items/page.tsx",
            "export default function Page() { return null; }\n",
        ),
        (
            "plain/pages/api/x.js",
            "export default function handler(req, res) {}\n",
        ),
    ];
    assert_eq!(
        routes(&files),
        [
            "GET /items/{*slug} <- site/app/(shop)/items/[...slug]/route.ts:GET",
            "POST /items/{*slug} <- site/app/(shop)/items/[...slug]/route.ts:POST",
            "PUT /docs <- site/app/docs/[[...page]]/route.js:handle",
            "PUT /docs/{*page} <- site/app/docs/[[...page]]/route.js:handle",
            "ANY /api/users/{id} <- site/pages/api/users/[id].ts:handler",
            "ANY /api <- site/src/pages/api/index.js:closure",
        ]
    );
}

const SERVER: &str = r#"
const express = require("express");
const app = express();
app.get("/health", h);
app.get("/users/:id", h);
app.get("/users/me", h);
app.post("/users", h);
app.all("/legacy/:x", h);
app.get("/legacy/known", h);
app.get("/api/v2/items", h);
function h() {}
"#;

#[test]
fn fetch_calls_read_literals_templates_and_constants() {
    let client = r#"
import { API } from "./config";
const USERS = "/users";
let mutable = "/health";
export async function load(id) {
  await fetch("/health");
  await fetch(`/users/${id}`, { method: "GET" });
  await fetch(USERS + "/me");
  await fetch(`${USERS}/${id}?full=1`);
  await fetch(API + "/items", { method: "post" });
  await window.fetch("/users", { method: "POST" });
  await fetch(mutable);
  await fetch("https://example.test/health");
  await fetch("/legacy/known", { method: "DELETE" });
  await fetch("/legacy/known");
  await fetch("/nowhere");
}
"#;
    assert_eq!(
        calls(&[
            ("srv/app.js", SERVER),
            ("web/client.js", client),
            ("web/config.js", "export const API = \"/api/v2\";\n"),
        ]),
        [
            "GET /health -> GET /health Exact",
            "GET /users/{} -> GET /users/{id} Template",
            "GET /users/me -> GET /users/me Exact",
            "GET /users/{} -> GET /users/{id} Template",
            "POST /api/v2/items -> unresolved",
            "POST /users -> POST /users Exact",
            "GET ? -> unresolved",
            "DELETE /legacy/known -> ANY /legacy/{x} Template",
            "GET /legacy/known -> GET /legacy/known Exact",
            "GET /nowhere -> unresolved",
        ]
    );
}

#[test]
fn axios_calls_configs_and_instances_with_a_base_url() {
    let client = r#"
import axios from "axios";
const http = axios.create({ baseURL: "/api/v2" });
const remote = axios.create({ baseURL: "https://example.test" });
const unknown = axios.create({ baseURL: process.env.BASE });
export async function go(id) {
  await axios.get("/health");
  await axios.post("/users", { name: "x" });
  await axios({ method: "get", url: `/users/${id}` });
  await axios("/users/me");
  await axios.request({ url: "/users", method: "POST" });
  await http.get("/items");
  await http({ url: "items", method: "get" });
  await remote.get("/health");
  await unknown.get("/health");
  await axios.get("/users", { baseURL: "/api/v2" });
}
"#;
    assert_eq!(
        calls(&[("srv/app.js", SERVER), ("web/client.js", client)]),
        [
            "GET /health -> GET /health Exact",
            "POST /users -> POST /users Exact",
            "GET /users/{} -> GET /users/{id} Template",
            "GET /users/me -> GET /users/me Exact",
            "POST /users -> POST /users Exact",
            "GET /api/v2/items -> GET /api/v2/items WrapperInferred",
            "GET /api/v2/items -> GET /api/v2/items WrapperInferred",
            "GET ? -> unresolved",
            "GET /api/v2/users -> unresolved",
        ]
    );
}

#[test]
fn configured_wrappers_apply_under_their_root_and_the_wrapper_body_is_not_a_call() {
    let rules = r#"
[[interfaces.wrappers]]
callee = "apiFetch"
prefix = "/api/v2"
root = "web"

[[interfaces.wrappers]]
callee = "client.send"
method = "argument"
at = 1
root = "web"
"#;
    let client = r#"
async function apiFetch(path, options) {
  return fetch("/api/v2" + path, options);
}
export async function go(client) {
  await apiFetch("/items");
  await apiFetch("/items", { method: "DELETE" });
  await client.send("/users", "post");
}
"#;
    let elsewhere = "export function go(client) { client.send(\"/users\", \"post\"); }\n";
    let files = [
        ("srv/app.js", SERVER),
        ("web/client.js", client),
        ("other/client.js", elsewhere),
    ];
    assert_eq!(
        calls_with(rules, &files),
        [
            "GET /api/v2/items -> GET /api/v2/items WrapperInferred",
            "DELETE /api/v2/items -> unresolved",
            "POST /users -> POST /users Exact",
        ]
    );
    let (_, found) = run_with(rules, &files);
    assert!(found.callees.iter().any(|c| c == "apiFetch"));
    assert!(found.callees.iter().any(|c| c == "client.send"));
    // Without a root, a wrapper applies to every file.
    let everywhere = "[[interfaces.wrappers]]\ncallee = \"client.send\"\nmethod = \"argument\"\n";
    assert_eq!(
        calls_with(
            everywhere,
            &[("srv/app.js", SERVER), ("other/client.js", elsewhere)]
        ),
        ["POST /users -> POST /users Exact"]
    );
}

/// Negative control: wrapper configuration lives only in the rules file. A `trellis` key in
/// a `package.json` configures nothing.
#[test]
fn a_package_json_trellis_key_configures_no_wrapper() {
    let package =
        r#"{"name":"web","trellis":{"wrappers":[{"callee":"apiFetch","prefix":"/api/v2"}]}}"#;
    let client = "export const go = () => apiFetch(\"/items\");\n";
    let (_, found) = run(&[
        ("srv/app.js", SERVER),
        ("web/package.json", package),
        ("web/client.js", client),
    ]);
    assert!(found.calls.is_empty());
    assert!(!found.callees.iter().any(|c| c == "apiFetch"));
}

#[test]
fn express_client_and_server_match_with_template_confidence_for_any_method() {
    let found = run(&[
        ("srv/app.js", SERVER),
        (
            "web/client.js",
            "export const go = () => fetch(\"/legacy/7\", { method: \"PATCH\" });\n",
        ),
    ])
    .1;
    let matched = found.matched[0].unwrap();
    assert_eq!(found.routes[matched.route].method, super::ANY);
    assert_eq!(matched.confidence, Confidence::Template);
}

#[test]
fn test_files_register_no_routes_and_make_no_calls() {
    let test = r#"
const express = require("express");
const app = express();
app.get("/test-only", (req, res) => {});
fetch("/health");
"#;
    let (_, found) = run(&[
        ("srv/app.js", SERVER),
        ("srv/app.test.js", test),
        ("tests/e2e.js", test),
    ]);
    assert!(found.routes.iter().all(|r| r.path_template != "/test-only"));
    assert!(found.calls.is_empty());
}

#[test]
fn javascript_extraction_over_its_budget_is_a_typed_bounded_work_error() {
    let shot = snapshot(&[
        ("srv/app.js", EXPRESS_APP),
        ("srv/users.js", EXPRESS_USERS),
        ("web/client.js", "fetch(\"/api\");\n"),
    ]);
    let mut spent = Work::new(usize::MAX);
    extract(&shot, &[], &mut spent, &mut || Ok(())).unwrap();
    for short in [0, spent.used / 3, spent.used / 2, spent.used - 1] {
        let error = extract(&shot, &[], &mut Work::new(short), &mut || Ok(()))
            .err()
            .unwrap();
        assert_eq!(error.to_string(), super::BUDGET, "{short}");
    }
    extract(&shot, &[], &mut Work::new(spent.used), &mut || Ok(())).unwrap();
}

#[test]
fn a_cyclic_mount_terminates() {
    let app = r#"
const express = require("express");
const app = express();
const a = express.Router();
const b = express.Router();
a.use("/b", b);
b.use("/a", a);
a.get("/x", (req, res) => {});
app.use("/a", a);
"#;
    let found = routes(&[("srv/app.js", app)]);
    assert!(found.contains(&"GET /a/x <- srv/app.js:closure".to_owned()));
    assert!(found.len() <= 20, "{found:?}");
}

// #866 review (Prime2): unknown spreads, fastify-plugin encapsulation and unknown prefixes.

/// A spread or unknown member that could set `url`, `baseURL` or `method` makes that part
/// unreadable: never a local exact or inferred match. A known key written after the spread
/// is read again.
#[test]
fn an_unknown_spread_never_invents_a_local_match() {
    let client = r#"
import axios from "axios";
const opts = makeOptions();
const shared = { ...opts };
const http = axios.create({ baseURL: "/api/v2", ...opts });
const fixed = axios.create({ ...opts, baseURL: "/api/v2" });
const computed = axios.create({ [key]: "/x" });
export async function go() {
  await axios.get("/health", { ...opts });
  await axios.get("/health", shared);
  await http.get("/items");
  await fixed.get("/items");
  await computed.get("/health");
  await fetch("/health", { method: "GET", ...opts });
  await fetch("/health", { ...opts, method: "GET" });
}
"#;
    assert_eq!(
        calls(&[("srv/app.js", SERVER), ("web/client.js", client)]),
        [
            "GET ? -> unresolved",
            "GET ? -> unresolved",
            "GET ? -> unresolved",
            "GET /api/v2/items -> GET /api/v2/items WrapperInferred",
            "GET ? -> unresolved",
            "? /health -> unresolved",
            "GET /health -> GET /health Exact",
        ]
    );
}

/// Fastify: a plugin wrapped in `fastify-plugin` is not encapsulated, so `register`'s
/// `prefix` does not apply to its routes (https://fastify.dev/docs/latest/Reference/Routes/,
/// "Route Prefixing and fastify-plugin"). A plain plugin keeps the prefix, and so does a
/// wrapped plugin with `encapsulate: true`.
#[test]
fn a_fastify_plugin_wrapped_plugin_ignores_the_register_prefix() {
    let server = r#"
import Fastify from "fastify";
import fp from "fastify-plugin";
const fastify = Fastify();
async function wrapped(app) { app.get("/wrapped", h); }
async function plain(app) { app.get("/plain", h); }
async function sealed(app) { app.get("/sealed", h); }
async function unsure(app) { app.get("/unsure", h); }
function h() {}
fastify.register(fp(wrapped), { prefix: "/v1" });
fastify.register(plain, { prefix: "/v1" });
fastify.register(fp(sealed, { encapsulate: true }), { prefix: "/v1" });
fastify.register(fp(unsure, { ...options }), { prefix: "/v1" });
"#;
    assert_eq!(
        routes(&[("api/server.js", server)]),
        [
            "GET /wrapped <- api/server.js:h",
            "GET /v1/plain <- api/server.js:h",
            "GET /v1/sealed <- api/server.js:h",
        ]
    );
}

/// A mount whose first argument has no readable value, such as a call or a conditional,
/// mounts nothing; a `const` prefix still mounts.
#[test]
fn an_unknown_mount_prefix_registers_nothing() {
    let app = r#"
const express = require("express");
const app = express();
const router = express.Router();
const other = express.Router();
const PREFIX = "/v1";
router.get("/x", (req, res) => {});
other.get("/y", (req, res) => {});
app.use(getPrefix(), router);
app.use(flag ? "/a" : "/b", router);
app.use(PREFIX, other);
"#;
    assert_eq!(
        routes(&[("srv/app.js", app)]),
        ["GET /v1/y <- srv/app.js:closure"]
    );
}

/// An axios instance imported from another file and called directly is read by its binding,
/// not by whether the calling file mentions axios: an unrelated comment changes nothing.
#[test]
fn an_imported_callable_axios_instance_is_read_by_its_binding() {
    let http = "import axios from \"axios\";\nexport const http = axios.create({ baseURL: \"/api/v2\" });\n";
    let client = "import { http } from \"./http\";\nexport const go = () => http(\"/items\");\n";
    let mentioned = format!("// axios\n{client}");
    for client in [client, mentioned.as_str()] {
        assert_eq!(
            calls(&[
                ("srv/app.js", SERVER),
                ("web/http.js", http),
                ("web/client.js", client),
            ]),
            ["GET /api/v2/items -> GET /api/v2/items WrapperInferred"],
            "{client}"
        );
    }
}
