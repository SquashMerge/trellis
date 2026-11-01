#![cfg(feature = "test-support")]
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use std::{fs, sync::Arc, time::Duration};
use tower::ServiceExt;
use trellis::{
    daemon::registry::{CheckoutOptions, CheckoutRegistry},
    http::ProvisionedBrowser,
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
pub(super) async fn request(app: axum::Router, key: &str, query: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/checkouts/{key}/architecture{query}"))
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
pub(super) async fn fixture() -> (
    tempfile::TempDir,
    Arc<trellis::daemon::registry::CheckoutRuntime>,
    ProvisionedBrowser,
    String,
) {
    fixture_with_sources(false, &[]).await
}
async fn fixture_with_roles(
    role_fixture: bool,
) -> (
    tempfile::TempDir,
    Arc<trellis::daemon::registry::CheckoutRuntime>,
    ProvisionedBrowser,
    String,
) {
    fixture_with_sources(role_fixture, &[]).await
}
pub(super) async fn fixture_with_sources(
    role_fixture: bool,
    extra_sources: &[(&str, &str)],
) -> (
    tempfile::TempDir,
    Arc<trellis::daemon::registry::CheckoutRuntime>,
    ProvisionedBrowser,
    String,
) {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("sample.js"), "function sample(x) { return x; }\n").unwrap();
    fs::create_dir(root.join("deep")).unwrap();
    fs::write(
        root.join("deep/extra.js"),
        "function extra() { return 2; }\n",
    )
    .unwrap();
    fs::write(root.join("empty.js"), "// no callable\n").unwrap();
    for (path, source) in extra_sources {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    fs::create_dir(root.join("zero")).unwrap();
    fs::write(root.join("zero/empty.js"), "// zero-callable directory\n").unwrap();
    if role_fixture {
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod roles; mod target;\n").unwrap();
        fs::write(
            root.join("src/roles.rs"),
            r#"fn production() { crate::target::run(); }
#[cfg(feature = "test-support")] fn support() { crate::target::run(); }
#[cfg(test)] mod tests { fn check() { crate::target::run(); } }
"#,
        )
        .unwrap();
        fs::write(root.join("src/target.rs"), "pub fn run() {}\n").unwrap();
        fs::create_dir(root.join("tests")).unwrap();
        fs::write(
            root.join("tests/only.rs"),
            "fn only() { crate::target::run(); }\n",
        )
        .unwrap();
    }
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
        .register(&identity, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    registry.attach_launch(1, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    {
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
    }
    let browser = ProvisionedBrowser::new(
        Arc::new(tokio::sync::Mutex::new(registry)),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let key = identity.root_key;
    (base, runtime, browser, key)
}

pub(super) fn write_rules(base: &tempfile::TempDir, text: &str) {
    let path = base.path().join("checkout/.trellis");
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("rules.toml"), text).unwrap();
}
pub(super) const RULES: &str = "version=1\n[layers.app]\norder=1\npaths=['**/*.js']";
#[tokio::test]
async fn architecture_route_pin_aggregate_setup_and_located_diagnostics() {
    let (base, _runtime, browser, key) = fixture().await;
    let app = browser.clone().router();
    let (status, missing) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{missing}");
    assert_eq!(missing["payload"]["state"], "missing_rules");
    assert_eq!(missing["payload"]["aggregate"], "max");
    assert_eq!(missing["payload"]["role"], "all");
    assert!(missing["payload"].get("metricsScope").is_none());
    let generation = missing["payload"]["indexGeneration"].as_str().unwrap();
    let revision = missing["payload"]["indexRevision"].as_u64().unwrap();
    write_rules(&base, RULES);
    let (status, ready) = request(
        app.clone(),
        &key,
        &format!("?indexGeneration={generation}&indexRevision={revision}&aggregate=mean"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    assert_eq!(ready["payload"]["aggregate"], "mean");
    assert_eq!(ready["payload"]["modules"].as_array().unwrap().len(), 4);
    for query in [
        format!("?indexGeneration={generation}"),
        format!("?indexRevision={revision}"),
        "?aggregate=avg".into(),
        "?aggregate=sum&aggregate=max".into(),
        "?role=".into(),
        "?role=unknown".into(),
        "?role=Production".into(),
        "?role=all&role=test".into(),
        "?role=all&%72ole=production".into(),
        "?scope=revision".into(),
        "?metrics=cyclomatic".into(),
        "?unexpected=x".into(),
    ] {
        let (status, error) = request(app.clone(), &key, &query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {error}");
    }
    let (status, stale) = request(
        app.clone(),
        &key,
        &format!(
            "?indexGeneration={generation}&indexRevision={}",
            revision + 1
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
    write_rules(&base, "version=[");
    let (status, invalid) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
    assert_eq!(invalid["error"]["code"], "invalid_rules");
    for field in [
        "kind",
        "path",
        "range",
        "line",
        "column",
        "attribution",
        "message",
    ] {
        assert!(!invalid["error"][field].is_null(), "{field}: {invalid}");
    }
    assert!(invalid.get("payload").is_none());
}
#[tokio::test]
async fn architecture_succeeds_for_callables_inside_measured_macros() {
    let (base, _runtime, browser, key) = fixture_with_sources(
        false,
        &[(
            "src/macros.rs",
            include_str!("../fixtures/rust/macro_callables.rs"),
        )],
    )
    .await;
    write_rules(
        &base,
        "version=1\n[layers.app]\norder=1\npaths=['src/**','*.js','deep/**','zero/**']",
    );
    let (status, body) = request(browser.router(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let modules = body["payload"]["modules"].as_array().unwrap();
    let measured = modules
        .iter()
        .find(|module| module["identity"]["path"] == "src/macros.rs")
        .unwrap_or_else(|| panic!("{body}"));
    assert!(
        measured["declarationCount"].as_u64().unwrap() >= 13,
        "{measured}"
    );
}
async fn view(app: axum::Router, key: &str, route: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/checkouts/{key}/{route}"))
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// #554: repository content is untrusted, but one pathological file is a gap in the
/// views, never a refusal of the whole checkout.
#[tokio::test]
async fn one_over_deep_file_is_partial_and_every_whole_checkout_view_is_served() {
    let nest = |open: &str, close: &str, inner: &str| {
        format!("{}{inner}{}", open.repeat(400), close.repeat(400))
    };
    let cases = [
        (
            "src/deep.rs",
            format!("pub fn deep() {{ custom!({}); }}\n", nest("(", ")", "")),
        ),
        (
            "src/deep.rs",
            format!("pub fn deep() -> i32 {{ {} }}\n", nest("(", ")", "1")),
        ),
        (
            "src/deep.js",
            format!("function deep() {{ return {}; }}\n", nest("[", "]", "")),
        ),
        (
            "src/Deep.java",
            format!(
                "class Deep {{ int d() {{ return {}; }} }}\n",
                nest("(", ")", "1")
            ),
        ),
        (
            "src/deep.py",
            format!("def deep():\n    return {}\n", nest("(", ")", "1")),
        ),
    ];
    for (path, source) in cases {
        let (base, _runtime, browser, key) = fixture_with_sources(
            false,
            &[
                (path, source.as_str()),
                ("src/a.js", "import { b } from './b.js'; b();\n"),
                ("src/b.js", "export function b() {}\n"),
            ],
        )
        .await;
        write_rules(
            &base,
            "version=1\n[layers.app]\norder=1\npaths=['src/**','*.js','deep/**','zero/**']",
        );
        let app = browser.router();
        let (status, body) = request(app.clone(), &key, "").await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        let payload = &body["payload"];
        assert_eq!(payload["partialFiles"][0]["path"], path, "{payload}");
        assert_eq!(payload["references"]["partialFiles"][0]["path"], path);
        assert_eq!(payload["rules"]["partialFiles"][0]["path"], path);
        assert!(
            payload["references"]["limitations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|limit| limit.as_str().unwrap().contains("partialFiles"))
        );
        assert!(
            payload["references"]["counts"]["mappedSites"]
                .as_u64()
                .unwrap()
                >= 1
        );
        let modules = payload["modules"].as_array().unwrap();
        assert!(modules.iter().any(|m| m["identity"]["path"] == "src/a.js"));
        let (status, metrics) = view(app.clone(), &key, "metrics").await;
        assert_eq!(status, StatusCode::OK, "{path}: {metrics}");
        assert_eq!(metrics["payload"]["partialFiles"][0]["path"], path);
        let (status, classes) = view(app.clone(), &key, "classes").await;
        assert_eq!(status, StatusCode::OK, "{path}: {classes}");
    }
}

#[tokio::test]
async fn architecture_role_filters_metrics_without_changing_reference_evidence() {
    let (base, _runtime, browser, key) = fixture_with_roles(true).await;
    write_rules(
        &base,
        "version=1\n[layers.app]\norder=2\npaths=['src/roles.rs','tests/**']\n[layers.target]\norder=1\npaths=['src/target.rs']",
    );
    let app = browser.router();
    let (status, omitted) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{omitted}");
    let baseline = &omitted["payload"];
    assert_eq!(baseline["role"], "all");
    assert!(
        baseline["references"]["counts"]["mappedSites"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        baseline["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["role"] == "test")
    );
    let generation = baseline["indexGeneration"].as_str().unwrap();
    let revision = baseline["indexRevision"].as_u64().unwrap();
    let pin = format!("indexGeneration={generation}&indexRevision={revision}");
    for (role, count) in [("production", 2), ("all", 3), ("test", 1)] {
        let (status, selected) = request(app.clone(), &key, &format!("?role={role}&{pin}")).await;
        assert_eq!(status, StatusCode::OK, "{selected}");
        let payload = &selected["payload"];
        assert_eq!(payload["role"], role);
        assert!(payload.get("metricsScope").is_none());
        for field in [
            "indexGeneration",
            "indexRevision",
            "aggregate",
            "references",
            "edges",
            "rules",
            "layers",
        ] {
            assert_eq!(payload[field], baseline[field], "{role}: {field}");
        }
        assert_eq!(
            payload["modules"].as_array().unwrap().len(),
            baseline["modules"].as_array().unwrap().len()
        );
        let mixed = payload["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["identity"]["path"] == "src/roles.rs")
            .unwrap();
        assert_eq!(mixed["declarationCount"], count);
        assert_eq!(
            mixed["roleCounts"],
            serde_json::json!({"production":2,"test":1})
        );
        assert_eq!(mixed["physicalLines"], 3);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/checkouts/{key}/metrics?role={role}&aggregate=max&{pin}"
                    ))
                    .header("host", "127.0.0.1:7331")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap();
        let metrics: Value = serde_json::from_slice(&bytes).unwrap();
        for module in payload["modules"].as_array().unwrap() {
            let file = metrics["payload"]["files"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| {
                    row["sourceSetId"] == module["identity"]["sourceSetId"]
                        && row["language"] == module["identity"]["language"]
                        && row["path"] == module["identity"]["path"]
                })
                .unwrap();
            for field in ["metrics", "declarationCount", "roleCounts"] {
                assert_eq!(module[field], file[field], "{role}: {field}");
            }
        }
        if role == "production" {
            let test_only = payload["modules"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["identity"]["path"] == "tests/only.rs")
                .unwrap();
            assert_eq!(test_only["declarationCount"], 0);
            assert_eq!(
                test_only["roleCounts"],
                serde_json::json!({"production":0,"test":1})
            );
            assert!(test_only["metrics"]["cyclomatic"].is_null());
        }
    }
}

#[tokio::test]
async fn architecture_bounds_cancellation_deadline_and_final_root_fences() {
    let (base, _runtime, browser, key) = fixture().await;
    write_rules(&base, RULES);
    browser.set_metrics_controls_for_tests(20, None, false);
    let (status, bounded) = request(browser.clone().router(), &key, "").await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{bounded}");
    assert_eq!(bounded["error"]["code"], "response_too_large");
    browser.set_metrics_controls_for_tests(64 * 1024 * 1024, None, true);
    let (_status, cancelled) = request(browser.clone().router(), &key, "").await;
    assert_eq!(cancelled["error"]["code"], "cancelled");
    browser.set_metrics_controls_for_tests(64 * 1024 * 1024, Some(Duration::from_millis(1)), false);
    browser.set_metrics_clock_skew_for_tests(1001);
    let (_status, expired) = request(browser.clone().router(), &key, "").await;
    assert_eq!(expired["error"]["code"], "deadline_exceeded");
    browser.set_metrics_clock_skew_for_tests(0);
    browser.set_metrics_controls_for_tests(64 * 1024 * 1024, None, false);
    // Located errors also pass the owned final read/root fence.
    write_rules(&base, "version=[");
    let root = base.path().join("checkout");
    let moved = base.path().join("moved");
    let once = std::sync::atomic::AtomicBool::new(true);
    browser.set_selected_response_hook_for_tests(Arc::new(move |stage| {
        if stage == "before_read_finish" && once.swap(false, std::sync::atomic::Ordering::AcqRel) {
            fs::rename(&root, &moved).unwrap();
        }
    }));
    let (status, changed) = request(browser.clone().router(), &key, "").await;
    assert_eq!(status, StatusCode::CONFLICT, "{changed}");
    assert_eq!(changed["error"]["code"], "root_changed");
    assert!(changed["error"].get("kind").is_none());
}

#[tokio::test]
async fn workbench_architecture_option_forwards_hidden_layer_population() {
    let (base, _runtime, browser, key) = fixture().await;
    write_rules(
        &base,
        "version=1\n[layers.quiet]\norder=1\npaths=['**/*.js']\ndisplay='hidden'",
    );
    let app = browser.router();
    let (status, baseline) = request(app.clone(), &key, "?role=production&workbench=false").await;
    assert_eq!(status, StatusCode::OK, "{baseline}");
    assert_eq!(baseline["payload"]["workbench"], false);
    for role in ["production", "all"] {
        let (status, data) =
            request(app.clone(), &key, &format!("?role={role}&workbench=true")).await;
        assert_eq!(status, StatusCode::OK, "{data}");
        assert_eq!(data["payload"]["workbench"], true);
        assert_eq!(
            data["payload"]["references"],
            baseline["payload"]["references"]
        );
        let sample = data["payload"]["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["identity"]["path"] == "sample.js")
            .unwrap();
        assert_eq!(sample["declarationCount"], usize::from(role == "all"));
        if role == "production" {
            assert_eq!(sample["excludedReason"], "hidden_layer");
        }
    }
    for query in [
        "?workbench=",
        "?workbench=1",
        "?workbench=True",
        "?workbench=false&workbench=true",
    ] {
        let (status, data) = request(app.clone(), &key, query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {data}");
    }
}

/// Every detail page of one kind, followed from offset 0 with `limit`.
async fn all_details(app: &axum::Router, key: &str, query: &str, limit: usize) -> Vec<Value> {
    let (mut offset, mut items) = (0, Vec::new());
    loop {
        let (status, page) = request(
            app.clone(),
            key,
            &format!("/details?{query}&limit={limit}&offset={offset}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{query}: {page}");
        let page = &page["payload"];
        assert_eq!(page["offset"], offset);
        let rows = page["items"].as_array().unwrap();
        assert!(rows.len() <= limit);
        items.extend(rows.iter().cloned());
        match page["nextOffset"].as_u64() {
            Some(next) => {
                assert_eq!(next as usize, offset + rows.len());
                offset = next as usize;
            }
            None => {
                assert_eq!(page["total"], items.len());
                return items;
            }
        }
    }
}

/// #448: the summary keeps counts only; details are paged at the same pin and
/// rules hash, and together they equal `trellis rules check` / `references`.
#[tokio::test]
async fn architecture_summary_counts_equal_cli_and_details_page_at_the_same_pin() {
    let (base, runtime, browser, key) = fixture_with_roles(true).await;
    write_rules(
        &base,
        "version=1\n[layers.app]\norder=2\npaths=['src/roles.rs','tests/**']\n[layers.target]\norder=1\npaths=['src/target.rs']",
    );
    let app = browser.router();
    let (status, summary) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{summary}");
    let summary = &summary["payload"];
    for (field, absent) in [
        (
            "references",
            ["modules", "production", "tests", "unresolved"].as_slice(),
        ),
        (
            "rules",
            ["instances", "testOnly", "unassigned", "externalPackages"].as_slice(),
        ),
    ] {
        for name in absent {
            assert!(summary[field].get(*name).is_none(), "{field}.{name}");
        }
    }
    let pin: trellis::model::IndexPin = serde_json::from_value(serde_json::json!({
        "indexGeneration": summary["indexGeneration"], "indexRevision": summary["indexRevision"]
    }))
    .unwrap();
    let (response, _) = runtime.evidence_response().unwrap();
    let basis = serde_json::json!({ "expectedBasis": pin });
    let checked = response
        .architecture_response("check", &basis, &mut || Ok(()))
        .unwrap();
    let references = response
        .architecture_response("references", &basis, &mut || Ok(()))
        .unwrap();
    response.finish(()).unwrap();
    // The same counts `trellis rules check --format json` prints.
    assert_eq!(summary["rules"]["counts"], checked["counts"]);
    assert_eq!(summary["rules"]["testCounts"], checked["testCounts"]);
    assert_eq!(
        summary["rules"]["unassignedCount"],
        checked["unassigned"].as_array().unwrap().len()
    );
    assert_eq!(summary["references"]["counts"], references["counts"]);
    let hash = summary["rulesFile"]["contentHash"].as_str().unwrap();
    let pinned = format!(
        "indexGeneration={}&indexRevision={}&rulesHash={hash}",
        pin.index_generation, pin.index_revision
    );
    let instances = all_details(&app, &key, &format!("kind=instances&{pinned}"), 1).await;
    let expected: Vec<Value> = [("instances", "production"), ("testOnly", "test")]
        .into_iter()
        .flat_map(|(field, role)| {
            checked[field]
                .as_array()
                .unwrap()
                .iter()
                .map(move |row| serde_json::json!({"role":role,"instance":row}))
        })
        .collect();
    assert!(expected.len() > 1);
    assert_eq!(
        instances, expected,
        "pages concatenate to the CLI instances"
    );
    let pairs = all_details(&app, &key, &format!("kind=pairs&{pinned}"), 2).await;
    let edges: Vec<_> = [("production", "production"), ("tests", "test")]
        .into_iter()
        .flat_map(|(field, role)| {
            references[field]
                .as_array()
                .unwrap()
                .iter()
                .map(move |pair| (role, pair.clone()))
        })
        .collect();
    assert_eq!(pairs.len(), edges.len());
    for (item, (role, pair)) in pairs.iter().zip(&edges) {
        assert_eq!(item["role"], *role);
        assert_eq!(item["pair"], *pair);
    }
    let unresolved = all_details(&app, &key, &format!("kind=unresolved&{pinned}"), 1).await;
    assert_eq!(unresolved, *references["unresolved"].as_array().unwrap());
    // Filters are exact and keep evaluation order.
    let scoped = all_details(
        &app,
        &key,
        &format!("kind=instances&{pinned}&role=test&path=src%2Froles.rs"),
        50,
    )
    .await;
    assert!(!scoped.is_empty());
    assert!(scoped.iter().all(|row| row["role"] == "test"
        && (row["instance"]["target"]["path"] == "src/roles.rs"
            || row["instance"]["destination"]["path"] == "src/roles.rs")));
    let layered = all_details(&app, &key, &format!("kind=pairs&{pinned}&layer=target"), 50).await;
    assert!(!layered.is_empty());
    assert!(
        layered
            .iter()
            .all(|row| row["targetLayer"] == "target" || row["sourceLayer"] == "target")
    );
    let unpinned = format!(
        "kind=instances&indexGeneration={}&indexRevision={}",
        pin.index_generation, pin.index_revision
    );
    for query in [
        unpinned,
        format!("kind=bogus&{pinned}"),
        format!("kind=instances&{pinned}&limit=0"),
        format!("kind=instances&{pinned}&limit=501"),
        format!("kind=instances&{pinned}&offset=-1"),
        format!("kind=instances&{pinned}&offset=100000"),
        format!("kind=instances&{pinned}&role=all"),
        format!("kind=instances&{pinned}&status=bad"),
        format!("kind=instances&{pinned}&unexpected=1"),
        format!("kind=unresolved&{pinned}&role=production"),
        format!("kind=pairs&{pinned}&unlayered=false"),
        format!("kind=pairs&{pinned}&path="),
        format!("kind=pairs&{pinned}&kind=pairs"),
    ] {
        let (status, error) = request(app.clone(), &key, &format!("/details?{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {error}");
    }
    let stale_rules = format!(
        "/details?kind=instances&indexGeneration={}&indexRevision={}&rulesHash=sha256%3Aold",
        pin.index_generation, pin.index_revision
    );
    let (status, conflict) = request(app.clone(), &key, &stale_rules).await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["error"]["code"], "rules_conflict");
    let stale_pin = format!(
        "/details?kind=instances&indexGeneration={}&indexRevision={}&rulesHash={hash}",
        pin.index_generation,
        pin.index_revision + 1
    );
    let (status, conflict) = request(app.clone(), &key, &stale_pin).await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert!(
        matches!(
            conflict["error"]["code"].as_str(),
            Some("revision_conflict" | "pin_expired")
        ),
        "{conflict}"
    );
    // A pair endpoint filter is an exact identity, not a path.
    let pair = &edges[0].1;
    let exact = |identity: &Value| url_encode(&serde_json::to_string(identity).unwrap());
    let selected = all_details(
        &app,
        &key,
        &format!(
            "kind=pairs&{pinned}&role={}&source={}&target={}",
            edges[0].0,
            exact(&pair["source"]),
            exact(&pair["target"])
        ),
        5,
    )
    .await;
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0]["pair"], *pair);
    let mut other = pair["source"].clone();
    other["sourceSetId"] = Value::from("another-source-set");
    let none = all_details(
        &app,
        &key,
        &format!("kind=pairs&{pinned}&source={}", exact(&other)),
        5,
    )
    .await;
    assert!(
        none.is_empty(),
        "the same path in another source set is not selected"
    );
    let (status, _) = request(
        app.clone(),
        &key,
        &format!("/details?kind=pairs&{pinned}&source=src%2Froles.rs"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a bare path is not an identity"
    );
    // The rules file changing after the summary is a typed conflict, never a stale page.
    write_rules(
        &base,
        "version=1\n[layers.app]\norder=1\npaths=['src/roles.rs','tests/**']\n[layers.target]\norder=2\npaths=['src/target.rs']",
    );
    let (status, conflict) = request(
        app.clone(),
        &key,
        &format!("/details?kind=instances&{pinned}"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["error"]["code"], "rules_conflict");
}

/// #449: the summary carries exact per-layer site coverage (so drawn counts can be read as
/// lower bounds) and the authored exception count, bounded by the layers and equal to the
/// reference counts at the same pin.
#[tokio::test]
async fn architecture_summary_reports_layer_site_coverage_and_exception_count() {
    let (base, _runtime, browser, key) = fixture_with_roles(true).await;
    write_rules(
        &base,
        "version=1\n[layers.app]\norder=2\npaths=['src/roles.rs','tests/**']\n[layers.target]\norder=1\npaths=['src/target.rs']\n[[exceptions]]\nrule='$layers'\ntarget={language='rust',path='src/roles.rs'}\ndestination={language='rust',path='src/target.rs'}\nreason='Reviewed'",
    );
    let app = browser.router();
    let (status, summary) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{summary}");
    let summary = &summary["payload"];
    assert_eq!(summary["exceptionCount"], 1);
    let coverage = summary["references"]["coverage"].as_array().unwrap();
    let layers = summary["layers"].as_array().unwrap().len();
    assert!(!coverage.is_empty() && coverage.len() <= 2 * (layers + 1));
    let sum = |field: &str| {
        coverage
            .iter()
            .map(|row| row[field].as_u64().unwrap())
            .sum::<u64>()
    };
    let counts = &summary["references"]["counts"];
    assert_eq!(sum("mappedSites"), counts["mappedSites"].as_u64().unwrap());
    assert_eq!(
        sum("unresolvedSites"),
        counts["unresolvedSites"].as_u64().unwrap()
    );
    let mut keys = std::collections::BTreeSet::new();
    for row in coverage {
        assert!(
            row["layer"].is_null() || ["app", "target"].contains(&row["layer"].as_str().unwrap())
        );
        assert!(["production", "test"].contains(&row["role"].as_str().unwrap()));
        assert!(keys.insert((row["layer"].to_string(), row["role"].to_string())));
        let sites = row["sites"].as_u64().unwrap();
        let (mapped, unresolved) = (
            row["mappedSites"].as_u64().unwrap(),
            row["unresolvedSites"].as_u64().unwrap(),
        );
        assert!(
            sites > 0 && mapped <= sites && unresolved <= sites && sites <= mapped + unresolved,
            "{row}"
        );
    }
    // Both production references in src/roles.rs (plain and feature test-support) map to src/target.rs.
    let covered = coverage
        .iter()
        .find(|row| row["layer"] == "app" && row["role"] == "production")
        .unwrap();
    assert_eq!(covered["sites"], 2, "{covered}");
    assert_eq!(covered["mappedSites"], 2);
    assert_eq!(covered["unresolvedSites"], 0);
    let app_up: u64 = summary["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|edge| edge["sourceLayer"] == "app" && edge["role"] == "production")
        .map(|edge| edge["count"].as_u64().unwrap())
        .sum();
    assert_eq!(
        app_up, 2,
        "every mapped app site is drawn, so its arrow is exact"
    );
    // Coverage is exact detail, not a sample: the paged unresolved sites agree with it.
    let pin = format!(
        "indexGeneration={}&indexRevision={}&rulesHash={}",
        summary["indexGeneration"].as_str().unwrap(),
        summary["indexRevision"],
        url_encode(summary["rulesFile"]["contentHash"].as_str().unwrap())
    );
    let unresolved = all_details(&app, &key, &format!("kind=unresolved&{pin}"), 50).await;
    assert_eq!(unresolved.len() as u64, sum("unresolvedSites"));
    // Without rules there is no exception inventory to count.
    fs::remove_file(base.path().join("checkout/.trellis/rules.toml")).unwrap();
    let (status, missing) = request(app, &key, "").await;
    assert_eq!(status, StatusCode::OK, "{missing}");
    assert_eq!(missing["payload"]["state"], "missing_rules");
    assert!(missing["payload"]["exceptionCount"].is_null());
}

fn url_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}
