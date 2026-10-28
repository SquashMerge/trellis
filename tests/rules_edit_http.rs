#![cfg(feature = "test-support")]
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, sync::Arc, time::Duration};
use tower::ServiceExt;
use trellis::{
    daemon::registry::{CheckoutOptions, CheckoutRegistry, CheckoutRuntime},
    http::ProvisionedBrowser,
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const RULES: &str = "# Preserve this reviewed configuration.\nversion=1\n[layers.source]\norder=2\npaths=['src/source.rs','src/quiet.rs']\n[layers.target]\norder=1\npaths=['src/target.rs','src/lib.rs']\n";
const EXCEPTION: &str = "\n# Existing exception comment.\n[[exceptions]]\nrule='$layers'\ntarget={language='rust',path='src/source.rs'}\ndestination={language='rust',path='src/target.rs'}\nreason='Reviewed edge'\n";

struct Fixture {
    base: tempfile::TempDir,
    _runtime: Arc<CheckoutRuntime>,
    browser: ProvisionedBrowser,
    key: String,
    generation: Value,
    revision: Value,
}
impl Fixture {
    fn rules_path(&self) -> std::path::PathBuf {
        self.base.path().join("checkout/.trellis/rules.toml")
    }
    fn route(&self, suffix: &str) -> String {
        format!("/api/checkouts/{}/architecture/rules{suffix}", self.key)
    }
    fn pin_query(&self) -> String {
        format!(
            "?indexGeneration={}&indexRevision={}",
            self.generation.as_str().unwrap(),
            self.revision.as_u64().unwrap()
        )
    }
    fn body(&self, base_hash: &Value, operations: Value) -> Value {
        json!({
            "indexGeneration": self.generation,
            "indexRevision": self.revision,
            "baseHash": base_hash,
            "operations": operations,
        })
    }
    async fn inventory(&self, app: &Router) -> Value {
        let (status, _, body) = call(app, "GET", &self.route(&self.pin_query()), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["payload"].clone()
    }
}

async fn raw_call(
    app: &Router,
    method: &str,
    path: &str,
    body: Vec<u8>,
    token: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:7331")
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, body)
}
async fn call(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (StatusCode, HeaderMap, Value) {
    raw_call(
        app,
        method,
        path,
        body.map_or_else(Vec::new, |body| serde_json::to_vec(body).unwrap()),
        Some(TOKEN),
    )
    .await
}

async fn fixture(with_exception: bool) -> Fixture {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("checkout");
    fs::create_dir_all(root.join("src")).unwrap();
    for (path, text) in [
        ("src/lib.rs", "mod source; mod target; mod quiet;\n"),
        ("src/source.rs", "pub fn run() { crate::target::run(); }\n"),
        ("src/target.rs", "pub fn run() {}\n"),
        ("src/quiet.rs", "pub fn unused() {}\n"),
        ("src/unassigned.rs", "pub fn unassigned() {}\n"),
    ] {
        fs::write(root.join(path), text).unwrap();
    }
    fs::create_dir(root.join(".trellis")).unwrap();
    fs::write(
        root.join(".trellis/rules.toml"),
        format!("{RULES}{}", if with_exception { EXCEPTION } else { "" }),
    )
    .unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    let store = Store::open(
        roots.clone(),
        WorkspaceIdentity::discover(Some(&root), &root)
            .unwrap()
            .attach_marker()
            .unwrap(),
    )
    .unwrap();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let leader = trellis::index_coordinator::establish_serving_session(
        &store,
        Some(&trellis::indexer::IndexOptions::new(root.clone())),
        &cancel,
    )
    .unwrap();
    leader.verify().unwrap();
    drop(leader);
    drop(store);
    let mut registry = CheckoutRegistry::with_roots(roots);
    registry
        .register(&identity, CheckoutOptions(json!({})))
        .unwrap();
    registry.attach_launch(1, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((response, catching_up)) = runtime.evidence_response()
                && response.status().is_ok()
                && response.finish(()).is_ok()
                && !catching_up
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let browser = ProvisionedBrowser::new(
        Arc::new(tokio::sync::Mutex::new(registry)),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let app = browser.clone().router();
    let (status, _, architecture) = call(
        &app,
        "GET",
        &format!("/api/checkouts/{}/architecture", identity.root_key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{architecture}");
    Fixture {
        base,
        _runtime: runtime,
        browser,
        key: identity.root_key,
        generation: architecture["payload"]["indexGeneration"].clone(),
        revision: architecture["payload"]["indexRevision"].clone(),
    }
}

fn edit(reason: &str) -> Value {
    json!([{"type":"editException","index":0,"reason":reason}])
}
fn pair(source: &str, destination: &str) -> Value {
    json!({
        "source":{"language":"rust","path":source},
        "destination":{"language":"rust","path":destination},
    })
}
fn add(pair: Value, reason: &str) -> Value {
    json!([{"type":"addExceptions","pairs":[pair],"reason":reason}])
}
fn assert_rejected(status: StatusCode, error: &Value) {
    assert!(
        matches!(
            status,
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
        ),
        "{status}: {error}"
    );
    assert!(error["error"]["code"].is_string(), "{error}");
    assert!(error.get("payload").is_none(), "{error}");
}
fn assert_conflict(status: StatusCode, error: &Value) {
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "rules_conflict", "{error}");
    assert!(error.get("payload").is_none(), "{error}");
}

#[tokio::test]
async fn inventory_requires_complete_pin_and_lists_editable_configuration() {
    let f = fixture(true).await;
    let app = f.browser.clone().router();
    for query in [
        String::new(),
        format!("?indexGeneration={}", f.generation.as_str().unwrap()),
        format!("?indexRevision={}", f.revision),
        format!("{}&indexRevision={}", f.pin_query(), f.revision),
        format!("{}&unexpected=true", f.pin_query()),
    ] {
        let (status, _, error) = call(&app, "GET", &f.route(&query), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {error}");
    }
    let (status, _, error) = call(
        &app,
        "GET",
        &f.route(&format!(
            "?indexGeneration={}&indexRevision={}",
            f.generation.as_str().unwrap(),
            f.revision.as_u64().unwrap() + 1
        )),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    let inventory = f.inventory(&app).await;
    assert_eq!(inventory["indexGeneration"], f.generation);
    assert_eq!(inventory["indexRevision"], f.revision);
    assert_eq!(inventory["rulesFile"]["path"], ".trellis/rules.toml");
    let original = fs::read(f.rules_path()).unwrap();
    assert_eq!(
        inventory["rulesFile"]["contentHash"],
        hex::encode(Sha256::digest(&original))
    );
    let exceptions = inventory["exceptions"].as_array().unwrap();
    assert_eq!(exceptions.len(), 1);
    assert_eq!(exceptions[0]["index"], 0);
    assert_eq!(exceptions[0]["rule"], "$layers");
    assert_eq!(exceptions[0]["reason"], "Reviewed edge");
    assert_eq!(
        exceptions[0]["target"],
        json!({"language":"rust","path":"src/source.rs"})
    );
    assert_eq!(
        exceptions[0]["destination"],
        json!({"language":"rust","path":"src/target.rs"})
    );
    let layers = inventory["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 2);
    assert!(layers.iter().any(|layer| layer["name"] == "source"));
    assert!(layers.iter().any(|layer| layer["name"] == "target"));
}

#[tokio::test]
async fn reason_preview_is_read_only_and_apply_writes_exact_preview_bytes() {
    let f = fixture(true).await;
    let app = f.browser.clone().router();
    let inventory = f.inventory(&app).await;
    let original = fs::read(f.rules_path()).unwrap();
    let mut body = f.body(
        &inventory["rulesFile"]["contentHash"],
        edit("Reviewed again: é"),
    );
    let (status, _, preview) = call(&app, "POST", &f.route("/preview"), Some(&body)).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(fs::read(f.rules_path()).unwrap(), original);
    let proposed = &preview["payload"];
    assert_eq!(proposed["indexGeneration"], f.generation);
    assert_eq!(proposed["indexRevision"], f.revision);
    assert_eq!(proposed["baseHash"], body["baseHash"]);
    let text = proposed["text"].as_str().unwrap();
    assert!(text.contains("# Preserve this reviewed configuration."));
    assert!(text.contains("# Existing exception comment."));
    assert!(text.contains("Reviewed again: é"));
    assert_eq!(
        proposed["proposedHash"],
        hex::encode(Sha256::digest(text.as_bytes()))
    );
    assert!(proposed["diff"].is_string());
    assert!(!proposed["diff"].as_str().unwrap().is_empty());
    body["proposedHash"] = proposed["proposedHash"].clone();
    let (status, headers, applied) = call(&app, "POST", &f.route("/apply"), Some(&body)).await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    assert_eq!(applied["payload"]["mutationOutcome"], "committed");
    assert_eq!(headers["x-trellis-mutation-outcome"], "committed");
    assert_eq!(
        applied["payload"]["rulesFile"]["contentHash"],
        proposed["proposedHash"]
    );
    assert_eq!(fs::read(f.rules_path()).unwrap(), text.as_bytes());
    let saved = f.inventory(&app).await;
    assert_eq!(saved["exceptions"][0]["reason"], "Reviewed again: é");
}

#[tokio::test]
async fn preview_and_apply_reject_changed_base_without_overwriting_external_edit() {
    let f = fixture(true).await;
    let app = f.browser.clone().router();
    let inventory = f.inventory(&app).await;
    let mut body = f.body(&inventory["rulesFile"]["contentHash"], edit("My draft"));
    let (status, _, preview) = call(&app, "POST", &f.route("/preview"), Some(&body)).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let external = format!("{RULES}{EXCEPTION}# External editor change.\n");
    fs::write(f.rules_path(), &external).unwrap();
    let (status, _, error) = call(&app, "POST", &f.route("/preview"), Some(&body)).await;
    assert_conflict(status, &error);
    body["proposedHash"] = preview["payload"]["proposedHash"].clone();
    let (status, _, error) = call(&app, "POST", &f.route("/apply"), Some(&body)).await;
    assert_conflict(status, &error);
    assert_eq!(fs::read(f.rules_path()).unwrap(), external.as_bytes());
}

#[tokio::test]
async fn add_requires_positive_deviating_pair_and_nonblank_reason() {
    let f = fixture(false).await;
    let app = f.browser.clone().router();
    let inventory = f.inventory(&app).await;
    let hash = &inventory["rulesFile"]["contentHash"];
    let original = fs::read(f.rules_path()).unwrap();
    for operations in [
        add(pair("src/quiet.rs", "src/target.rs"), "Not witnessed"),
        add(
            pair("src/target.rs", "src/source.rs"),
            "Reverse is not witnessed",
        ),
        add(pair("src/source.rs", "src/target.rs"), " \t\n"),
        add(pair("src/source.rs", "src/target.rs"), ""),
    ] {
        let body = f.body(hash, operations);
        let (status, _, error) = call(&app, "POST", &f.route("/preview"), Some(&body)).await;
        assert_rejected(status, &error);
        assert_eq!(fs::read(f.rules_path()).unwrap(), original);
    }
    let mut body = f.body(
        hash,
        add(
            pair("src/source.rs", "src/target.rs"),
            "Approved existing edge",
        ),
    );
    let (status, _, preview) = call(&app, "POST", &f.route("/preview"), Some(&body)).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(fs::read(f.rules_path()).unwrap(), original);
    body["proposedHash"] = preview["payload"]["proposedHash"].clone();
    let (status, _, saved) = call(&app, "POST", &f.route("/apply"), Some(&body)).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        fs::read(f.rules_path()).unwrap(),
        preview["payload"]["text"].as_str().unwrap().as_bytes()
    );
    let inventory = f.inventory(&app).await;
    assert_eq!(inventory["exceptions"].as_array().unwrap().len(), 1);
    assert_eq!(
        inventory["exceptions"][0]["reason"],
        "Approved existing edge"
    );
}

#[tokio::test]
async fn mutation_requests_have_closed_shapes_and_fixed_body_bound() {
    let f = fixture(true).await;
    let app = f.browser.clone().router();
    let inventory = f.inventory(&app).await;
    let valid = f.body(&inventory["rulesFile"]["contentHash"], edit("Changed"));
    let original = fs::read(f.rules_path()).unwrap();
    let mut unknown_field = valid.clone();
    unknown_field["rawText"] = json!("version=1");
    let mut missing_pin = valid.clone();
    missing_pin
        .as_object_mut()
        .unwrap()
        .remove("indexGeneration");
    let mut missing_base = valid.clone();
    missing_base.as_object_mut().unwrap().remove("baseHash");
    let mut unknown_op = valid.clone();
    unknown_op["operations"] = json!([{"type":"replaceFile","text":"version=1"}]);
    let mut unknown_op_field = valid.clone();
    unknown_op_field["operations"][0]["path"] = json!("src/source.rs");
    for body in [
        unknown_field,
        missing_pin,
        missing_base,
        unknown_op,
        unknown_op_field,
    ] {
        let (status, _, error) = call(&app, "POST", &f.route("/preview"), Some(&body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {error}");
    }
    let (status, _, error) = call(&app, "POST", &f.route("/apply"), Some(&valid)).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "missing proposedHash: {error}"
    );
    for suffix in ["/preview", "/apply"] {
        let (status, _, error) = raw_call(
            &app,
            "POST",
            &f.route(suffix),
            vec![b' '; 1024 * 1024 + 1],
            Some(TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{suffix}: {error}");
    }
    assert_eq!(fs::read(f.rules_path()).unwrap(), original);
}

#[tokio::test]
async fn rules_routes_are_authenticated_and_checkout_scoped() {
    let f = fixture(true).await;
    let app = f.browser.clone().router();
    let original = fs::read(f.rules_path()).unwrap();
    for (method, suffix) in [
        ("GET", f.pin_query()),
        ("POST", "/preview".into()),
        ("POST", "/apply".into()),
    ] {
        for token in [None, Some("wrong-token")] {
            let (status, _, error) =
                raw_call(&app, method, &f.route(&suffix), b"{}".to_vec(), token).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {suffix}: {error}"
            );
        }
        let (status, _, error) = call(
            &app,
            method,
            &format!("/api/architecture/rules{suffix}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "unscoped {method}: {error}");
    }
    let unknown_key = format!(
        "{}{}",
        if f.key.starts_with('a') { "b" } else { "a" },
        &f.key[1..]
    );
    let (status, _, error) = call(
        &app,
        "GET",
        &format!(
            "/api/checkouts/{unknown_key}/architecture/rules{}",
            f.pin_query()
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert_eq!(error["error"]["code"], "workspace_selection_failed");
    assert_eq!(error["error"]["reason"], "not_checkout");
    assert_eq!(fs::read(f.rules_path()).unwrap(), original);
}

#[tokio::test]
async fn two_browser_drafts_from_same_base_have_one_serial_save_and_one_conflict() {
    let f = fixture(true).await;
    let first_browser = f.browser.clone().router();
    let second_browser = f.browser.clone().router();
    let first_inventory = f.inventory(&first_browser).await;
    let second_inventory = f.inventory(&second_browser).await;
    assert_eq!(first_inventory["rulesFile"], second_inventory["rulesFile"]);
    let hash = &first_inventory["rulesFile"]["contentHash"];
    let mut first = f.body(hash, edit("First browser reason"));
    let mut second = f.body(hash, edit("Second browser reason"));
    let (status, _, first_preview) =
        call(&first_browser, "POST", &f.route("/preview"), Some(&first)).await;
    assert_eq!(status, StatusCode::OK, "{first_preview}");
    let (status, _, second_preview) =
        call(&second_browser, "POST", &f.route("/preview"), Some(&second)).await;
    assert_eq!(status, StatusCode::OK, "{second_preview}");
    first["proposedHash"] = first_preview["payload"]["proposedHash"].clone();
    second["proposedHash"] = second_preview["payload"]["proposedHash"].clone();
    let (status, _, committed) =
        call(&first_browser, "POST", &f.route("/apply"), Some(&first)).await;
    assert_eq!(status, StatusCode::OK, "{committed}");
    let (status, _, conflict) =
        call(&second_browser, "POST", &f.route("/apply"), Some(&second)).await;
    assert_conflict(status, &conflict);
    assert_eq!(
        fs::read(f.rules_path()).unwrap(),
        first_preview["payload"]["text"]
            .as_str()
            .unwrap()
            .as_bytes()
    );
    assert_eq!(
        f.inventory(&second_browser).await["exceptions"][0]["reason"],
        "First browser reason"
    );
}

#[tokio::test]
async fn response_bound_refuses_preview_and_apply_before_writing() {
    let f = fixture(true).await;
    let app = f.browser.clone().router();
    let inventory = f.inventory(&app).await;
    let mut body = f.body(
        &inventory["rulesFile"]["contentHash"],
        edit("Bounded draft"),
    );
    let (status, _, preview) = call(&app, "POST", &f.route("/preview"), Some(&body)).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let original = fs::read(f.rules_path()).unwrap();
    f.browser.set_metrics_controls_for_tests(20, None, false);
    let (status, _, error) = call(&app, "POST", &f.route("/preview"), Some(&body)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{error}");
    assert_eq!(error["error"]["code"], "response_too_large");
    assert_eq!(fs::read(f.rules_path()).unwrap(), original);
    body["proposedHash"] = preview["payload"]["proposedHash"].clone();
    let (status, _, error) = call(&app, "POST", &f.route("/apply"), Some(&body)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{error}");
    assert_eq!(error["error"]["code"], "response_too_large");
    assert_eq!(fs::read(f.rules_path()).unwrap(), original);
}

#[tokio::test]
async fn cancellation_and_deadline_refuse_preview_and_apply_without_writing() {
    let f = fixture(true).await;
    let app = f.browser.clone().router();
    let inventory = f.inventory(&app).await;
    let preview_body = f.body(
        &inventory["rulesFile"]["contentHash"],
        edit("Interrupted draft"),
    );
    let (status, _, preview) = call(&app, "POST", &f.route("/preview"), Some(&preview_body)).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let mut apply_body = preview_body.clone();
    apply_body["proposedHash"] = preview["payload"]["proposedHash"].clone();
    let original = fs::read(f.rules_path()).unwrap();
    for (cancelled, deadline, skew, code) in [
        (true, None, 0, "cancelled"),
        (
            false,
            Some(Duration::from_millis(1)),
            1001,
            "deadline_exceeded",
        ),
    ] {
        f.browser
            .set_metrics_controls_for_tests(64 * 1024 * 1024, deadline, cancelled);
        f.browser.set_metrics_clock_skew_for_tests(skew);
        for (suffix, body) in [("/preview", &preview_body), ("/apply", &apply_body)] {
            let (status, _, error) = call(&app, "POST", &f.route(suffix), Some(body)).await;
            assert!(
                status.is_client_error() || status.is_server_error(),
                "{error}"
            );
            assert_eq!(error["error"]["code"], code, "{suffix}: {error}");
            assert_eq!(fs::read(f.rules_path()).unwrap(), original);
        }
    }
}

#[tokio::test]
async fn assignment_requires_existing_layer_and_indexed_unassigned_file() {
    let f = fixture(false).await;
    let app = f.browser.clone().router();
    let inventory = f.inventory(&app).await;
    let hash = &inventory["rulesFile"]["contentHash"];
    let original = fs::read(f.rules_path()).unwrap();
    for (path, layer) in [
        ("src/unassigned.rs", "missing-layer"),
        ("src/source.rs", "target"),
        ("src/absent.rs", "source"),
    ] {
        let body = f.body(
            hash,
            json!([{"type":"assignFile","path":path,"layer":layer}]),
        );
        let (status, _, error) = call(&app, "POST", &f.route("/preview"), Some(&body)).await;
        assert_rejected(status, &error);
        assert_eq!(fs::read(f.rules_path()).unwrap(), original);
    }
    let mut body = f.body(
        hash,
        json!([{"type":"assignFile","path":"src/unassigned.rs","layer":"source"}]),
    );
    let (status, _, preview) = call(&app, "POST", &f.route("/preview"), Some(&body)).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(fs::read(f.rules_path()).unwrap(), original);
    body["proposedHash"] = preview["payload"]["proposedHash"].clone();
    let (status, _, saved) = call(&app, "POST", &f.route("/apply"), Some(&body)).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let config: toml::Value = toml::from_str(&fs::read_to_string(f.rules_path()).unwrap()).unwrap();
    assert!(
        config["layers"]["source"]["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path.as_str() == Some("src/unassigned.rs"))
    );
}

#[tokio::test]
async fn unmatched_named_exception_can_be_edited_and_removed() {
    let f = fixture(false).await;
    let app = f.browser.clone().router();
    let named = format!(
        "{RULES}\n[roles.unmatched]\nselect={{name='NoSuchDeclaration'}}\n[[rules]]\nid='named'\nfor_each='unmatched'\nrequire=[{{calls='unmatched'}}]\n[[exceptions]]\nrule='named'\ntarget={{source_set='missing-source-set',language='rust',path='src/source.rs',sid='sid:v1:00000000000000000000000000000000'}}\nreason='Old review'\n"
    );
    fs::write(f.rules_path(), &named).unwrap();
    for (operations, remaining) in [
        (edit("Still reviewed"), 1),
        (json!([{"type":"removeException","index":0}]), 0),
    ] {
        let inventory = f.inventory(&app).await;
        assert_eq!(inventory["exceptions"][0]["rule"], "named");
        let mut body = f.body(&inventory["rulesFile"]["contentHash"], operations);
        let original = fs::read(f.rules_path()).unwrap();
        let (status, _, preview) = call(&app, "POST", &f.route("/preview"), Some(&body)).await;
        assert_eq!(status, StatusCode::OK, "{preview}");
        assert_eq!(fs::read(f.rules_path()).unwrap(), original);
        body["proposedHash"] = preview["payload"]["proposedHash"].clone();
        let (status, _, saved) = call(&app, "POST", &f.route("/apply"), Some(&body)).await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        let inventory = f.inventory(&app).await;
        assert_eq!(inventory["exceptions"].as_array().unwrap().len(), remaining);
        if remaining == 1 {
            assert_eq!(inventory["exceptions"][0]["reason"], "Still reviewed");
            assert_eq!(
                inventory["exceptions"][0]["target"]["source_set"],
                "missing-source-set"
            );
        }
    }
}

async fn await_hook_signal(receiver: std::sync::mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || {
        receiver
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn simultaneous_browser_saves_serialize_at_commit_and_conflict() {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    };
    let f = fixture(true).await;
    let app = f.browser.clone().router();
    let inventory = f.inventory(&app).await;
    let mut first = f.body(
        &inventory["rulesFile"]["contentHash"],
        edit("First overlapping draft"),
    );
    let mut second = f.body(
        &inventory["rulesFile"]["contentHash"],
        edit("Second overlapping draft"),
    );
    let (status, _, first_preview) = call(&app, "POST", &f.route("/preview"), Some(&first)).await;
    assert_eq!(status, StatusCode::OK, "{first_preview}");
    let (status, _, second_preview) = call(&app, "POST", &f.route("/preview"), Some(&second)).await;
    assert_eq!(status, StatusCode::OK, "{second_preview}");
    first["proposedHash"] = first_preview["payload"]["proposedHash"].clone();
    second["proposedHash"] = second_preview["payload"]["proposedHash"].clone();
    let original = fs::read(f.rules_path()).unwrap();
    let (held_tx, held_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let (second_started_tx, second_started_rx) = mpsc::sync_channel(1);
    let release = Mutex::new(release_rx);
    let first_commit = AtomicBool::new(true);
    let started = AtomicUsize::new(0);
    f.browser
        .set_selected_response_hook_for_tests(Arc::new(move |stage| {
            if stage == "before_work" && started.fetch_add(1, Ordering::AcqRel) == 1 {
                let _ = second_started_tx.try_send(());
            }
            if stage == "rules_before_rename" && first_commit.swap(false, Ordering::AcqRel) {
                held_tx.send(()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(trellis::testing::HANG_BACKSTOP)
                    .unwrap();
            }
        }));
    let first_worker = tokio::spawn({
        let app = app.clone();
        let route = f.route("/apply");
        async move { call(&app, "POST", &route, Some(&first)).await }
    });
    await_hook_signal(held_rx).await;
    assert_eq!(fs::read(f.rules_path()).unwrap(), original);
    let second_worker = tokio::spawn({
        let app = app.clone();
        let route = f.route("/apply");
        async move { call(&app, "POST", &route, Some(&second)).await }
    });
    await_hook_signal(second_started_rx).await;
    release_tx.send(()).unwrap();
    let (first_result, second_result) =
        tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            (first_worker.await.unwrap(), second_worker.await.unwrap())
        })
        .await
        .unwrap();
    assert_eq!(first_result.0, StatusCode::OK, "{}", first_result.2);
    assert_conflict(second_result.0, &second_result.2);
    assert_eq!(
        fs::read(f.rules_path()).unwrap(),
        first_preview["payload"]["text"]
            .as_str()
            .unwrap()
            .as_bytes()
    );
}

#[tokio::test]
async fn dropping_apply_future_before_rename_cancels_worker_without_writing() {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    let f = fixture(true).await;
    let app = f.browser.clone().router();
    let inventory = f.inventory(&app).await;
    let mut body = f.body(
        &inventory["rulesFile"]["contentHash"],
        edit("Abandoned draft"),
    );
    let (status, _, preview) = call(&app, "POST", &f.route("/preview"), Some(&body)).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    body["proposedHash"] = preview["payload"]["proposedHash"].clone();
    let original = fs::read(f.rules_path()).unwrap();
    let (held_tx, held_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let release = Mutex::new(release_rx);
    let once = AtomicBool::new(true);
    f.browser
        .set_selected_response_hook_for_tests(Arc::new(move |stage| {
            if stage == "rules_before_rename" && once.swap(false, Ordering::AcqRel) {
                held_tx.send(()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(trellis::testing::HANG_BACKSTOP)
                    .unwrap();
            }
            if stage == "metrics_worker_done" {
                let _ = done_tx.try_send(());
            }
        }));
    let worker = tokio::spawn({
        let app = app.clone();
        let route = f.route("/apply");
        async move { call(&app, "POST", &route, Some(&body)).await }
    });
    await_hook_signal(held_rx).await;
    assert_eq!(fs::read(f.rules_path()).unwrap(), original);
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    release_tx.send(()).unwrap();
    await_hook_signal(done_rx).await;
    assert_eq!(fs::read(f.rules_path()).unwrap(), original);
    assert_eq!(
        f.inventory(&app).await["exceptions"][0]["reason"],
        "Reviewed edge"
    );
}
