//! #856: Python server routes (Flask, FastAPI, Django) and client calls (requests, httpx),
//! one fixture per framework and client form, and a regression test for each review lesson
//! of #866 and #867.
use super::{BUDGET, ClientPrefix, Confidence, Extraction, HandlerKind, extract};
use crate::references::{DocumentInput, Snapshot, work::Work};
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
    Snapshot {
        pin: crate::model::IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 1,
        },
        documents,
        manifests: vec![],
        root_tsconfig: None,
        evidence_gaps: Default::default(),
    }
}

fn extract_with(shot: &Snapshot, work: &mut Work) -> anyhow::Result<Extraction> {
    extract(shot, &[], &Default::default(), work, &mut || Ok(()))
}

fn run(files: &[(&str, &str)]) -> (Snapshot, Extraction) {
    let shot = snapshot(files);
    let found = extract_with(&shot, &mut Work::new(usize::MAX)).unwrap();
    (shot, found)
}

/// Each route as `METHOD template <- path:handler`, the handler a declaration name or
/// `closure`.
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
    let (_, found) = run(files);
    found
        .calls
        .iter()
        .zip(&found.matched)
        .map(|(call, matched)| {
            let call_text = format!(
                "{} {}",
                call.method.as_deref().unwrap_or("?"),
                call.path_template.as_deref().unwrap_or("?")
            );
            match matched {
                Some(m) => format!(
                    "{call_text} -> {} {:?}",
                    found.routes[m.route].path_template, m.confidence
                ),
                None => format!("{call_text} -> unresolved"),
            }
        })
        .collect()
}

/// The `METHOD path` of each client call of one client document.
fn client_paths(client: &str) -> Vec<String> {
    let (_, found) = run(&[("cli/client.py", client)]);
    found
        .calls
        .iter()
        .map(|call| {
            format!(
                "{} {}",
                call.method.as_deref().unwrap_or("?"),
                call.path_template.as_deref().unwrap_or("?")
            )
        })
        .collect()
}

const FLASK_APP: &str = r#"
from flask import Flask
from .users import users

app = Flask(__name__)
app.register_blueprint(users, url_prefix="/api/users")

@app.route("/health")
def health():
    return "ok"

@app.route("/items", methods=["GET", "POST"])
def items():
    return "items"

@app.get("/items/<int:item_id>")
def item(item_id):
    return "item"

@app.route("/files/<path:rest>")
def files(rest):
    return rest
"#;

const FLASK_USERS: &str = r#"
from flask import Blueprint
from .admin import admin

users = Blueprint("users", __name__, url_prefix="/ignored")
users.register_blueprint(admin)

@users.route("/<int:user_id>", methods=["DELETE"])
def remove(user_id):
    return ""

@users.post("")
def create():
    return ""
"#;

const FLASK_ADMIN: &str = r#"
import flask

admin = flask.Blueprint("admin", __name__, url_prefix="/admin")

@admin.route("/stats")
def stats():
    return ""
"#;

fn flask() -> Vec<(&'static str, &'static str)> {
    vec![
        ("svc/app.py", FLASK_APP),
        ("svc/users.py", FLASK_USERS),
        ("svc/admin.py", FLASK_ADMIN),
    ]
}

#[test]
fn flask_routes_and_blueprints_are_routes_under_their_prefixes() {
    assert_eq!(
        routes(&flask()),
        [
            "GET /api/users/admin/stats <- svc/admin.py:stats",
            "GET /health <- svc/app.py:health",
            "GET /items <- svc/app.py:items",
            "POST /items <- svc/app.py:items",
            "GET /items/{item_id} <- svc/app.py:item",
            "GET /files/{*rest} <- svc/app.py:files",
            "DELETE /api/users/{user_id} <- svc/users.py:remove",
            "POST /api/users <- svc/users.py:create",
        ]
    );
}

const FASTAPI_MAIN: &str = r#"
from fastapi import FastAPI
from api.routers import items
from .routers.users import router as users_router

app = FastAPI()
app.include_router(items.router, prefix="/v1")
app.include_router(users_router)

@app.get("/")
async def root():
    return {}

@app.api_route("/ping", methods=["GET", "HEAD"])
async def ping():
    return {}
"#;

const FASTAPI_ITEMS: &str = r#"
from fastapi import APIRouter

router = APIRouter(prefix="/items")

@router.get("/{item_id}")
async def read_item(item_id: int):
    return {}

@router.put("/{item_id:int}")
async def update_item(item_id: int):
    return {}

@router.get("/files/{file_path:path}")
async def read_file(file_path: str):
    return {}
"#;

const FASTAPI_USERS: &str = r#"
import fastapi

router = fastapi.APIRouter(prefix="/users")
sub = fastapi.APIRouter()
router.include_router(sub, prefix="/{user_id}")

@router.post("")
async def create():
    return {}

@sub.get("/orders")
async def orders(user_id: int):
    return {}
"#;

fn fastapi() -> Vec<(&'static str, &'static str)> {
    vec![
        ("api/main.py", FASTAPI_MAIN),
        ("api/routers/items.py", FASTAPI_ITEMS),
        ("api/routers/users.py", FASTAPI_USERS),
    ]
}

#[test]
fn fastapi_routes_and_included_routers_are_routes_under_their_prefixes() {
    assert_eq!(
        routes(&fastapi()),
        [
            "GET / <- api/main.py:root",
            "GET /ping <- api/main.py:ping",
            "HEAD /ping <- api/main.py:ping",
            "GET /v1/items/{item_id} <- api/routers/items.py:read_item",
            "PUT /v1/items/{item_id} <- api/routers/items.py:update_item",
            "GET /v1/items/files/{*file_path} <- api/routers/items.py:read_file",
            "POST /users <- api/routers/users.py:create",
            "GET /users/{user_id}/orders <- api/routers/users.py:orders",
        ]
    );
}

const DJANGO_SETTINGS: &str = "ROOT_URLCONF = \"project.urls\"\nDEBUG = False\n";

const DJANGO_URLS: &str = r#"
from django.urls import include, path, re_path
from blog import views as blog_views
from . import views

urlpatterns = [
    path("", views.index),
    path("blog/", include("blog.urls")),
    path("api/", include([path("status/", views.status)])),
    re_path(r"^archive/(?P<year>[0-9]{4})/$", blog_views.archive),
]
urlpatterns += [path("extra/<slug:slug>/", views.extra)]
"#;

const DJANGO_VIEWS: &str = r#"
from django.http import HttpResponse

def index(request):
    return HttpResponse("")

def status(request):
    return HttpResponse("")

def extra(request, slug):
    return HttpResponse("")
"#;

const BLOG_URLS: &str = r#"
from django.urls import path, re_path
from . import views

urlpatterns = [
    path("<int:pk>/", views.detail),
    path("files/<path:rest>", views.files),
    re_path(r"^tag/(?P<name>[a-z]+)$", views.tag),
    path("other/", lambda request: None),
]
"#;

const BLOG_VIEWS: &str = r#"
from django.http import HttpResponse

def detail(request, pk):
    return HttpResponse("")

def files(request, rest):
    return HttpResponse("")

def tag(request, name):
    return HttpResponse("")

def archive(request, year):
    return HttpResponse("")
"#;

fn django() -> Vec<(&'static str, &'static str)> {
    vec![
        ("site/project/settings.py", DJANGO_SETTINGS),
        ("site/project/urls.py", DJANGO_URLS),
        ("site/project/views.py", DJANGO_VIEWS),
        ("site/blog/urls.py", BLOG_URLS),
        ("site/blog/views.py", BLOG_VIEWS),
    ]
}

#[test]
fn django_patterns_reached_from_the_root_urlconf_are_any_routes() {
    assert_eq!(
        routes(&django()),
        [
            "ANY /blog/{pk}/ <- site/blog/views.py:detail",
            "ANY /blog/files/{*rest} <- site/blog/views.py:files",
            "ANY /blog/tag/{name} <- site/blog/views.py:tag",
            "ANY /blog/other/ <- site/blog/urls.py:closure",
            "ANY / <- site/project/views.py:index",
            "ANY /api/status/ <- site/project/views.py:status",
            "ANY /archive/{year}/ <- site/blog/views.py:archive",
            "ANY /extra/{slug}/ <- site/project/views.py:extra",
        ]
    );
}

#[test]
fn django_patterns_without_a_root_urlconf_are_not_routes() {
    let without: Vec<_> = django()
        .into_iter()
        .filter(|(path, _)| !path.ends_with("settings.py"))
        .collect();
    assert!(routes(&without).is_empty());
    // A settings module naming a module that is not there mounts nothing either.
    let mut missing = without.clone();
    missing.push((
        "site/project/settings.py",
        "ROOT_URLCONF = \"nowhere.urls\"\n",
    ));
    assert!(routes(&missing).is_empty());
}

const CLIENT: &str = r#"
import requests
import httpx
from requests import post as send

BASE = "http://localhost:8000"
session = requests.Session()

def run(item_id, user_id):
    requests.get(f"{BASE}/items")
    requests.request("POST", BASE + "/items")
    send(url=f"{BASE}/api/users")
    session.delete("%s/api/users/%s" % (BASE, user_id))
    httpx.get("{}/items/{}".format(BASE, item_id))
    with httpx.Client(base_url="http://localhost:8000/v1") as client:
        client.put(f"/items/{item_id}")
        client.get("http://localhost:8000/health")
    requests.get("/items")
    requests.get(f"{BASE}/missing")

async def go(user_id):
    async with httpx.AsyncClient(base_url=BASE) as aclient:
        await aclient.get(f"/users/{user_id}/orders")
        await aclient.request("GET", "/ping")
"#;

#[test]
fn requests_and_httpx_calls_match_flask_and_fastapi_routes() {
    let mut files = flask();
    files.extend(fastapi());
    files.push(("cli/client.py", CLIENT));
    assert_eq!(
        calls(&files),
        [
            "GET /items -> /items Exact",
            "POST /items -> /items Exact",
            "POST /api/users -> /api/users Exact",
            "DELETE /api/users/{} -> /api/users/{user_id} Template",
            "GET /items/{} -> /items/{item_id} Template",
            "PUT /v1/items/{} -> /v1/items/{item_id} WrapperInferred",
            "GET /health -> /health Exact",
            "GET ? -> unresolved",
            "GET /missing -> unresolved",
            "GET /users/{}/orders -> /users/{user_id}/orders Template",
            "GET /ping -> /ping Exact",
        ]
    );
    let (_, found) = run(&files);
    let first = &found.calls[0];
    assert_eq!(first.lines.start, 10);
    assert!(first.call_id.is_some());
    let caller = serde_json::to_value(first.caller.as_ref().unwrap()).unwrap();
    assert_eq!(caller["name"], "run");
    assert_eq!(found.calls[5].prefix, ClientPrefix::Joined);
    assert!(
        found
            .calls
            .iter()
            .enumerate()
            .all(|(at, call)| at == 5 || call.prefix == ClientPrefix::Written)
    );
}

#[test]
fn requests_calls_match_django_any_routes() {
    let client = r#"
import requests
BASE = "http://localhost:8000"
def run(pk):
    requests.post(f"{BASE}/blog/{pk}/")
    requests.get(f"{BASE}/extra/news/")
"#;
    let mut files = django();
    files.push(("cli/client.py", client));
    assert_eq!(
        calls(&files),
        [
            "POST /blog/{}/ -> /blog/{pk}/ Template",
            "GET /extra/news/ -> /extra/{slug}/ Template",
        ]
    );
}

#[test]
fn python_code_that_is_not_a_request_is_not_a_client_call() {
    let lookalikes = r#"
import requests
from cache import get
def f(cache, data, self):
    cache.get("http://localhost/health")
    data.get("http://localhost/health")
    get("http://localhost/health")
    self.session.get("http://localhost/health")
    requests.Session().mount("http://", None)
    requests.get()
    requests.send("http://localhost/health")
"#;
    let no_library = "def g(http):\n    http.get(\"http://localhost/health\")\n";
    let in_tests =
        "import requests\ndef test_it():\n    requests.get(\"http://localhost/health\")\n";
    let (_, found) = run(&[
        ("cli/client.py", lookalikes),
        ("cli/plain.py", no_library),
        ("cli/tests/api.py", in_tests),
        ("cli/test_client.py", in_tests),
    ]);
    assert!(found.calls.is_empty(), "{}", found.calls.len());
}

#[test]
fn objects_that_are_not_framework_objects_register_nothing() {
    let source = r#"
from myweb import Flask
from flask import Blueprint
app = Flask(__name__)
loose = Blueprint("loose", __name__)

@app.route("/fake")
def fake():
    return ""

@loose.route("/unregistered")
def unregistered():
    return ""

def init(server):
    @server.route("/param")
    def param():
        return ""
"#;
    let in_tests = "from flask import Flask\napp = Flask(__name__)\n@app.route(\"/t\")\ndef t():\n    return \"\"\n";
    assert!(routes(&[("svc/app.py", source), ("svc/tests/app.py", in_tests)]).is_empty());
}

// #866 / #867 review lessons.

/// Never invent a match: a prefix that cannot be read registers nothing under it, never at
/// the root.
#[test]
fn an_unreadable_prefix_registers_nothing_under_it() {
    let flask = r#"
import os
from flask import Flask, Blueprint
app = Flask(__name__)
env = Blueprint("env", __name__, url_prefix=os.environ["P"])
spread = Blueprint("spread", __name__, **options)
dynamic = Blueprint("dynamic", __name__)
splat = Blueprint("splat", __name__)
named = Blueprint("named", __name__, url_prefix=os.environ["P"])
app.register_blueprint(env)
app.register_blueprint(spread)
app.register_blueprint(dynamic, url_prefix=prefix)
app.register_blueprint(splat, **options)
app.register_blueprint(named, url_prefix="/named")

@env.route("/a")
def a():
    return ""

@spread.route("/b")
def b():
    return ""

@dynamic.route("/c")
def c():
    return ""

@splat.route("/d")
def d():
    return ""

@named.route("/e")
def e():
    return ""
"#;
    assert_eq!(
        routes(&[("svc/app.py", flask)]),
        ["GET /named/e <- svc/app.py:e"]
    );
    let fastapi = r#"
from fastapi import FastAPI, APIRouter
from settings import PREFIX
app = FastAPI()
first = APIRouter(prefix=PREFIX)
second = APIRouter(**config)
third = APIRouter()
fourth = APIRouter()
fifth = APIRouter()
app.include_router(first)
app.include_router(second)
app.include_router(third, prefix=PREFIX)
app.include_router(fourth, **options)
app.include_router(fifth, prefix="/ok")

@first.get("/a")
def a():
    return {}

@second.get("/b")
def b():
    return {}

@third.get("/c")
def c():
    return {}

@fourth.get("/d")
def d():
    return {}

@fifth.get("/e")
def e():
    return {}

def mount(router):
    app.include_router(router)
"#;
    assert_eq!(
        routes(&[("api/main.py", fastapi)]),
        ["GET /ok/e <- api/main.py:e"]
    );
}

/// Never invent a match: an unreadable method or methods list registers nothing.
#[test]
fn an_unreadable_method_registers_nothing() {
    let source = r#"
from flask import Flask
app = Flask(__name__)
VERBS = ["GET"]

@app.route("/a", methods=VERBS)
def a():
    return ""

@app.route("/b", methods=["GET", verb])
def b():
    return ""

@app.route("/c", **options)
def c():
    return ""

@app.route("/d", methods=["GET", *more])
def d():
    return ""

@app.route(path)
def e():
    return ""

@app.route("/f", methods=("PUT",))
def f():
    return ""
"#;
    assert_eq!(
        routes(&[("svc/app.py", source)]),
        ["PUT /f <- svc/app.py:f"]
    );
}

/// Never invent a match: a client whose URL, base URL or method cannot be read is
/// unresolved, never rooted.
#[test]
fn an_unreadable_client_url_base_or_method_is_unresolved() {
    let client = r#"
import requests, httpx
def run(options, verb, base, args):
    requests.get(**options)
    requests.request(verb, "http://h/a")
    requests.request("FETCH", "http://h/a")
    requests.request(*args)
    httpx.Client(**options).get("/a")
    httpx.Client(base_url=base).get("/a")
    httpx.Client(base_url="/relative").get("/a")
    httpx.Client(base_url=f"http://{base}").get("/a")
    requests.get("/a")
    requests.get(base + "/a")
    httpx.request(method="DELETE", url="http://h/a")
"#;
    assert_eq!(
        client_paths(client),
        [
            "GET ?",
            "? /a",
            "? /a",
            "? ?",
            "GET ?",
            "GET ?",
            "GET ?",
            "GET ?",
            "GET ?",
            "GET ?",
            "DELETE /a",
        ]
    );
}

/// Constant visibility: any other binding of the name anywhere in the document makes the
/// module-level constant unreadable.
#[test]
fn a_name_also_bound_anywhere_else_is_unreadable() {
    for binding in [
        "from config import URL",
        "import config as URL",
        "import URL",
        "URL = \"http://localhost/other\"",
        "URL += \"/x\"",
        "for URL in []: pass",
        "[0 for URL in []]",
        "{URL: 0 for URL in []}",
        "with open(p) as URL: pass",
        "try:\n        pass\n    except Exception as URL:\n        pass",
        "(URL := 1)",
        "global URL",
        "f = lambda URL: URL",
        "class URL: pass",
        "def URL(): pass",
        "match p:\n        case [URL]: pass",
    ] {
        let client = format!(
            "import requests\nURL = \"http://localhost/health\"\nKEPT = \"http://localhost/kept\"\ndef other(p):\n    {binding}\ndef run():\n    requests.get(URL)\n    requests.get(KEPT)\n"
        );
        assert_eq!(client_paths(&client), ["GET ?", "GET /kept"], "{binding}");
    }
    let parameter = "import requests\nURL = \"http://localhost/health\"\ndef run(URL):\n    requests.get(URL)\n";
    assert_eq!(client_paths(parameter), ["GET ?"]);
    let local = "import requests\ndef run():\n    URL = \"http://localhost/health\"\n    requests.get(URL)\n";
    assert_eq!(client_paths(local), ["GET ?"]);
}

/// Constant visibility: a module-level star import may rebind any name bound before it; one
/// inside a function or class may bind any name.
#[test]
fn a_star_import_hides_the_names_it_may_rebind() {
    let after = "URL = \"http://localhost/health\"\nfrom config import *\nimport requests\ndef run():\n    requests.get(URL)\n";
    assert_eq!(client_paths(after), ["GET ?"]);
    let before = "from config import *\nimport requests\nURL = \"http://localhost/health\"\ndef run():\n    requests.get(URL)\n";
    assert_eq!(client_paths(before), ["GET /health"]);
    let local = "import requests\nURL = \"http://localhost/health\"\nclass C:\n    from config import *\ndef run():\n    requests.get(URL)\n";
    assert!(client_paths(local).is_empty());
    let object = "from flask import Flask\napp = Flask(__name__)\nfrom views import *\n@app.route(\"/x\")\ndef x():\n    return \"\"\n";
    assert!(routes(&[("svc/app.py", object)]).is_empty());
}

/// f-strings and `%`/`.format` formatting: a format specification or conversion makes that
/// substitution unknown, never its raw value.
#[test]
fn a_format_specification_is_a_substitution_not_its_value() {
    let client = r#"
import requests
ID = "7"
BASE = "http://localhost"
def run(d):
    requests.get(f"{BASE}/n/{ID:03}")
    requests.get(f"{BASE}/n/{ID!r}")
    requests.get(f"{BASE}/n/{ID=}")
    requests.get("{}/n/{:>3}".format(BASE, ID))
    requests.get("{0}/n/{1!s}".format(BASE, ID))
    requests.get("%s/n/%03s" % (BASE, ID))
    requests.get("%s/n/%r" % (BASE, ID))
    requests.get(f"{BASE:>40}/n")
    requests.get(f"{BASE}/n/{ID}")
    requests.get("%s/n/%s" % (BASE, ID))
    requests.get("{b}/n/{i}".format(b=BASE, i=ID))
    requests.get("%(b)s/n" % {"b": BASE})
    requests.get("%(b)s/n" % {**d, "b": BASE})
    requests.get("%(b)s/n" % {"b": BASE, **d})
    requests.get("{b}/n".format(**d))
    requests.get(f"{{BASE}}/n")
    requests.get("http://localhost" "/implicit")
"#;
    assert_eq!(
        client_paths(client),
        [
            "GET /n/{}",
            "GET /n/{}",
            "GET /n/{}",
            "GET /n/{}",
            "GET /n/{}",
            "GET /n/{}",
            "GET /n/{}",
            "GET ?",
            "GET /n/7",
            "GET /n/7",
            "GET /n/7",
            "GET /n",
            "GET /n",
            "GET ?",
            "GET ?",
            "GET ?",
            "GET /implicit",
        ]
    );
}

/// An expansion is charged to the work budget before it is built: a string that repeats a
/// constant expands past the budget although its source is small.
#[test]
fn a_string_expansion_over_the_budget_is_typed_bounded_work() {
    let p = "a".repeat(1000);
    for template in [
        format!("f\"http://h/{}\"", "{P}".repeat(50)),
        format!("\"http://h/{}\".format(P)", "{0}".repeat(50)),
        format!("\"http://h/{}\" % ({})", "%s".repeat(50), "P, ".repeat(50)),
    ] {
        let client =
            format!("import requests\nP = \"{p}\"\ndef f():\n    requests.get({template})\n");
        let shot = snapshot(&[("cli/client.py", &client)]);
        let error = extract_with(&shot, &mut Work::new(client.len() * 10))
            .err()
            .unwrap();
        assert_eq!(error.to_string(), BUDGET, "{template}");
        let found = extract_with(&shot, &mut Work::new(usize::MAX)).unwrap();
        assert_eq!(
            found.calls[0].path_template.as_ref().unwrap().len(),
            1 + 50 * 1000,
            "{template}"
        );
    }
}

/// Cancellation is checked for each substitution, not only for each syntax node.
#[test]
fn each_substitution_checks_cancellation() {
    let checks = |count: usize| {
        let client = format!(
            "import requests\nP = \"p\"\ndef f():\n    requests.get(\"http://h/{}\".format(P))\n",
            "{0}".repeat(count)
        );
        let shot = snapshot(&[("cli/client.py", &client)]);
        let mut checked = 0usize;
        extract(
            &shot,
            &[],
            &Default::default(),
            &mut Work::new(usize::MAX),
            &mut || {
                checked += 1;
                Ok(())
            },
        )
        .unwrap();
        checked
    };
    assert!(checks(301) >= checks(1) + 300);
}

/// Fan-out: the blueprint registrations times its routes are charged before they are built.
#[test]
fn the_mount_and_route_fan_out_is_charged() {
    let app = |mounts: usize| {
        let mut source = "from flask import Flask, Blueprint\napp = Flask(__name__)\nbp = Blueprint(\"bp\", __name__)\n".to_owned();
        for n in 0..mounts {
            source.push_str(&format!(
                "app.register_blueprint(bp, url_prefix=\"/m{n}\", name=\"b{n}\")\n"
            ));
        }
        for n in 0..30 {
            source.push_str(&format!(
                "@bp.route(\"/r{n}\")\ndef r{n}():\n    return \"\"\n"
            ));
        }
        source
    };
    let one = app(1);
    let shot = snapshot(&[("svc/app.py", &one)]);
    let found = extract_with(&shot, &mut Work::new(one.len() * 12)).unwrap();
    assert_eq!(found.routes.len(), 30);
    let many = app(30);
    let shot = snapshot(&[("svc/app.py", &many)]);
    let error = extract_with(&shot, &mut Work::new(many.len() * 12))
        .err()
        .unwrap();
    assert_eq!(error.to_string(), BUDGET);
    let found = extract_with(&shot, &mut Work::new(usize::MAX)).unwrap();
    assert_eq!(found.routes.len(), 900);
}

/// Fan-out: a Django pattern list included many times is charged per inclusion.
#[test]
fn the_django_include_fan_out_is_charged() {
    let urls = |includes: usize| {
        let mut source =
            "from django.urls import include, path\nfrom . import views\ninner = [\n".to_owned();
        for n in 0..30 {
            source.push_str(&format!("    path(\"r{n}/\", views.v),\n"));
        }
        source.push_str("]\nurlpatterns = [\n");
        for n in 0..includes {
            source.push_str(&format!("    path(\"m{n}/\", include(inner)),\n"));
        }
        source.push_str("]\n");
        source
    };
    let views =
        "from django.http import HttpResponse\ndef v(request):\n    return HttpResponse(\"\")\n";
    for (includes, fits) in [(1, true), (30, false)] {
        let source = urls(includes);
        let files = [
            ("site/settings.py", "ROOT_URLCONF = \"urls\"\n"),
            ("site/urls.py", source.as_str()),
            ("site/views.py", views),
        ];
        let shot = snapshot(&files);
        let size: usize = files.iter().map(|(_, text)| text.len()).sum();
        let outcome = extract_with(&shot, &mut Work::new(size * 12));
        if fits {
            assert_eq!(outcome.unwrap().routes.len(), 30);
        } else {
            assert_eq!(outcome.err().unwrap().to_string(), BUDGET);
            let found = extract_with(&shot, &mut Work::new(usize::MAX)).unwrap();
            assert_eq!(found.routes.len(), 900);
        }
    }
}

/// Each read-time scan charges work: the candidate scan of every production Python
/// document, then the parse and the walks of each candidate.
#[test]
fn every_python_scan_is_charged_and_an_exhausted_budget_is_typed() {
    let plain = "def f():\n    return 1\n";
    let shot = snapshot(&[("lib/plain.py", plain)]);
    let mut work = Work::new(plain.len() - 1);
    let error = super::python::Sources::new(&shot, &mut work, &mut || Ok(()))
        .err()
        .unwrap();
    assert_eq!(error.to_string(), BUDGET);
    let mut work = Work::new(plain.len());
    super::python::Sources::new(&shot, &mut work, &mut || Ok(())).unwrap();
    // A candidate is charged again to be parsed.
    let shot = snapshot(&[("cli/client.py", CLIENT)]);
    let mut work = Work::new(2 * CLIENT.len() - 1);
    let error = super::python::Sources::new(&shot, &mut work, &mut || Ok(()))
        .err()
        .unwrap();
    assert_eq!(error.to_string(), BUDGET);
    // And the walks of the bindings, routes and calls are charged too.
    let mut files = flask();
    files.push(("cli/client.py", CLIENT));
    let shot = snapshot(&files);
    let mut work = Work::new(usize::MAX);
    extract_with(&shot, &mut work).unwrap();
    let used = work.used;
    let error = extract_with(&shot, &mut Work::new(used - 1)).err().unwrap();
    assert_eq!(error.to_string(), BUDGET);
    // A candidate with many nodes and nothing to read: beyond its scan and parse, the walks
    // of its nodes are charged.
    let nodes = format!("import requests\nx = [{}]\n", "1,".repeat(2000));
    let shot = snapshot(&[("cli/nodes.py", &nodes)]);
    let mut work = Work::new(usize::MAX);
    extract_with(&shot, &mut work).unwrap();
    assert!(
        work.used >= 2 * nodes.len() + 2000,
        "{} {}",
        work.used,
        nodes.len()
    );
}

/// Cross-file mounts follow only the bindings that resolve: a same-named object elsewhere is
/// not the one registered, and a name bound inside a function is not a module attribute.
#[test]
fn cross_file_registrations_follow_only_resolved_bindings() {
    let app = r#"
from flask import Flask
def create_app():
    app = Flask(__name__)
    from .views import bp
    app.register_blueprint(bp)
    return app
"#;
    let views = r#"
from flask import Blueprint
bp = Blueprint("views", __name__, url_prefix="/v")
@bp.route("/kept")
def kept():
    return ""
"#;
    let other = r#"
from flask import Blueprint
bp = Blueprint("other", __name__)
@bp.route("/other")
def other():
    return ""
"#;
    let factory = r#"
from flask import Flask
def make():
    app = Flask(__name__)
    return app
"#;
    let decorates = r#"
from .factory import app
@app.route("/local")
def local():
    return ""
"#;
    assert_eq!(
        routes(&[
            ("svc/app.py", app),
            ("svc/views.py", views),
            ("other/views.py", other),
            ("svc/factory.py", factory),
            ("svc/decorates.py", decorates),
        ]),
        ["GET /v/kept <- svc/views.py:kept"]
    );
    // An absolute import that two directories above the importer could satisfy is
    // ambiguous, and registers nothing.
    let absolute = r#"
from flask import Flask
from pkg.views import bp
app = Flask(__name__)
app.register_blueprint(bp)
"#;
    let files = [
        ("svc/app.py", absolute),
        ("svc/pkg/views.py", views),
        ("pkg/views.py", views),
    ];
    assert!(routes(&files).is_empty());
    assert_eq!(
        routes(&files[..2]),
        ["GET /v/kept <- svc/pkg/views.py:kept"]
    );
}

/// An httpx base URL with a path is a prefix the client adds, so its match is
/// `wrapperInferred`.
#[test]
fn an_httpx_base_url_path_infers_the_match() {
    let mut files = fastapi();
    files.push((
        "cli/client.py",
        "import httpx\nc = httpx.Client(base_url=\"http://h/v1/items\")\ndef f():\n    c.get(\"/7\")\n",
    ));
    let (_, found) = run(&files);
    assert_eq!(
        found
            .matched
            .iter()
            .map(|m| m.map(|m| m.confidence))
            .collect::<Vec<_>>(),
        [Some(Confidence::WrapperInferred)]
    );
}

/// #877 review: an httpx base URL with any unreadable part, here its path, is unreadable
/// whole, so a relative call through it is unresolved, never a joined `{}` prefix.
#[test]
fn a_partly_unreadable_httpx_base_url_is_unreadable() {
    let server = r#"
from fastapi import FastAPI
app = FastAPI()

@app.get("/{part}/users")
def users(part):
    return {}
"#;
    let client = r#"
import httpx
BASE = "http://h/api"
def run(prefix):
    httpx.Client(base_url=f"http://h/{prefix}").get("/users")
    httpx.Client(base_url="http://h/api").get("/users")
    httpx.Client(base_url=BASE).get("/users")
    httpx.Client(base_url=f"http://h/{prefix}").get("http://h/api/users")
"#;
    assert_eq!(
        calls(&[("api/main.py", server), ("cli/client.py", client)]),
        [
            "GET ? -> unresolved",
            "GET /api/users -> /{part}/users WrapperInferred",
            "GET /api/users -> /{part}/users WrapperInferred",
            "GET /api/users -> /{part}/users Template",
        ]
    );
}

fn django_urls(urls: &str, child: Option<&str>) -> Vec<String> {
    let views = "from django.http import HttpResponse\ndef v(request, part=None):\n    return HttpResponse(\"\")\n";
    let mut files = vec![
        ("site/settings.py", "ROOT_URLCONF = \"urls\"\n"),
        ("site/urls.py", urls),
        ("site/views.py", views),
    ];
    if let Some(child) = child {
        files.push(("site/child.py", child));
    }
    routes(&files)
        .into_iter()
        .map(|route| route.split(" <- ").next().unwrap().to_owned())
        .collect()
}

/// #877 review: a named group whose body may match `/` without a literal slash is
/// unreadable, never a one-segment `{name}`. Bodies that provably stay in one segment are
/// read.
#[test]
fn a_named_group_that_may_match_a_slash_is_unreadable() {
    let urls = r#"
from django.urls import re_path
from . import views
urlpatterns = [
    re_path(r"^a/(?P<part>[^x]+)/done$", views.v),
    re_path(r"^b/(?P<part>a.*b)/done$", views.v),
    re_path(r"^c/(?P<part>[!-0]+)/done$", views.v),
    re_path(r"^d/(?P<part>[^/]+)/done$", views.v),
    re_path(r"^e/(?P<part>[0-9]+)/done$", views.v),
    re_path(r"^f/(?P<part>[\w-]+)/done$", views.v),
]
"#;
    assert_eq!(
        django_urls(urls, None),
        [
            "ANY /d/{part}/done",
            "ANY /e/{part}/done",
            "ANY /f/{part}/done",
        ]
    );
}

/// #877 review: a regex that ends in `$` matches only when nothing remains, so a pattern
/// list it includes is not reached past it. Without the anchor it is.
#[test]
fn an_anchored_regex_mounts_no_include() {
    let child =
        "from django.urls import path\nfrom . import views\nurlpatterns = [path(\"x\", views.v)]\n";
    let anchored = "from django.urls import include, re_path\nurlpatterns = [re_path(r\"^api/$\", include(\"child\"))]\n";
    assert!(django_urls(anchored, Some(child)).is_empty());
    let open = "from django.urls import include, re_path\nurlpatterns = [re_path(r\"^api/\", include(\"child\"))]\n";
    assert_eq!(django_urls(open, Some(child)), ["ANY /api/x"]);
    // An escaped `$` is a literal, not an anchor.
    let escaped = "from django.urls import include, re_path\nurlpatterns = [re_path(r\"^api\\$\", include(\"child\"))]\n";
    assert_eq!(django_urls(escaped, Some(child)), ["ANY /api$x"]);
}
