#![cfg(feature = "test-support")]
//! #556: one background Architecture build per revision and rules file, served to every read.
use super::architecture_http::{RULES, fixture, fixture_with_sources, request, write_rules};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;
use trellis::daemon::registry::MetricsResponseHooks;
use trellis::http::DaemonState;
use trellis::indexer::IndexOptions;
use trellis::store::MetricsOptions;

const OTHER_RULES: &str = "version=1\n[layers.other]\norder=1\npaths=['**/*.js']";

/// Holds the owner at one hook point until released, and reports that it got there.
struct Hold {
    reached: Mutex<mpsc::Receiver<()>>,
    release: Mutex<Option<mpsc::Sender<()>>>,
}
impl Hold {
    fn at(scheduler: &Arc<DaemonState>, point: &'static str) -> Arc<Self> {
        let (reached_tx, reached_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        scheduler.set_architecture_hook_for_tests(Arc::new(move |seen| {
            if seen == point {
                let _ = reached_tx.send(());
                let _ = release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(trellis::testing::HANG_BACKSTOP);
            }
        }));
        Arc::new(Self {
            reached: Mutex::new(reached_rx),
            release: Mutex::new(Some(release_tx)),
        })
    }
    /// `at`, also recording that the owner passed `marker`.
    fn at_with(
        scheduler: &Arc<DaemonState>,
        point: &'static str,
        seen_marker: Arc<std::sync::atomic::AtomicBool>,
        marker: &'static str,
    ) -> Arc<Self> {
        let (reached_tx, reached_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        scheduler.set_architecture_hook_for_tests(Arc::new(move |seen| {
            if seen == marker {
                seen_marker.store(true, std::sync::atomic::Ordering::Release);
            }
            if seen == point {
                let _ = reached_tx.send(());
                let _ = release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(trellis::testing::HANG_BACKSTOP);
            }
        }));
        Arc::new(Self {
            reached: Mutex::new(reached_rx),
            release: Mutex::new(Some(release_tx)),
        })
    }
    fn wait_reached(&self) {
        self.reached
            .lock()
            .unwrap()
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .expect("owner reached the hold");
    }
    fn release(&self) {
        self.release.lock().unwrap().take();
    }
}

fn head(payload: &Value) -> String {
    format!(
        "?indexGeneration={}&indexRevision={}",
        payload["indexGeneration"].as_str().unwrap(),
        payload["indexRevision"]
    )
}

#[tokio::test]
async fn the_first_read_after_publication_is_served_from_one_build() {
    let (base, runtime, browser, key) = fixture().await;
    write_rules(&base, RULES);
    let scheduler = runtime.browser_scheduler().unwrap();
    let app = browser.clone().router();
    // The owner starts on the publication hint alone: no read asked.
    let hold = Hold::at(&scheduler, "architecture_build_finished");
    scheduler.architecture_hint();
    hold.wait_reached();
    hold.release();
    assert_eq!(scheduler.architecture_builds_for_tests(), 1);
    let (status, first) = request(app.clone(), &key, "?role=production").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["payload"]["state"], "ready");
    assert_eq!(first["payload"]["modules"].as_array().unwrap().len(), 4);
    let (status, second) = request(app.clone(), &key, "?role=production").await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(first["payload"], second["payload"]);
    // Another role and aggregate is a projection of the same core.
    let (status, other) = request(app, &key, "?role=all&aggregate=mean").await;
    assert_eq!(status, StatusCode::OK, "{other}");
    assert_eq!(other["payload"]["aggregate"], "mean");
    assert_eq!(scheduler.architecture_builds_for_tests(), 1);
}

#[tokio::test]
async fn a_read_during_the_build_answers_building_with_progress_then_the_result() {
    let (base, runtime, browser, key) = fixture().await;
    write_rules(&base, RULES);
    let scheduler = runtime.browser_scheduler().unwrap();
    let app = browser.clone().router();
    let hold = Hold::at(&scheduler, "evaluating");
    scheduler.architecture_hint();
    hold.wait_reached();
    scheduler.set_architecture_browser_wait_for_tests(Duration::ZERO);

    let (status, building) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{building}");
    let payload = &building["payload"];
    assert_eq!(payload["state"], "building");
    assert_eq!(payload["progress"]["phase"], "evaluating");
    assert_eq!(payload["progress"]["total"], 4);
    assert_eq!(payload["retryAfterMs"], 250);
    let (status, retained) = request(app.clone(), &key, &head(payload)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{retained}");

    hold.release();
    scheduler.set_architecture_browser_wait_for_tests(trellis::testing::HANG_BACKSTOP);
    let (status, ready) = request(app, &key, &head(payload)).await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    assert_eq!(ready["payload"]["state"], "ready");
    assert_eq!(scheduler.architecture_builds_for_tests(), 1);
}

#[tokio::test]
async fn concurrent_readers_wait_for_one_build() {
    let (base, runtime, browser, key) = fixture().await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let app = browser.clone().router();
    // The build starts only once every reader is waiting on the owner.
    let waiting = Arc::new((Mutex::new(0usize), Condvar::new()));
    let hook_waiting = waiting.clone();
    scheduler.set_architecture_hook_for_tests(Arc::new(move |point| {
        let (count, changed) = &*hook_waiting;
        match point {
            "architecture_read_waiting" => {
                *count.lock().unwrap() += 1;
                changed.notify_all();
            }
            "architecture_build_started" => {
                let _ = changed
                    .wait_timeout_while(
                        count.lock().unwrap(),
                        trellis::testing::HANG_BACKSTOP,
                        |count| *count < 3,
                    )
                    .unwrap();
            }
            _ => {}
        }
    }));
    // Rules after the hook, so no build can start before it.
    write_rules(&base, RULES);
    let (a, b, c) = tokio::join!(
        request(app.clone(), &key, ""),
        request(app.clone(), &key, ""),
        request(app.clone(), &key, "")
    );
    for (status, data) in [&a, &b, &c] {
        assert_eq!(*status, StatusCode::OK, "{data}");
        assert_eq!(data["payload"], a.1["payload"]);
    }
    assert_eq!(*waiting.0.lock().unwrap(), 3);
    assert_eq!(scheduler.architecture_builds_for_tests(), 1);
}

#[tokio::test]
async fn a_rules_change_is_never_served_from_the_older_core() {
    let (base, runtime, browser, key) = fixture().await;
    write_rules(&base, RULES);
    let scheduler = runtime.browser_scheduler().unwrap();
    let app = browser.clone().router();
    let (status, first) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    write_rules(&base, OTHER_RULES);
    let (status, changed) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["payload"]["layers"][0]["name"], "other");
    assert_ne!(
        first["payload"]["rulesFile"]["contentHash"],
        changed["payload"]["rulesFile"]["contentHash"]
    );
    assert_eq!(scheduler.architecture_builds_for_tests(), 2);
    // A detail read at the older hash stays a conflict, and metrics did not need the rules.
    write_rules(&base, "version=[");
    let (status, invalid) = request(app, &key, "").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
    assert_eq!(invalid["error"]["code"], "invalid_rules");
}

#[tokio::test]
async fn a_new_revision_gets_its_own_build_and_an_older_pin_stays_readable() {
    let (base, runtime, browser, key) = fixture().await;
    write_rules(&base, RULES);
    let scheduler = runtime.browser_scheduler().unwrap();
    let app = browser.clone().router();
    let (status, first) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let root = base.path().join("checkout");
    std::fs::write(root.join("added.js"), "function added() {}\n").unwrap();
    let indexing = runtime.clone();
    tokio::task::spawn_blocking(move || indexing.cli_index(&IndexOptions::new(root)))
        .await
        .unwrap()
        .unwrap();
    let (status, next) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{next}");
    assert!(
        next["payload"]["indexRevision"].as_u64() > first["payload"]["indexRevision"].as_u64(),
        "{next}"
    );
    assert_eq!(next["payload"]["modules"].as_array().unwrap().len(), 5);
    let builds = scheduler.architecture_builds_for_tests();
    assert!(builds >= 2, "{builds}");
    // The owner keeps the core it replaced: a page still on that revision reads on, unchanged.
    let (status, older) = request(app, &key, &head(&first["payload"])).await;
    assert_eq!(status, StatusCode::OK, "{older}");
    assert_eq!(older["payload"], first["payload"]);
    let tool = runtime.clone();
    let pin = json!({
        "indexGeneration": first["payload"]["indexGeneration"],
        "indexRevision": first["payload"]["indexRevision"]
    });
    let (retained, expected) = tokio::task::spawn_blocking(move || {
        let (response, _) = tool.evidence_response().unwrap();
        (
            tool.architecture_tool(
                &response,
                "references",
                &json!({"expectedBasis": pin}),
                &mut || Ok(()),
            )
            .unwrap(),
            first["payload"]["indexRevision"].clone(),
        )
    })
    .await
    .unwrap();
    assert_eq!(retained["indexRevision"], expected);
    assert_eq!(scheduler.architecture_builds_for_tests(), builds);
}

/// Release removes a revision the owner still holds a core for: its pin stops resolving, typed,
/// on every route, and the core is not served in its place. The head is untouched.
#[tokio::test]
async fn a_released_revision_is_not_served_from_the_core_the_owner_kept() {
    let (base, runtime, browser, key) = fixture().await;
    write_rules(&base, RULES);
    let scheduler = runtime.browser_scheduler().unwrap();
    let app = browser.clone().router();
    let (status, first) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let root = base.path().join("checkout");
    std::fs::write(root.join("added.js"), "function added() {}\n").unwrap();
    let indexing = runtime.clone();
    tokio::task::spawn_blocking(move || indexing.cli_index(&IndexOptions::new(root)))
        .await
        .unwrap()
        .unwrap();
    let (status, next) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{next}");
    let older = head(&first["payload"]);
    let (status, kept) = request(app.clone(), &key, &older).await;
    assert_eq!(status, StatusCode::OK, "{kept}");
    let builds = scheduler.architecture_builds_for_tests();

    let pin: trellis::model::IndexPin = serde_json::from_value(json!({
        "indexGeneration": first["payload"]["indexGeneration"],
        "indexRevision": first["payload"]["indexRevision"]
    }))
    .unwrap();
    let releasing = runtime.clone();
    tokio::task::spawn_blocking(move || releasing.release_past_grace_for_tests(pin))
        .await
        .unwrap()
        .unwrap();

    let (status, gone) = request(app.clone(), &key, &older).await;
    assert_eq!(status, StatusCode::CONFLICT, "{gone}");
    assert_eq!(gone["error"]["code"], "pin_expired", "{gone}");
    let tool = runtime.clone();
    let basis = json!({
        "indexGeneration": first["payload"]["indexGeneration"],
        "indexRevision": first["payload"]["indexRevision"]
    });
    let error = tokio::task::spawn_blocking(move || {
        let (response, _) = tool.evidence_response().unwrap();
        tool.architecture_tool(
            &response,
            "references",
            &json!({"expectedBasis": basis}),
            &mut || Ok(()),
        )
        .unwrap_err()
    })
    .await
    .unwrap();
    assert!(error.to_string().starts_with("pin_expired"), "{error:#}");
    let (status, current) = request(app, &key, "").await;
    assert_eq!(status, StatusCode::OK, "{current}");
    assert_eq!(current["payload"], next["payload"]);
    assert_eq!(scheduler.architecture_builds_for_tests(), builds);
}

/// A page on an older revision nothing was built for gets it from the owner, after the head.
#[tokio::test]
async fn a_page_on_an_older_revision_gets_it_from_the_owner() {
    let (base, runtime, browser, key) = fixture().await;
    let app = browser.clone().router();
    // No rules yet, so this revision has no core.
    let (status, missing) = request(app.clone(), &key, "").await;
    assert_eq!(missing["payload"]["state"], "missing_rules", "{status}");
    let older = head(&missing["payload"]);
    let root = base.path().join("checkout");
    std::fs::write(root.join("added.js"), "function added() {}\n").unwrap();
    let indexing = runtime.clone();
    tokio::task::spawn_blocking(move || indexing.cli_index(&IndexOptions::new(root)))
        .await
        .unwrap()
        .unwrap();
    write_rules(&base, RULES);
    let scheduler = runtime.browser_scheduler().unwrap();
    let (status, read) = request(app.clone(), &key, &older).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["payload"]["modules"].as_array().unwrap().len(), 4);
    assert_eq!(
        read["payload"]["indexRevision"],
        missing["payload"]["indexRevision"]
    );
    assert_eq!(scheduler.architecture_builds_for_tests(), 2);
    // Both stay served, without another build.
    let (status, current) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{current}");
    assert_eq!(current["payload"]["modules"].as_array().unwrap().len(), 5);
    let (status, again) = request(app, &key, &older).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(scheduler.architecture_builds_for_tests(), 2);
}

/// A rules read with no rules file fails at once: it does not ask for or wait on a build.
#[tokio::test]
async fn a_rules_read_without_rules_does_not_wait_for_a_build() {
    let (_base, runtime, _browser, _key) = fixture().await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let tool = runtime.clone();
    let failed = tokio::task::spawn_blocking(move || {
        tool.await_architecture_tool(
            "check",
            &json!({}),
            trellis::testing::HANG_BACKSTOP,
            &mut || Ok(()),
        );
        let (response, _) = tool.evidence_response().unwrap();
        tool.architecture_tool(&response, "check", &json!({}), &mut || Ok(()))
            .unwrap_err()
    })
    .await
    .unwrap();
    assert_eq!(failed.to_string(), "rules_not_found");
    assert_eq!(scheduler.architecture_builds_for_tests(), 0);
}

/// A running build holds no read: the head can move, and the build gives way to it.
#[tokio::test]
async fn a_new_revision_supersedes_a_build_that_is_computing() {
    let (base, runtime, browser, key) = fixture().await;
    write_rules(&base, RULES);
    let scheduler = runtime.browser_scheduler().unwrap();
    let superseded = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hold = Hold::at_with(
        &scheduler,
        "evaluating",
        superseded.clone(),
        "architecture_build_superseded",
    );
    scheduler.architecture_hint();
    hold.wait_reached();
    assert_eq!(scheduler.leases_for_tests(), 0);
    let root = base.path().join("checkout");
    std::fs::write(root.join("added.js"), "function added() {}\n").unwrap();
    let indexing = runtime.clone();
    tokio::task::spawn_blocking(move || indexing.cli_index(&IndexOptions::new(root)))
        .await
        .unwrap()
        .unwrap();
    // The hint the worker gives at the end of that step, which may still be running.
    scheduler.architecture_hint();
    hold.release();
    let (status, next) = request(browser.clone().router(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{next}");
    assert_eq!(next["payload"]["modules"].as_array().unwrap().len(), 5);
    assert!(superseded.load(std::sync::atomic::Ordering::Acquire));
    assert!(scheduler.architecture_builds_for_tests() >= 2);
}

/// A read for a rules file newer than the running build waits for that file's build.
#[tokio::test]
async fn a_rules_edit_during_a_build_is_built_next() {
    let (base, runtime, browser, key) = fixture().await;
    write_rules(&base, RULES);
    let scheduler = runtime.browser_scheduler().unwrap();
    let hold = Hold::at(&scheduler, "evaluating");
    scheduler.architecture_hint();
    hold.wait_reached();
    write_rules(&base, OTHER_RULES);
    let app = browser.clone().router();
    let reading = tokio::spawn(async move { request(app, &key, "").await });
    hold.release();
    let (status, read) = reading.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["payload"]["layers"][0]["name"], "other");
    assert_eq!(scheduler.architecture_builds_for_tests(), 2);
}

#[tokio::test]
async fn a_panicked_build_is_reported_once_and_the_next_read_builds_again() {
    let (base, runtime, browser, key) = fixture().await;
    write_rules(&base, RULES);
    let scheduler = runtime.browser_scheduler().unwrap();
    let panics = Arc::new(std::sync::atomic::AtomicBool::new(true));
    scheduler.set_architecture_hook_for_tests(Arc::new(move |point| {
        if point == "resolving" && panics.swap(false, std::sync::atomic::Ordering::AcqRel) {
            panic!("injected build panic");
        }
    }));
    let app = browser.clone().router();
    let (status, failed) = request(app.clone(), &key, "").await;
    assert!(status.is_server_error(), "{status} {failed}");
    let (status, ready) = request(app, &key, "").await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    assert_eq!(scheduler.architecture_builds_for_tests(), 2);
}

#[tokio::test]
async fn a_failure_of_the_revision_is_kept_and_answered_typed() {
    let (base, runtime, browser, key) = fixture().await;
    // Valid rules that only evaluation finds ambiguous.
    write_rules(
        &base,
        "version=1\n[layers.a]\norder=1\npaths=['**/*.js']\n[layers.b]\norder=2\npaths=['**/*.js']",
    );
    let scheduler = runtime.browser_scheduler().unwrap();
    let app = browser.clone().router();
    for _ in 0..2 {
        let (status, failed) = request(app.clone(), &key, "").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{failed}");
        assert_eq!(failed["error"]["code"], "invalid_rules");
    }
    assert_eq!(scheduler.architecture_builds_for_tests(), 1);
}

#[tokio::test]
async fn a_tool_read_is_served_or_typed_retryable_within_its_wait() {
    let (base, runtime, _browser, _key) = fixture().await;
    write_rules(&base, RULES);
    let scheduler = runtime.browser_scheduler().unwrap();
    let hold = Hold::at(&scheduler, "architecture_build_started");
    scheduler.architecture_hint();
    hold.wait_reached();
    let tool = runtime.clone();
    let (building, scoped) = tokio::task::spawn_blocking(move || {
        let (response, _) = tool.evidence_response().unwrap();
        let read =
            |arguments| tool.architecture_tool(&response, "check", &arguments, &mut || Ok(()));
        (
            read(json!({})).unwrap_err(),
            read(json!({"paths":["sample.js"]})).unwrap(),
        )
    })
    .await
    .unwrap();
    // A narrow read need not wait: it computes its own scope while the build runs.
    assert_eq!(scoped["rulesVersion"], "rules:v2");
    assert!(building.to_string().starts_with("deadline_exceeded:"));
    let envelope = trellis::mcp::architecture::failure(&json!(1), &building);
    assert_eq!(envelope["error"]["code"], "deadline_exceeded");
    assert_eq!(envelope["error"]["retryable"], true);
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("being computed"),
        "{envelope}"
    );

    hold.release();
    let tool = runtime.clone();
    let whole = tokio::task::spawn_blocking(move || {
        tool.await_architecture_tool(
            "check",
            &json!({}),
            trellis::testing::HANG_BACKSTOP,
            &mut || Ok(()),
        );
        let (response, _) = tool.evidence_response().unwrap();
        tool.architecture_tool(&response, "check", &json!({}), &mut || Ok(()))
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(whole["rulesVersion"], "rules:v2");
    assert_eq!(scheduler.architecture_builds_for_tests(), 1);
}

#[tokio::test]
async fn metrics_from_a_published_core_answer_like_computed_metrics() {
    let (base, runtime, browser, key) = fixture().await;
    write_rules(&base, RULES);
    let scheduler = runtime.browser_scheduler().unwrap();
    let (status, ready) = request(browser.clone().router(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    let builds = scheduler.architecture_builds_for_tests();
    let source_set = ready["payload"]["modules"][0]["identity"]["sourceSetId"]
        .as_str()
        .unwrap()
        .to_owned();
    let read = runtime.clone();
    tokio::task::spawn_blocking(move || {
        for (scope, path, role, top) in [
            ("revision", None, "all", None),
            ("revision", None, "production", Some(3)),
            ("directory", Some("deep"), "all", None),
            ("file", Some("sample.js"), "all", None),
        ] {
            let options = MetricsOptions {
                scope: scope.into(),
                role: role.into(),
                workbench: true,
                aggregate: "max".into(),
                source_set_id: path.map(|_| source_set.clone()),
                language: (scope == "file").then(|| "javascript".to_owned()),
                path: path.map(str::to_owned),
                top_cognitive: top,
                ..MetricsOptions::default()
            };
            let (response, _) = read.evidence_response().unwrap();
            let computed =
                serde_json::to_value(response.metrics_at(&options, &mut || Ok(())).unwrap())
                    .unwrap();
            let (body, _) = read
                .metrics_response(
                    &options,
                    &mut || Ok(()),
                    MetricsResponseHooks {
                        before_work: &mut || {},
                        before_finish: &mut || {},
                        after_finish: &mut || {},
                        on_write: &mut |_| {},
                        on_progress: &mut || {},
                    },
                    64 * 1024 * 1024,
                )
                .unwrap();
            let served: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(served["payload"], computed, "{scope} {role}");
        }
    })
    .await
    .unwrap();
    assert_eq!(scheduler.architecture_builds_for_tests(), builds);
}

/// A read waits with no lease, so it never holds a publication's SQLite lock, and its
/// cancellation ends the wait.
#[tokio::test]
async fn a_waiting_read_holds_nothing_and_ends_on_cancellation() {
    let (base, runtime, _browser, _key) = fixture().await;
    write_rules(&base, RULES);
    let scheduler = runtime.browser_scheduler().unwrap();
    let hold = Hold::at(&scheduler, "evaluating");
    scheduler.architecture_hint();
    hold.wait_reached();
    let tool = runtime.clone();
    let observed = scheduler.clone();
    tokio::task::spawn_blocking(move || {
        let mut checks = 0;
        tool.await_architecture_tool(
            "check",
            &json!({}),
            trellis::testing::HANG_BACKSTOP,
            &mut || {
                assert_eq!(observed.leases_for_tests(), 0);
                checks += 1;
                anyhow::ensure!(checks < 2, "cancelled");
                Ok(())
            },
        );
        assert_eq!(checks, 2);
    })
    .await
    .unwrap();
    hold.release();
}

/// Owner ruling on #571: a revision over a whole-revision budget stays refused as a whole, and
/// a narrow read computes its own scope under the budgets of that scope.
#[tokio::test]
async fn an_over_budget_revision_still_answers_a_narrow_read() {
    // Every import is a reference site; the revision has more than the lowered budget.
    let wide = (0..12)
        .map(|i| format!("import {{ v as v{i} }} from './p.js';\n"))
        .collect::<String>();
    let (base, runtime, browser, key) = fixture_with_sources(
        false,
        &[
            ("p.js", "export function v() {}\n"),
            ("narrow.js", "import { v } from './p.js';\nv();\n"),
            ("wide.js", &wide),
        ],
    )
    .await;
    trellis::references::set_site_limit_for_tests(5);
    write_rules(&base, RULES);
    let scheduler = runtime.browser_scheduler().unwrap();
    let (status, whole) = request(browser.clone().router(), &key, "").await;
    assert!(!status.is_success(), "{status} {whole}");

    let tool = runtime.clone();
    let (references, rules, unscoped) = tokio::task::spawn_blocking(move || {
        let read = |kind: &str, arguments: Value| {
            tool.await_architecture_tool(
                kind,
                &arguments,
                trellis::testing::HANG_BACKSTOP,
                &mut || Ok(()),
            );
            let (response, _) = tool.evidence_response().unwrap();
            tool.architecture_tool(&response, kind, &arguments, &mut || Ok(()))
        };
        (
            read("references", json!({"paths":["narrow.js"]})).unwrap(),
            read("check", json!({"paths":["narrow.js"]})).unwrap(),
            read("check", json!({})).unwrap_err(),
        )
    })
    .await
    .unwrap();
    assert_eq!(references["counts"]["scannedDocuments"], 1);
    assert_eq!(references["production"][0]["source"]["path"], "narrow.js");
    assert_eq!(rules["rulesVersion"], "rules:v2");
    assert_eq!(rules["instances"][0]["target"]["path"], "narrow.js");
    assert!(
        unscoped.to_string().starts_with("bounded_work"),
        "{unscoped:#}"
    );
    // The failure is kept: the narrow reads asked for no further build.
    assert_eq!(scheduler.architecture_builds_for_tests(), 1);
}
