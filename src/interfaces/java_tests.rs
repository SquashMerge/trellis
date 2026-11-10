//! #857: Java server routes (Spring MVC, JAX-RS) and client calls (`java.net.http`,
//! `RestTemplate`, `WebClient`), one fixture per framework and client form, and a test for
//! each way a route or call must not be invented.
use super::{ClientPrefix, Extraction, HandlerKind, extract};
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

fn run(files: &[(&str, &str)]) -> Extraction {
    extract_with(&snapshot(files), &mut Work::new(usize::MAX)).unwrap()
}

/// `METHOD /template` of each route, in route order.
fn routes(found: &Extraction) -> Vec<String> {
    found
        .routes
        .iter()
        .map(|r| format!("{} {}", r.method, r.path_template))
        .collect()
}

fn route_table(files: &[(&str, &str)]) -> Vec<String> {
    routes(&run(files))
}

/// `METHOD path -> route confidence` of each call, in call order.
fn outcomes(found: &Extraction) -> Vec<String> {
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

/// `METHOD path` of each call of a client document, with no server.
fn calls(client: &str) -> Vec<String> {
    run(&[("client/src/main/java/c/Client.java", client)])
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

const SPRING: &str = r#"package com.example.api;

import org.springframework.web.bind.annotation.*;

@RestController
@RequestMapping("/api/users")
public class UserController {
    static final String BY_ID = "/{id}";

    @GetMapping
    public List<User> list() { return null; }

    @GetMapping("/{id:\\d+}")
    public User show(@PathVariable long id) { return null; }

    @PostMapping(consumes = "application/json")
    public User create(@RequestBody User user) { return null; }

    @RequestMapping(method = RequestMethod.PUT, value = BY_ID)
    public User update(@PathVariable long id) { return null; }

    @DeleteMapping(path = BY_ID + "/avatar")
    public void dropAvatar(@PathVariable long id) {}

    @RequestMapping(value = {"/search", "/find"}, method = {RequestMethod.GET, RequestMethod.POST})
    public List<User> search() { return null; }

    @RequestMapping("/legacy")
    public void legacy() {}

    public void helper() {}
}
"#;

const SPRING_VERSIONED: &str = r#"package com.example.api;

import org.springframework.stereotype.Controller;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.PatchMapping;
import org.springframework.web.bind.annotation.RequestMapping;

@Controller
@RequestMapping({"/v1", "/v2/"})
class StatusController {
    @GetMapping("status")
    String status() { return "ok"; }

    @PatchMapping({"/flags/{name}", "/toggles/{name}"})
    void flag() {}

    @GetMapping("/files/{*path}")
    void files() {}
}
"#;

#[test]
fn spring_mappings_join_their_class_prefix_and_expand_every_path_and_method() {
    let found = run(&[
        (
            "api/src/main/java/com/example/api/UserController.java",
            SPRING,
        ),
        (
            "api/src/main/java/com/example/api/StatusController.java",
            SPRING_VERSIONED,
        ),
    ]);
    assert_eq!(
        routes(&found),
        [
            "GET /v1/status",
            "GET /v2/status",
            "PATCH /v1/flags/{name}",
            "PATCH /v1/toggles/{name}",
            "PATCH /v2/flags/{name}",
            "PATCH /v2/toggles/{name}",
            "GET /v1/files/{*path}",
            "GET /v2/files/{*path}",
            "GET /api/users",
            "GET /api/users/{id}",
            "POST /api/users",
            "PUT /api/users/{id}",
            "DELETE /api/users/{id}/avatar",
            "GET /api/users/search",
            "GET /api/users/find",
            "POST /api/users/search",
            "POST /api/users/find",
            "ANY /api/users/legacy",
        ]
    );
    let show = found
        .routes
        .iter()
        .find(|r| r.path_template == "/api/users/{id}" && r.method == "GET")
        .unwrap();
    assert_eq!(show.handler.kind, HandlerKind::Declaration);
    assert_eq!(
        show.handler.stable_lookup_key.as_deref(),
        Some("api/src/main/java/com/example/api/UserController.java::UserController::show")
    );
    assert_eq!(show.handler.lines.start, 13);
}

const JAXRS: &str = r#"package com.example.rs;

import jakarta.ws.rs.GET;
import jakarta.ws.rs.POST;
import jakarta.ws.rs.DELETE;
import jakarta.ws.rs.Path;
import jakarta.ws.rs.PathParam;
import jakarta.ws.rs.Produces;

@Path("items")
public class ItemResource {
    @GET
    public String list() { return ""; }

    @GET
    @Path("{id}")
    public String show(@PathParam("id") String id) { return ""; }

    @POST
    public void create(String body) {}

    @DELETE
    @Path("/{id : [0-9]+}")
    public void remove(@PathParam("id") String id) {}

    @GET
    @Path("files/{rest: .*}")
    public String file() { return ""; }

    @Path("parts")
    public PartResource parts() { return null; }

    @Produces("text/plain")
    public String notAResource() { return ""; }
}
"#;

const JAXRS_APP: &str = r#"package com.example.rs;

import jakarta.ws.rs.ApplicationPath;
import jakarta.ws.rs.core.Application;

@ApplicationPath("/api/*")
public class RestApplication extends Application {}
"#;

#[test]
fn jaxrs_resource_methods_are_routes_under_the_application_path() {
    let item = ("rs/src/main/java/com/example/rs/ItemResource.java", JAXRS);
    assert_eq!(
        route_table(&[item]),
        [
            "GET /items",
            "GET /items/{id}",
            "POST /items",
            "DELETE /items/{id}",
            "GET /items/files/{*rest}",
        ]
    );
    let app = (
        "rs/src/main/java/com/example/rs/RestApplication.java",
        JAXRS_APP,
    );
    assert_eq!(
        route_table(&[item, app]),
        [
            "GET /api/items",
            "GET /api/items/{id}",
            "POST /api/items",
            "DELETE /api/items/{id}",
            "GET /api/items/files/{*rest}",
        ]
    );
    // Two application paths, or one that cannot be read, leave every mount unknown.
    let other = (
        "rs/src/main/java/com/example/rs/Other.java",
        "package com.example.rs;\nimport javax.ws.rs.ApplicationPath;\n@ApplicationPath(\"/other\")\nclass Other extends javax.ws.rs.core.Application {}\n",
    );
    assert!(route_table(&[item, app, other]).is_empty());
    let unreadable = (
        "rs/src/main/java/com/example/rs/Other.java",
        "package com.example.rs;\nimport javax.ws.rs.ApplicationPath;\n@ApplicationPath(Config.ROOT)\nclass Other extends javax.ws.rs.core.Application {}\n",
    );
    assert!(route_table(&[item, unreadable]).is_empty());
}

const CLIENT: &str = r#"package com.example.client;

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import org.springframework.http.HttpMethod;
import org.springframework.web.client.RestTemplate;
import org.springframework.boot.web.client.RestTemplateBuilder;
import org.springframework.web.reactive.function.client.WebClient;

public class ApiClient {
    private static final String HOST = "http://localhost:8080";
    private final HttpClient http = HttpClient.newHttpClient();
    private final RestTemplate rest = new RestTemplate();
    private final RestTemplate rooted = new RestTemplateBuilder().rootUri("http://localhost:8080/api").build();
    private final WebClient web = WebClient.create("http://localhost:8080");
    private final WebClient api = WebClient.builder().baseUrl(HOST + "/api").build();
    private final WebClient injected;

    public ApiClient(WebClient injected) {
        this.injected = injected;
    }

    public void calls(long id, String host) throws Exception {
        http.send(HttpRequest.newBuilder().uri(URI.create(HOST + "/api/users")).build(), null);
        http.send(HttpRequest.newBuilder(URI.create(HOST + "/api/users/" + id + "/avatar")).DELETE().build(), null);
        http.send(HttpRequest.newBuilder().uri(new URI(String.format("%s/api/users/%d", HOST, id))).PUT(null).build(), null);
        http.send(HttpRequest.newBuilder().method("POST", null).uri(URI.create("http://" + host + "/api/users")).build(), null);
        rest.getForObject(HOST + "/api/users/{id}", String.class, id);
        rest.postForEntity(HOST + "/api/users", null, String.class);
        rest.exchange("%s/api/users/{id}".formatted(HOST), HttpMethod.PUT, null, String.class, id);
        rest.delete(URI.create(HOST + "/api/users/7/avatar"));
        rooted.getForObject("/users", String.class);
        web.get().uri("/api/users/{id}", id).retrieve();
        web.method(HttpMethod.POST).uri("/api/users").retrieve();
        api.get().uri("/users").retrieve();
        injected.get().uri("/api/users").retrieve();
        injected.get().uri(HOST + "/api/users").retrieve();
        rest.getForObject(HOST + "/api/missing", String.class);
        web.get().uri(builder -> builder.path("/api/users").build()).retrieve();
        rest.exchange(HOST + "/api/users", verb(), null, String.class);
    }
}
"#;

#[test]
fn each_java_client_form_matches_spring_routes_with_and_without_a_base_url() {
    let found = run(&[
        (
            "api/src/main/java/com/example/api/UserController.java",
            SPRING,
        ),
        (
            "client/src/main/java/com/example/client/ApiClient.java",
            CLIENT,
        ),
    ]);
    assert_eq!(
        outcomes(&found),
        [
            "GET /api/users -> /api/users Exact",
            "DELETE /api/users/{}/avatar -> /api/users/{id}/avatar Template",
            "PUT /api/users/{} -> /api/users/{id} Template",
            "POST /api/users -> /api/users Exact",
            "GET /api/users/{} -> /api/users/{id} Template",
            "POST /api/users -> /api/users Exact",
            "PUT /api/users/{} -> /api/users/{id} Template",
            "DELETE /api/users/7/avatar -> /api/users/{id}/avatar Template",
            "GET /api/users -> /api/users WrapperInferred",
            "GET /api/users/{} -> /api/users/{id} Template",
            "POST /api/users -> /api/users Exact",
            "GET /api/users -> /api/users WrapperInferred",
            "GET ? -> unresolved",
            "GET /api/users -> /api/users Exact",
            "GET /api/missing -> unresolved",
            "GET ? -> unresolved",
            "? /api/users -> unresolved",
        ]
    );
    let prefixes: Vec<ClientPrefix> = found.calls.iter().map(|call| call.prefix).collect();
    assert_eq!(prefixes[8], ClientPrefix::Joined);
    assert_eq!(prefixes[11], ClientPrefix::Joined);
    assert!(
        prefixes
            .iter()
            .enumerate()
            .all(|(at, prefix)| matches!(at, 8 | 11) || *prefix == ClientPrefix::Written)
    );
    let first = &found.calls[0];
    assert_eq!(first.lines.start, 26);
    assert!(first.call_id.is_some());
    let caller = serde_json::to_value(first.caller.as_ref().unwrap()).unwrap();
    assert_eq!(caller["name"], "calls");
}

/// Both one-sided directions: a route no call reaches, and a call that reaches no route.
#[test]
fn unmatched_java_routes_and_calls_are_kept_on_both_sides() {
    let client = r#"package c;
import org.springframework.web.client.RestTemplate;
class C {
    void run(RestTemplate rest) {
        rest.getForObject("http://localhost/api/users", String.class);
        rest.getForObject("http://localhost/api/orders", String.class);
    }
}
"#;
    let found = run(&[
        (
            "api/src/main/java/com/example/api/UserController.java",
            SPRING,
        ),
        ("client/src/main/java/c/C.java", client),
    ]);
    assert_eq!(
        outcomes(&found),
        [
            "GET /api/users -> /api/users Exact",
            "GET /api/orders -> unresolved",
        ]
    );
    let reached: Vec<usize> = found.matched.iter().flatten().map(|m| m.route).collect();
    let unreached = (0..found.routes.len())
        .filter(|route| !reached.contains(route))
        .count();
    assert_eq!(unreached, found.routes.len() - 1);
}

#[test]
fn code_that_is_not_a_controller_or_a_client_is_neither() {
    let feign = r#"package com.example.api;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.cloud.openfeign.FeignClient;
@FeignClient("users")
public interface UsersClient {
    @GetMapping("/api/users")
    String list();
}
"#;
    let plain = r#"package com.example.api;
import org.springframework.web.bind.annotation.GetMapping;
public class NotAController {
    @GetMapping("/plain")
    public String plain() { return ""; }
}
"#;
    let own_annotation = r#"package com.example.api;
@RestController
public class OwnAnnotations {
    @GetMapping("/own")
    public String own() { return ""; }
}
"#;
    let abstract_base = r#"package com.example.api;
import org.springframework.web.bind.annotation.*;
@RestController
public abstract class AbstractBase {
    @GetMapping("/abstract")
    public String base() { return ""; }
}
"#;
    let test_code = r#"package com.example.api;
import org.springframework.web.bind.annotation.*;
@RestController
public class FixtureController {
    @GetMapping("/fixture")
    public String fixture() { return ""; }
}
"#;
    let client_test = r#"package c;
import org.springframework.web.client.RestTemplate;
class ClientTest {
    void run(RestTemplate rest) { rest.getForObject("http://localhost/api/users", String.class); }
}
"#;
    let lookalikes = r#"package c;
import java.util.Map;
import org.springframework.web.client.RestTemplate;
class Lookalikes {
    void run(Map<String, String> map, Cache web, RestTemplate rest) {
        map.get("/api/users");
        web.get().uri("/api/users");
        restTemplate.getForObject("http://localhost/api/users", String.class);
        rest.send("http://localhost/api/users");
        java.net.http.HttpRequest.newBuilder().uri(java.net.URI.create("/x"));
        Builder.newBuilder().uri(java.net.URI.create("/y")).build();
    }
}
"#;
    let found = run(&[
        ("api/src/main/java/com/example/api/UsersClient.java", feign),
        (
            "api/src/main/java/com/example/api/NotAController.java",
            plain,
        ),
        (
            "api/src/main/java/com/example/api/OwnAnnotations.java",
            own_annotation,
        ),
        (
            "api/src/main/java/com/example/api/AbstractBase.java",
            abstract_base,
        ),
        (
            "api/src/test/java/com/example/api/FixtureController.java",
            test_code,
        ),
        ("client/src/test/java/c/ClientTest.java", client_test),
        ("client/src/main/java/c/Lookalikes.java", lookalikes),
    ]);
    assert!(found.routes.is_empty(), "{:?}", routes(&found));
    assert!(found.calls.is_empty(), "{:?}", outcomes(&found));
}

/// Review lesson: a class-level mapping whose value cannot be read registers nothing under
/// it; it never falls back to the root.
#[test]
fn an_unreadable_class_prefix_registers_nothing() {
    for class_mapping in [
        "@RequestMapping(Paths.USERS)",
        "@RequestMapping(\"/api\" + version())",
        "@RequestMapping(value = \"/a\", path = \"/b\")",
        "@RequestMapping(value = \"/api\", method = RequestMethod.GET)",
        "@RequestMapping(\"/a\") @RequestMapping(\"/b\")",
    ] {
        let source = format!(
            "package a;\nimport org.springframework.web.bind.annotation.*;\n@RestController\n{class_mapping}\nclass C {{\n    @GetMapping(\"/users\") String users() {{ return \"\"; }}\n}}\n"
        );
        assert!(
            route_table(&[("src/main/java/a/C.java", &source)]).is_empty(),
            "{class_mapping}"
        );
    }
}

/// Review lesson: a composed or meta-annotation is not resolved, so it registers nothing.
#[test]
fn composed_and_meta_annotations_register_nothing() {
    let composed = r#"package a;
import org.springframework.web.bind.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@GetMapping
@interface GetJson { String value() default ""; }
@RestController
class C {
    @GetJson("/json") String json() { return ""; }
    @GetMapping("/plain") String plain() { return ""; }
}
"#;
    assert_eq!(
        route_table(&[("src/main/java/a/C.java", composed)]),
        ["GET /plain"]
    );
    let stereotype = r#"package a;
import org.springframework.web.bind.annotation.*;
@RestController
@interface ApiController {}
@ApiController
class D {
    @GetMapping("/composed") String composed() { return ""; }
}
"#;
    assert!(route_table(&[("src/main/java/a/D.java", stereotype)]).is_empty());
    let jaxrs = r#"package a;
import javax.ws.rs.*;
@HttpMethod("PATCH")
@interface Patchy {}
@Path("/r")
public class R {
    @Patchy public void patch() {}
    @GET @GET public void twice() {}
}
"#;
    assert!(route_table(&[("src/main/java/a/R.java", jaxrs)]).is_empty());
}

/// Review lesson: a `method = ..` that cannot be read is an unknown method, which
/// registers nothing; it never becomes `ANY`.
#[test]
fn an_unreadable_request_method_registers_nothing() {
    for method in [
        "methods()",
        "GET",
        "RequestMethod.TRACE",
        "{RequestMethod.GET, verb}",
        "{}",
        "Other.RequestMethod.GET",
    ] {
        let source = format!(
            "package a;\nimport org.springframework.web.bind.annotation.*;\n@RestController\nclass C {{\n    @RequestMapping(value = \"/x\", method = {method}) String x() {{ return \"\"; }}\n}}\n"
        );
        assert!(
            route_table(&[("src/main/java/a/C.java", &source)]).is_empty(),
            "{method}"
        );
    }
    let own_type = "package a;\nimport org.springframework.web.bind.annotation.*;\nenum RequestMethod { GET }\n@RestController\nclass C {\n    @RequestMapping(value = \"/x\", method = RequestMethod.GET) String x() { return \"\"; }\n}\n";
    assert!(route_table(&[("src/main/java/a/C.java", own_type)]).is_empty());
}

/// Review lesson: only a `static final String` constant visible at the use site, and bound
/// by nothing else in the document, is read.
#[test]
fn a_constant_is_read_only_where_it_is_visible_and_unshadowed() {
    let read = |body: &str| {
        let source =
            format!("package c;\nimport org.springframework.web.client.RestTemplate;\n{body}\n");
        calls(&source)
    };
    let call = "void run(RestTemplate rest) { rest.getForObject(URL, String.class); }";
    let constant = "static final String URL = \"http://localhost/api/users\";";
    assert_eq!(
        read(&format!("class C {{ {constant} {call} }}")),
        ["GET /api/users"]
    );
    for (body, why) in [
        (
            format!(
                "interface Paths {{ String URL = \"http://localhost/api/users\"; }}\nclass C implements Paths {{ {call} }}"
            ),
            "an inherited constant",
        ),
        (
            format!("class C {{ final String URL = \"http://localhost/api/users\"; {call} }}"),
            "not static",
        ),
        (
            format!("class C {{ static String URL = \"http://localhost/api/users\"; {call} }}"),
            "not final",
        ),
        (
            format!(
                "class C {{ static final Object URL = \"http://localhost/api/users\"; {call} }}"
            ),
            "not a String",
        ),
        (
            format!(
                "class C {{ static final String URL = \"http://localhost/api/\\u0075sers\"; {call} }}"
            ),
            "an escape",
        ),
        (
            format!("class Other {{ {constant} }}\nclass C {{ {call} }}"),
            "another top-level class",
        ),
        (
            format!("class C {{ {constant} void other() {{ String URL = \"x\"; }} {call} }}"),
            "a local",
        ),
        (
            format!("class C {{ {constant} void other(String URL) {{}} {call} }}"),
            "a parameter",
        ),
        (
            format!(
                "class C {{ {constant} static class Inner {{ static final String URL = \"http://localhost/other\"; }} {call} }}"
            ),
            "a field of an inner class",
        ),
        (
            format!("import static c.Config.URL;\nclass C {{ {constant} {call} }}"),
            "a static import",
        ),
        (
            format!("class C {{ {constant} class Inner extends Base {{ {call} }} }}"),
            "an inner class with a supertype",
        ),
        (
            format!(
                "class C {{ {constant} Runnable r = new Runnable() {{ public void run() {{}} {call} }}; }}"
            ),
            "an anonymous class",
        ),
        (
            format!(
                "class C {{ {constant} void f(Object o) {{ if (o instanceof String URL) {{}} }} {call} }}"
            ),
            "a pattern variable",
        ),
        (
            format!(
                "class C {{ {constant} void f() {{ java.util.List.of().forEach(URL -> {{}}); }} {call} }}"
            ),
            "a lambda parameter",
        ),
    ] {
        assert_eq!(read(&body), ["GET ?"], "{why}");
    }
    // A class's own constant is not in scope in the class's own annotations.
    let own = r#"package a;
import org.springframework.web.bind.annotation.*;
@RestController
@RequestMapping(BASE)
class C {
    static final String BASE = "/api";
    @GetMapping(BASE + "/users") String users() { return ""; }
}
"#;
    assert!(route_table(&[("src/main/java/a/C.java", own)]).is_empty());
    let outer = r#"package a;
import org.springframework.web.bind.annotation.*;
class Outer {
    static final String BASE = "/api";
    @RestController
    @RequestMapping(BASE)
    static class C {
        @GetMapping(BASE + "/users") String users() { return ""; }
    }
}
"#;
    assert_eq!(
        route_table(&[("src/main/java/a/Outer.java", outer)]),
        ["GET /api/api/users"]
    );
}

/// Review lesson: a format specification makes its substitution unknown.
#[test]
fn a_format_specification_is_a_substitution_not_its_value() {
    let client = r#"package c;
import org.springframework.web.client.RestTemplate;
class C {
    static final String HOST = "http://localhost";
    static final String ID = "7";
    void run(RestTemplate rest) {
        rest.getForObject(String.format("%s/n/%s", HOST, ID), String.class);
        rest.getForObject(String.format("%s/n/%5s", HOST, ID), String.class);
        rest.getForObject(String.format("%s/n/%1$s", HOST, ID), String.class);
        rest.getForObject(String.format("%s/n/%S", HOST, ID), String.class);
        rest.getForObject(String.format("%s/n/%d/%s", HOST, 1, ID), String.class);
        rest.getForObject("%s/n/%-3s".formatted(HOST, ID), String.class);
        rest.getForObject(String.format("%10s/n", HOST), String.class);
        rest.getForObject(String.format(prefix(), HOST), String.class);
        rest.getForObject(String.format("%s/n/%%", HOST), String.class);
    }
}
"#;
    assert_eq!(
        calls(client),
        [
            "GET /n/7",
            "GET /n/{}",
            "GET /n/{}",
            "GET /n/{}",
            "GET /n/{}/7",
            "GET /n/{}",
            "GET ?",
            "GET ?",
            "GET /n/%",
        ]
    );
}

/// Review lesson: a concatenation that repeats a constant expands past the budget although
/// its source is small. Each expansion is charged before it is copied.
#[test]
fn a_string_expansion_over_the_budget_is_typed_bounded_work() {
    let client = format!(
        "package c;\nimport org.springframework.web.client.RestTemplate;\nclass C {{\n    static final String P = \"{}\";\n    void run(RestTemplate rest) {{ rest.getForObject(\"http://h/\" {}, String.class); }}\n}}\n",
        "a".repeat(1000),
        "+ P ".repeat(50)
    );
    let shot = snapshot(&[("client/src/main/java/c/C.java", &client)]);
    let error = extract_with(&shot, &mut Work::new(client.len() * 4))
        .err()
        .unwrap();
    assert_eq!(error.to_string(), super::BUDGET);
    let found = extract_with(&shot, &mut Work::new(usize::MAX)).unwrap();
    assert_eq!(
        found.calls[0].path_template.as_ref().unwrap().len(),
        1 + 50 * 1000
    );
}

/// Review lesson: class paths x method paths x methods is charged before it is built.
#[test]
fn the_class_and_method_mapping_fan_out_is_charged() {
    let paths = |prefix: &str| {
        (0..40)
            .map(|n| format!("\"/{prefix}{n}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let source = format!(
        "package a;\nimport org.springframework.web.bind.annotation.*;\n@RestController\n@RequestMapping({{{}}})\nclass C {{\n    @RequestMapping(value = {{{}}}, method = {{RequestMethod.GET, RequestMethod.POST, RequestMethod.PUT}})\n    String x() {{ return \"\"; }}\n}}\n",
        paths("class"),
        paths("method")
    );
    let shot = snapshot(&[("src/main/java/a/C.java", &source)]);
    let found = extract_with(&shot, &mut Work::new(usize::MAX)).unwrap();
    assert_eq!(found.routes.len(), 40 * 40 * 3);
    let error = extract_with(&shot, &mut Work::new(source.len() * 6))
        .err()
        .unwrap();
    assert_eq!(error.to_string(), super::BUDGET);
}

/// Review lesson: each read-time scan charges the budget, and a stopped check ends it.
#[test]
fn every_java_scan_is_charged_and_a_stopped_check_ends_extraction() {
    let shot = snapshot(&[
        (
            "api/src/main/java/com/example/api/UserController.java",
            SPRING,
        ),
        (
            "client/src/main/java/com/example/client/ApiClient.java",
            CLIENT,
        ),
    ]);
    // A parse and a bindings scan of each document, then a route scan and a call scan.
    let floor = 3 * (SPRING.len() + CLIENT.len());
    for limit in [SPRING.len() - 1, SPRING.len() + CLIENT.len(), floor] {
        let error = super::java::extract(&shot, &mut Work::new(limit), &mut || Ok(()))
            .err()
            .unwrap();
        assert_eq!(error.to_string(), super::BUDGET, "{limit}");
    }
    let error = super::java::extract(&shot, &mut Work::new(usize::MAX), &mut || {
        anyhow::bail!("stopped")
    })
    .err()
    .unwrap();
    assert_eq!(error.to_string(), "stopped");
}

const USERS_API: &str = r#"package com.example.contract;

import org.springframework.web.bind.annotation.*;

@RequestMapping("/api/users")
public interface UsersApi {
    @GetMapping("/{id}")
    String show(@PathVariable("id") long id);

    @PostMapping
    String create(String body);
}
"#;

const BASE_CONTROLLER: &str = r#"package com.example.api;

import org.springframework.web.bind.annotation.*;

public abstract class BaseController {
    @GetMapping("/health")
    public String health() { return "ok"; }
}
"#;

const IMPLEMENTING: &str = r#"package com.example.api;

import com.example.contract.UsersApi;
import org.springframework.web.bind.annotation.RestController;

@RestController
public class UsersController extends BaseController implements UsersApi {
    @Override
    public String show(long id) { return ""; }

    @Override
    public String create(String body) { return ""; }
}
"#;

/// Review lesson: inheritance is followed only through resolved bindings: the same
/// document, a single-type import, or the same package.
#[test]
fn inherited_mappings_are_followed_only_through_resolved_supertypes() {
    let contract = (
        "contract/src/main/java/com/example/contract/UsersApi.java",
        USERS_API,
    );
    let base = (
        "api/src/main/java/com/example/api/BaseController.java",
        BASE_CONTROLLER,
    );
    let controller = "api/src/main/java/com/example/api/UsersController.java";
    let found = run(&[contract, base, (controller, IMPLEMENTING)]);
    assert_eq!(
        routes(&found),
        [
            "GET /api/users/{id}",
            "POST /api/users",
            "GET /api/users/health"
        ]
    );
    let show = &found.routes[0].handler;
    assert_eq!(
        show.stable_lookup_key.as_deref(),
        Some("api/src/main/java/com/example/api/UsersController.java::UsersController::show")
    );
    let health = &found.routes[2].handler;
    assert_eq!(
        health.document.path,
        "api/src/main/java/com/example/api/BaseController.java"
    );

    // An on-demand import does not resolve: the class prefix and inherited mappings are
    // unknown, so nothing registers, and nothing falls back to the root.
    let wildcard = IMPLEMENTING.replace(
        "import com.example.contract.UsersApi;",
        "import com.example.contract.*;",
    );
    assert!(route_table(&[contract, base, (controller, &wildcard)]).is_empty());

    // A supertype found only in a file of another package is unresolved.
    let moved = USERS_API.replace("com.example.contract", "com.example.other");
    let unimported = IMPLEMENTING
        .replace("import com.example.contract.UsersApi;\n", "")
        .replace(" extends BaseController", "");
    assert!(
        route_table(&[
            (
                "contract/src/main/java/com/example/other/UsersApi.java",
                &moved
            ),
            (controller, &unimported),
        ])
        .is_empty()
    );

    // The controller's own mappings stay known under its own prefix. A mapping that an
    // unresolved supertype could decide first (Spring searches interfaces before the
    // superclass) does not count, even when a resolved supertype declares one.
    let own = r#"package com.example.api;

import org.springframework.web.bind.annotation.*;

@RestController
@RequestMapping("/own")
public class OwnController extends BaseController implements Unknown {
    @GetMapping("/mine")
    public String mine() { return ""; }
}
"#;
    let own_path = "api/src/main/java/com/example/api/OwnController.java";
    assert_eq!(route_table(&[base, (own_path, own)]), ["GET /own/mine"]);

    // With no class prefix of its own, an unresolved supertype may carry one: nothing
    // registers, not even the class's own mappings at the root.
    let unprefixed = own.replace("@RequestMapping(\"/own\")\n", "");
    assert!(route_table(&[base, (own_path, &unprefixed)]).is_empty());
}

/// JAX-RS: a method with no JAX-RS annotation of its own inherits its interface's; one
/// with any JAX-RS annotation of its own does not.
#[test]
fn jaxrs_methods_inherit_from_a_resolved_interface() {
    let contract = r#"package com.example.rs;
import javax.ws.rs.*;
public interface Orders {
    @GET @Path("{id}") String show(@PathParam("id") String id);
    @POST String create(String body);
}
"#;
    let resource = r#"package com.example.rs;
import javax.ws.rs.*;
@Path("/orders")
public class OrderResource implements Orders {
    public String show(String id) { return ""; }
    @Produces("text/plain") public String create(String body) { return ""; }
}
"#;
    assert_eq!(
        route_table(&[
            ("rs/src/main/java/com/example/rs/Orders.java", contract),
            (
                "rs/src/main/java/com/example/rs/OrderResource.java",
                resource
            ),
        ]),
        ["GET /orders/{id}"]
    );
}

#[test]
fn deeply_nested_and_malformed_java_does_not_panic() {
    let nested = format!(
        "package a;\nimport org.springframework.web.client.RestTemplate;\nclass C {{ void f(RestTemplate rest) {{ rest.getForObject({}\"http://h/x\"{}, String.class); }} }}\n",
        "(".repeat(3000),
        ")".repeat(3000)
    );
    let chain = format!(
        "package a;\nimport java.net.http.HttpRequest;\nclass C {{ void f() {{ HttpRequest.newBuilder(){}.build(); }} }}\n",
        ".GET()".repeat(5000)
    );
    let concat = format!(
        "package a;\nimport org.springframework.web.client.RestTemplate;\nclass C {{ void f(RestTemplate rest) {{ rest.getForObject(\"http://h/\"{}, String.class); }} }}\n",
        " + \"a\"".repeat(5000)
    );
    let cyclic = "package a;\nimport org.springframework.web.bind.annotation.*;\n@RestController\nclass C extends D { @GetMapping(\"/c\") String c() { return \"\"; } }\nclass D extends C {}\n";
    for source in [
        &nested[..],
        &chain[..],
        &concat[..],
        cyclic,
        "package a; @RestController class { @GetMapping(\"/x\" ",
    ] {
        run(&[("src/main/java/a/C.java", source)]);
    }
    assert_eq!(
        route_table(&[("src/main/java/a/C.java", cyclic)]),
        ["GET /c"]
    );
}

// #876 review: each scenario failed on 8a6a3d96, with a control that held there.

/// #876 review 1: overloads on distinct types that share a simple name are distinct
/// methods, and an override still inherits its own overload's mapping.
#[test]
fn overloads_on_distinct_qualified_types_stay_distinct() {
    let overloads = r#"package a;
import org.springframework.web.bind.annotation.*;
@RestController
class C {
    @GetMapping("/a") public void run(x.Id id) {}
    @PostMapping("/b") public void run(y.Id id) {}
}
"#;
    assert_eq!(
        route_table(&[("src/main/java/a/C.java", overloads)]),
        ["GET /a", "POST /b"]
    );
    let inherited = r#"package a;
import org.springframework.web.bind.annotation.*;
interface Api {
    @GetMapping("/i") void run(x.Id id);
}
@RestController
class D implements Api {
    public void run(y.Id id) {}
    public void run(x.Id id) {}
}
"#;
    let found = run(&[("src/main/java/a/D.java", inherited)]);
    assert_eq!(routes(&found), ["GET /i"]);
    assert_eq!(found.routes[0].handler.lines.start, 9);
}

/// #876 review 2: Spring and JAX-RS templates follow their own framework's rules.
#[test]
fn spring_and_jaxrs_templates_follow_their_own_rules() {
    let spring = r#"package a;
import org.springframework.web.bind.annotation.*;
@RestController
@RequestMapping("/files")
class C {
    @GetMapping("/{name:.+}") String file() { return ""; }
    @GetMapping("/tail/{*rest}") String tail() { return ""; }
    @GetMapping("/*") String star() { return ""; }
}
"#;
    let client = r#"package c;
import org.springframework.web.client.RestTemplate;
class Client {
    void run(RestTemplate rest) {
        rest.getForObject("http://h/files/a/b", String.class);
        rest.getForObject("http://h/files/a", String.class);
    }
}
"#;
    let found = run(&[
        ("api/src/main/java/a/C.java", spring),
        ("client/src/main/java/c/Client.java", client),
    ]);
    assert_eq!(
        routes(&found),
        ["GET /files/{name}", "GET /files/tail/{*rest}"]
    );
    assert_eq!(
        outcomes(&found),
        [
            "GET /files/a/b -> unresolved",
            "GET /files/a -> /files/{name} Template",
        ]
    );
    let jaxrs = r#"package a;
import javax.ws.rs.*;
@Path("/files")
public class R {
    @GET @Path("{name:.+}") public String file() { return ""; }
    @GET @Path("*") public String star() { return ""; }
    @GET @Path("tail/{*rest}") public String tail() { return ""; }
}
"#;
    assert_eq!(
        route_table(&[("src/main/java/a/R.java", jaxrs)]),
        ["GET /files/{*name}", "GET /files/*"]
    );
}

/// #876 review 3: an own composed mapping or prefix decides what it is written on, so it
/// never falls back to an ancestor's mapping or prefix.
#[test]
fn an_own_composed_annotation_never_inherits_an_ancestor_mapping() {
    let method = r#"package a;
import java.lang.annotation.*;
import org.springframework.web.bind.annotation.*;
@RequestMapping(path = "/own", method = RequestMethod.POST)
@Target(ElementType.METHOD) @Retention(RetentionPolicy.RUNTIME)
@interface OwnPost {}
interface Base { @GetMapping("/base") String call(); }
@RestController @RequestMapping("/r")
class C implements Base {
    @OwnPost public String call() { return ""; }
}
"#;
    assert!(route_table(&[("src/main/java/a/C.java", method)]).is_empty());
    let prefix = r#"package a;
import java.lang.annotation.*;
import org.springframework.web.bind.annotation.*;
@RequestMapping("/own")
@Target(ElementType.TYPE) @Retention(RetentionPolicy.RUNTIME)
@interface OwnPrefix {}
@RequestMapping("/base") interface Base {}
@RestController @OwnPrefix
class C implements Base {
    @GetMapping("/x") public String x() { return ""; }
}
"#;
    assert!(route_table(&[("src/main/java/a/C.java", prefix)]).is_empty());
    // Declared in another file and imported.
    let own_post = r#"package a.meta;
import org.springframework.web.bind.annotation.*;
@RequestMapping(path = "/own", method = RequestMethod.POST)
public @interface OwnPost {}
"#;
    let imported = method
        .replace(
            "@RequestMapping(path = \"/own\", method = RequestMethod.POST)\n@Target(ElementType.METHOD) @Retention(RetentionPolicy.RUNTIME)\n@interface OwnPost {}\n",
            "",
        )
        .replace("package a;\n", "package a;\nimport a.meta.OwnPost;\n");
    assert!(
        route_table(&[
            ("src/main/java/a/meta/OwnPost.java", own_post),
            ("src/main/java/a/C.java", &imported),
        ])
        .is_empty()
    );
    let jaxrs = r#"package a;
import java.lang.annotation.*;
import javax.ws.rs.*;
@HttpMethod("PATCH")
@Target(ElementType.METHOD) @Retention(RetentionPolicy.RUNTIME)
@interface Patchy {}
interface Base { @GET @Path("old") String call(); }
@Path("/r")
public class R implements Base {
    @Patchy public String call() { return ""; }
}
"#;
    assert!(route_table(&[("src/main/java/a/R.java", jaxrs)]).is_empty());
    // Control: an unrelated annotation does not stop ordinary inheritance.
    let ordinary = method.replace("@OwnPost public", "@Override @Deprecated public");
    assert_eq!(
        route_table(&[("src/main/java/a/C.java", &ordinary)]),
        ["GET /r/base"]
    );
    let ordinary_jaxrs = jaxrs.replace("@Patchy public", "@Override public");
    assert_eq!(
        route_table(&[("src/main/java/a/R.java", &ordinary_jaxrs)]),
        ["GET /r/old"]
    );
}

/// #876 review 4: a user type named `Object` is that type, not `java.lang.Object`.
#[test]
fn a_user_type_named_object_is_not_java_lang_object() {
    let same_document = r#"package a;
import org.springframework.web.bind.annotation.*;
@RequestMapping("/api") class Object {}
@RestController
class C extends Object {
    @GetMapping("/x") public String x() { return ""; }
}
"#;
    assert_eq!(
        route_table(&[("src/main/java/a/C.java", same_document)]),
        ["GET /api/x"]
    );
    let user_object = r#"package b;
import org.springframework.web.bind.annotation.*;
@RequestMapping("/api")
public class Object {}
"#;
    let imported = r#"package a;
import b.Object;
import org.springframework.web.bind.annotation.*;
@RestController
class C extends Object {
    @GetMapping("/x") public String x() { return ""; }
}
"#;
    let both = [
        ("src/main/java/b/Object.java", user_object),
        ("src/main/java/a/C.java", imported),
    ];
    assert_eq!(route_table(&both), ["GET /api/x"]);
    // An imported `Object` the snapshot does not hold is unknown: nothing registers.
    assert!(route_table(&[both[1]]).is_empty());
    // Control: `java.lang.Object` carries no mapping.
    let plain = imported.replace("import b.Object;\n", "");
    assert_eq!(
        route_table(&[("src/main/java/a/C.java", &plain)]),
        ["GET /x"]
    );
    let qualified = plain.replace("extends Object", "extends java.lang.Object");
    assert_eq!(
        route_table(&[("src/main/java/a/C.java", &qualified)]),
        ["GET /x"]
    );
}

/// #876 review 5: a custom URI factory or template handler overrides the base URL in either
/// order, so a rooted path is unresolved.
#[test]
fn a_custom_uri_factory_overrides_the_base_url_in_either_order() {
    let client = r#"package c;
import org.springframework.web.reactive.function.client.WebClient;
import org.springframework.web.client.RestTemplate;
import org.springframework.boot.web.client.RestTemplateBuilder;
class C {
    private final WebClient before = WebClient.builder().uriBuilderFactory(factory).baseUrl("http://h/api").build();
    private final WebClient after = WebClient.builder().baseUrl("http://h/api").uriBuilderFactory(factory).build();
    private final WebClient base = WebClient.builder().baseUrl("http://h/api").build();
    private final RestTemplate handled = new RestTemplateBuilder().uriTemplateHandler(handler).rootUri("http://h/api").build();
    void run() {
        before.get().uri("/users").retrieve();
        after.get().uri("/users").retrieve();
        base.get().uri("/users").retrieve();
        handled.getForObject("/users", String.class);
    }
}
"#;
    assert_eq!(calls(client), ["GET ?", "GET ?", "GET /api/users", "GET ?"]);
}

/// #876 review 6: `String.format` is the standard formatter only when `String` is
/// `java.lang.String` at the call.
#[test]
fn a_shadowed_string_format_is_not_read() {
    let parameter = r#"package c;
import org.springframework.web.client.RestTemplate;
class Formatter { java.lang.String format(java.lang.String ignored) { return null; } }
class C {
    void run(RestTemplate rest, Formatter String) {
        rest.getForObject(String.format("http://h/api/users"), java.lang.String.class);
    }
}
"#;
    assert_eq!(calls(parameter), ["GET ?"]);
    let local = parameter.replace(
        "void run(RestTemplate rest, Formatter String) {",
        "void run(RestTemplate rest) { Formatter String = new Formatter();",
    );
    assert_eq!(calls(&local), ["GET ?"]);
    let imported = r#"package c;
import com.example.String;
import org.springframework.web.client.RestTemplate;
class C {
    void run(RestTemplate rest) {
        rest.getForObject(String.format("http://h/api/users"), Object.class);
    }
}
"#;
    assert_eq!(calls(imported), ["GET ?"]);
    // Controls: the standard formatter, written simple or qualified.
    let standard = r#"package c;
import org.springframework.web.client.RestTemplate;
class C {
    void run(RestTemplate rest) {
        rest.getForObject(String.format("http://h/api/%s", "users"), String.class);
        rest.getForObject(java.lang.String.format("http://h/api/users"), String.class);
    }
}
"#;
    assert_eq!(calls(standard), ["GET /api/users", "GET /api/users"]);
}

/// #876 review (calls 3): a `java.net.URI` is sent as it is, so a client's base URL does not
/// prefix a rooted `URI`.
#[test]
fn a_rooted_uri_under_a_base_url_is_unresolved() {
    let client = r#"package c;
import java.net.URI;
import org.springframework.web.client.RestTemplate;
import org.springframework.boot.web.client.RestTemplateBuilder;
class C {
    private final RestTemplate rooted = new RestTemplateBuilder().rootUri("http://h/api").build();
    void run() {
        rooted.getForObject(URI.create("/users"), String.class);
        rooted.getForObject("/users", String.class);
        rooted.getForObject(URI.create("http://h/api/users"), String.class);
    }
}
"#;
    assert_eq!(calls(client), ["GET ?", "GET /api/users", "GET /api/users"]);
}

/// #876 review (residual): every value a document binds hides a library type of the same
/// name used as a qualifier, an enum constant included.
#[test]
fn any_value_binding_hides_a_library_qualifier() {
    const IMPORTS: &str = "package c;\nimport java.net.URI;\nimport java.net.http.HttpRequest;\nimport org.springframework.http.HttpMethod;\nimport org.springframework.web.client.RestTemplate;\nimport org.springframework.web.reactive.function.client.WebClient;\n";
    // (qualifier, the call, what it reads as unshadowed, what it reads as shadowed)
    let qualifiers: [(&str, &str, &[&str], &[&str]); 5] = [
        (
            "String",
            "rest.getForObject(String.format(\"http://h/api/users\"), Object.class);",
            &["GET /api/users"],
            &["GET ?"],
        ),
        (
            "HttpMethod",
            "rest.exchange(\"http://h/api/users\", HttpMethod.POST, null, Object.class);",
            &["POST /api/users"],
            &["? /api/users"],
        ),
        (
            "URI",
            "rest.getForObject(URI.create(\"http://h/api/users\"), Object.class);",
            &["GET /api/users"],
            &["GET ?"],
        ),
        (
            "HttpRequest",
            "HttpRequest.newBuilder(java.net.URI.create(\"http://h/api/users\")).build();",
            &["GET /api/users"],
            &[],
        ),
        (
            "WebClient",
            "WebClient.create(\"http://h\").get().uri(\"/api/users\").retrieve();",
            &["GET /api/users"],
            &[],
        ),
    ];
    for (name, call, unshadowed, shadowed) in qualifiers {
        let control = format!("{IMPORTS}class C {{ void run(RestTemplate rest) {{ {call} }} }}\n");
        assert_eq!(calls(&control), unshadowed, "{name} control");
        let bindings = [
            (
                "an enum constant",
                format!("enum C {{ {name}; void run(RestTemplate rest) {{ {call} }} }}"),
            ),
            (
                "a field",
                format!("class C {{ Object {name}; void run(RestTemplate rest) {{ {call} }} }}"),
            ),
            (
                "a record component",
                format!("record C(Object {name}) {{ void run(RestTemplate rest) {{ {call} }} }}"),
            ),
            (
                "a local",
                format!(
                    "class C {{ void run(RestTemplate rest) {{ Object {name} = null; {call} }} }}"
                ),
            ),
            (
                "a lambda parameter",
                format!(
                    "class C {{ void run(RestTemplate rest) {{ java.util.function.Consumer<Object> f = {name} -> {{}}; {call} }} }}"
                ),
            ),
            (
                "a catch parameter",
                format!(
                    "class C {{ void run(RestTemplate rest) {{ try {{ {call} }} catch (RuntimeException {name}) {{}} }} }}"
                ),
            ),
            (
                "a pattern variable",
                format!(
                    "class C {{ void run(RestTemplate rest, Object o) {{ if (o instanceof Integer {name}) {{}} {call} }} }}"
                ),
            ),
            (
                "a resource",
                format!(
                    "class C {{ void run(RestTemplate rest) throws Exception {{ try (java.io.StringReader {name} = null) {{}} {call} }} }}"
                ),
            ),
            (
                "a local class",
                format!("class C {{ void run(RestTemplate rest) {{ class {name} {{}} {call} }} }}"),
            ),
        ];
        for (form, body) in bindings {
            let source = format!("{IMPORTS}{body}\n");
            assert_eq!(calls(&source), shadowed, "{name} bound by {form}");
        }
    }
    // The server side: `RequestMethod` bound by an enum constant of a nested enum.
    let route = |extra: &str| {
        format!(
            "package a;\nimport org.springframework.web.bind.annotation.*;\n@RestController\nclass C {{\n    {extra}\n    @RequestMapping(value = \"/x\", method = RequestMethod.GET) String x() {{ return \"\"; }}\n}}\n"
        )
    };
    assert_eq!(
        route_table(&[("src/main/java/a/C.java", &route(""))]),
        ["GET /x"]
    );
    let shadowed = route("enum Kind { RequestMethod }");
    assert!(route_table(&[("src/main/java/a/C.java", &shadowed)]).is_empty());
}

/// #876 review (residual): a parameter type reads as one canonical name however it is
/// written, so an override of a method declared with a type of its own document matches.
#[test]
fn one_parameter_type_reads_the_same_however_it_is_written() {
    let api = r#"package a;
import org.springframework.web.bind.annotation.*;
interface Api { @GetMapping("/x") void run(Id id); }
class Id {}
"#;
    let controller = |parameter: &str, imports: &str| {
        format!(
            "package a;\n{imports}import org.springframework.web.bind.annotation.*;\n@RestController @RequestMapping(\"/r\")\nclass C implements Api {{\n    public void run({parameter} id) {{}}\n}}\n"
        )
    };
    // A simple `Id` here names a non-public type of another file, which the snapshot does
    // not bind by its file: the method is unknown, never anchored on the abstract `Api.run`.
    let simple = controller("Id", "");
    assert!(
        route_table(&[
            ("src/main/java/a/Api.java", api),
            ("src/main/java/a/C.java", &simple),
        ])
        .is_empty()
    );
    // Written qualified or imported: one method, whose handler is the controller's body.
    for (parameter, imports) in [("a.Id", ""), ("Id", "import a.Id;\n")] {
        let source = controller(parameter, imports);
        let found = run(&[
            ("src/main/java/a/Api.java", api),
            ("src/main/java/a/C.java", &source),
        ]);
        assert_eq!(routes(&found), ["GET /r/x"], "{parameter} {imports}");
        assert_eq!(
            found.routes[0].handler.document.path, "src/main/java/a/C.java",
            "{parameter} {imports}"
        );
        assert_eq!(
            found.routes[0].handler.lines.start,
            5 + imports.lines().count()
        );
    }
    // A nested type is named by its enclosing type, as a qualified spelling reads.
    let nested = r#"package a;
import org.springframework.web.bind.annotation.*;
interface Api { @GetMapping("/x") void run(Api.Id id); class Id {} }
"#;
    let source = controller("a.Api.Id", "");
    let found = run(&[
        ("src/main/java/a/Api.java", nested),
        ("src/main/java/a/C.java", &source),
    ]);
    // `Api.Id` starts with a type, so it is not proven to be `a.Api.Id`: the method is
    // unknown rather than anchored on the abstract declaration.
    assert!(
        found
            .routes
            .iter()
            .all(|route| route.handler.document.path == "src/main/java/a/C.java"),
        "{:?}",
        routes(&found)
    );
    // Control: a distinct qualified type is a distinct overload.
    let other = controller("b.Id", "");
    let found = run(&[
        ("src/main/java/a/Api.java", api),
        ("src/main/java/a/C.java", &other),
    ]);
    assert_eq!(routes(&found), ["GET /r/x"]);
    assert_eq!(
        found.routes[0].handler.document.path,
        "src/main/java/a/Api.java"
    );
}
