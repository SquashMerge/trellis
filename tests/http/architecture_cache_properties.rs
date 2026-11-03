#![cfg(feature = "test-support")]
//! Cache differential, not an independent oracle for the shared architecture algorithms.
//! Every fresh read invokes a new core for the identical full pin, live rules and options.
use super::architecture_http::{fixture_with_sources, request, write_rules};
use axum::http::StatusCode;
use proptest::prelude::*;
use serde_json::{Value, json};
use std::sync::Arc;
use trellis::daemon::registry::{CheckoutRuntime, MetricsResponseHooks};
use trellis::indexer::IndexOptions;
use trellis::model::IndexPin;
use trellis::store::MetricsOptions;

const A: &str = "version=1\n[layers.app]\norder=2\npaths=['src/roles.rs']\n[layers.target]\norder=1\npaths=['src/target.rs']";
const B: &str = "version=1\n[layers.quiet]\norder=1\npaths=['src/roles.rs']\ndisplay='hidden'\n[layers.target]\norder=2\npaths=['src/target.rs']";
const ROLES: &str = "fn production(x: bool) { if x { crate::target::run(); } }\nfn simple() {}\n#[cfg(test)] mod tests { fn check() { crate::target::run(); } }\n";

#[derive(Clone, Debug)]
struct Choice {
    role: u8,
    aggregate: u8,
    workbench: bool,
    old: bool,
}

#[derive(Clone, Debug)]
enum Op {
    Rules(bool),
    Read(Choice),
}

fn options(pin: IndexPin, choice: &Choice) -> MetricsOptions {
    MetricsOptions {
        pin: Some(pin),
        role: ["production", "all", "test"][choice.role as usize].into(),
        aggregate: ["max", "sum", "mean"][choice.aggregate as usize].into(),
        workbench: choice.workbench,
        ..MetricsOptions::default()
    }
}

fn pin(payload: &Value) -> IndexPin {
    serde_json::from_value(json!({"indexGeneration": payload["indexGeneration"], "indexRevision": payload["indexRevision"]})).unwrap()
}

async fn compare(
    runtime: Arc<CheckoutRuntime>,
    app: axum::Router,
    key: &str,
    pin: IndexPin,
    choice: &Choice,
) -> Value {
    let options = options(pin, choice);
    let query = format!(
        "?indexGeneration={}&indexRevision={}&role={}&aggregate={}&workbench={}",
        pin.index_generation,
        pin.index_revision,
        options.role,
        options.aggregate,
        options.workbench
    );
    let (status, served) = request(app, key, &query).await;
    assert_eq!(status, StatusCode::OK, "{choice:?}: {served}");
    let (fresh, metrics, fresh_metrics) = tokio::task::spawn_blocking(move || {
        let (read, _) = runtime.evidence_response().unwrap();
        let fresh = read.architecture_view(&options, &mut || Ok(())).unwrap();
        let fresh_metrics =
            serde_json::to_value(read.metrics_at(&options, &mut || Ok(())).unwrap()).unwrap();
        read.finish(()).unwrap();
        let (body, _) = runtime
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
        (
            fresh,
            serde_json::from_slice::<Value>(&body).unwrap()["payload"].clone(),
            fresh_metrics,
        )
    })
    .await
    .unwrap();
    assert_eq!(
        served["payload"], fresh,
        "architecture cache differential: {choice:?}"
    );
    assert_eq!(
        metrics, fresh_metrics,
        "metrics cache differential: {choice:?}"
    );
    fresh
}

async fn scenario(operations: Vec<Op>) {
    let (base, runtime, browser, key) = fixture_with_sources(
        false,
        &[
            ("src/lib.rs", "mod roles; mod target;\n"),
            ("src/roles.rs", ROLES),
            ("src/target.rs", "pub fn run() {}\n"),
        ],
    )
    .await;
    let app = browser.router();
    let scheduler = runtime.browser_scheduler().unwrap();
    write_rules(&base, A);
    let (status, first) = request(
        app.clone(),
        &key,
        "?role=production&aggregate=max&workbench=false",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let old = pin(&first["payload"]);
    let control = Choice {
        role: 0,
        aggregate: 0,
        workbench: false,
        old: true,
    };
    let a = compare(runtime.clone(), app.clone(), &key, old, &control).await;
    let builds = scheduler.architecture_builds_for_tests();
    compare(runtime.clone(), app.clone(), &key, old, &control).await;
    assert_eq!(
        scheduler.architecture_builds_for_tests(),
        builds,
        "repeated owner read must reach cache"
    );

    write_rules(&base, B);
    let b = compare(runtime.clone(), app.clone(), &key, old, &control).await;
    assert_ne!(a["rulesFile"]["contentHash"], b["rulesFile"]["contentHash"]);
    assert_ne!(a["layers"], b["layers"], "rules change must be observable");
    let hash = a["rulesFile"]["contentHash"]
        .as_str()
        .unwrap()
        .replace(':', "%3A");
    let (status, conflict) = request(
        app.clone(),
        &key,
        &format!(
            "/details?kind=instances&indexGeneration={}&indexRevision={}&rulesHash={hash}",
            old.index_generation, old.index_revision
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["error"]["code"], "rules_conflict");

    let root = base.path().join("checkout");
    std::fs::write(
        root.join("src/target.rs"),
        "pub fn run() {}\npub fn added(x: bool) { if x { run(); } }\n",
    )
    .unwrap();
    let indexing = runtime.clone();
    tokio::task::spawn_blocking(move || indexing.cli_index(&IndexOptions::new(root)))
        .await
        .unwrap()
        .unwrap();
    let (status, current) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{current}");
    let new = pin(&current["payload"]);
    assert_ne!(old, new);
    let newer = compare(runtime.clone(), app.clone(), &key, new, &control).await;
    let older = compare(runtime.clone(), app.clone(), &key, old, &control).await;
    let target_count = |payload: &Value| {
        payload["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["identity"]["path"] == "src/target.rs")
            .unwrap()["declarationCount"]
            .clone()
    };
    assert_eq!(target_count(&older), 1);
    assert_eq!(
        target_count(&newer),
        2,
        "revision must change indexed facts"
    );
    assert_eq!(older["layers"], b["layers"], "old pin uses current rules");

    // Force each option dimension across the same populated core even if shrinking removes
    // every generated choice. These controls make summary-key mutants observable.
    let mut controls = Vec::new();
    for choice in [
        Choice {
            role: 1,
            ..control.clone()
        },
        Choice {
            role: 2,
            ..control.clone()
        },
        Choice {
            aggregate: 1,
            ..control.clone()
        },
        Choice {
            aggregate: 2,
            ..control.clone()
        },
        Choice {
            workbench: true,
            ..control.clone()
        },
    ]
    .iter()
    {
        let result = compare(
            runtime.clone(),
            app.clone(),
            &key,
            if choice.old { old } else { new },
            choice,
        )
        .await;
        controls.push(result);
    }
    let module = |payload: &Value| {
        payload["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["identity"]["path"] == "src/roles.rs")
            .unwrap()
            .clone()
    };
    let production = module(&older);
    assert_ne!(
        production["declarationCount"],
        module(&controls[0])["declarationCount"],
        "role must change selected callables"
    );
    assert_ne!(
        production["metrics"],
        module(&controls[2])["metrics"],
        "aggregate must change measured values"
    );
    assert_ne!(
        production["declarationCount"],
        module(&controls[4])["declarationCount"],
        "workbench must change the hidden-layer population"
    );
    for op in operations {
        match op {
            Op::Rules(choose_b) => write_rules(&base, if choose_b { B } else { A }),
            Op::Read(choice) => {
                compare(
                    runtime.clone(),
                    app.clone(),
                    &key,
                    if choice.old { old } else { new },
                    &choice,
                )
                .await;
            }
        }
    }
}

#[test]
fn deterministic_cache_differential_witness() {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(scenario(vec![]));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_PROPTEST_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(4), ..ProptestConfig::default() })]
    #[test]
    fn generated_cache_histories(operations in prop::collection::vec(prop_oneof![
        any::<bool>().prop_map(Op::Rules),
        (0u8..3, 0u8..3, any::<bool>(), any::<bool>()).prop_map(|(role, aggregate, workbench, old)| Op::Read(Choice { role, aggregate, workbench, old })),
    ], 0..8)) {
        tokio::runtime::Runtime::new().unwrap().block_on(scenario(operations));
    }
}
