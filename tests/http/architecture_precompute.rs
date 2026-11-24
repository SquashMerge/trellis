#![cfg(feature = "test-support")]
//! #556: one background Architecture build per revision and rules file, served to every read.
//! The fixture's activation has already built its head's core (#798), so each test counts the
//! builds it starts from `before`.
use super::architecture_http::{
    RULES, fixture, fixture_with_sources, request, summary, write_rules,
};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;
use trellis::daemon::registry::MetricsResponseHooks;
use trellis::http::DaemonState;
use trellis::http::{
    ArchitectureAttempt, ArchitectureBuildPath, ArchitectureCoreHandle, FullReason,
};
use trellis::indexer::IndexOptions;
use trellis::metrics_read::MetricsOptions;
use trellis::model::IndexPin;

const NATIVE_SOURCES: &[(&str, &str)] = &[
    ("src/main.rs", "fn main() { let _value = 1; }\n"),
    (
        "src/lib.rs",
        "pub mod worker; pub mod target; pub mod clean;\n",
    ),
    ("src/worker.rs", "pub fn work() { crate::target::run(); }\n"),
    ("src/target.rs", "pub fn run() {}\n"),
    ("src/clean.rs", "pub fn clean() {}\n"),
];

const PACKAGE_SOURCES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nrenamed={package='vendor',version='1'}\n[dev-dependencies]\nrenamed={package='vendor',version='1'}\n",
    ),
    ("src/main.rs", "fn main() { let _value = 1; }\n"),
    (
        "src/lib.rs",
        "pub mod worker; pub mod target; pub mod clean;\npub fn library() { renamed::run(); std::mem::drop(1); }\n",
    ),
    (
        "src/worker.rs",
        "pub fn work() { renamed::run(); std::mem::drop(1); }\n",
    ),
    ("src/target.rs", "pub fn run() {}\n"),
    (
        "src/clean.rs",
        "pub fn clean() { renamed::run(); std::mem::drop(1); }\n",
    ),
    (
        "package.json",
        "{\"dependencies\":{\"shared\":\"1.0.0\"},\"devDependencies\":{\"shared\":\"1.0.0\"}}",
    ),
    (
        "root.js",
        "import shared from 'shared'; import another from 'shared'; import fs from 'node:fs'; shared(); another(); fs.readFile('a');\n",
    ),
    (
        "other.js",
        "import shared from 'shared'; import second from 'shared'; shared(); second();\n",
    ),
    (
        "typed.ts",
        "import shared from 'shared'; import fs from 'node:fs'; shared(); fs.readFile('a');\n",
    ),
    (
        "tools/package.json",
        "{\"dependencies\":{\"shared\":\"2.0.0\"},\"optionalDependencies\":{\"shared\":\"2.0.0\"}}",
    ),
    (
        "tools/nested.js",
        "import shared from 'shared'; shared();\n",
    ),
    (
        "Example.java",
        "import java.util.ArrayList; class Example { void run() { new ArrayList(); } }\n",
    ),
    ("example.py", "import json\njson.loads('{}')\n"),
];

async fn index_edit(
    base: &tempfile::TempDir,
    runtime: &Arc<trellis::daemon::registry::CheckoutRuntime>,
    edits: &[(&str, Option<&str>)],
) -> IndexPin {
    let pin = publish_edit(base, runtime, edits).await;
    runtime.settled_for_tests(Some(pin)).await.unwrap();
    pin
}

async fn publish_edit(
    base: &tempfile::TempDir,
    runtime: &Arc<trellis::daemon::registry::CheckoutRuntime>,
    edits: &[(&str, Option<&str>)],
) -> IndexPin {
    let root = base.path().join("checkout");
    runtime.hold_worker_step_for_tests(|| {
        for (path, text) in edits {
            let path = root.join(path);
            match text {
                Some(text) => {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, text).unwrap();
                }
                None => std::fs::remove_file(path).unwrap(),
            }
        }
    });
    let indexing = runtime.clone();
    let publication =
        tokio::task::spawn_blocking(move || indexing.cli_index(&IndexOptions::new(root)))
            .await
            .unwrap()
            .unwrap();
    serde_json::from_value(publication["publishedRevision"].clone()).unwrap()
}

async fn reference_answer(
    runtime: &Arc<trellis::daemon::registry::CheckoutRuntime>,
    pin: IndexPin,
    paths: &[&str],
    prefix: Option<&str>,
) -> Value {
    let runtime = runtime.clone();
    let work_limit = trellis::references::ReferenceWorkLimitForTests::capture();
    let mut args = json!({"expectedBasis": pin, "paths": paths});
    if let Some(prefix) = prefix {
        args["prefix"] = json!(prefix);
    }
    tokio::task::spawn_blocking(move || {
        let _work_limit = work_limit.apply();
        runtime.await_architecture_tool(
            "references",
            &args,
            trellis::testing::HANG_BACKSTOP,
            &mut || Ok(()),
        );
        let (response, _) = runtime.evidence_response().unwrap();
        let answer = runtime
            .architecture_tool(&response, "references", &args, &mut || Ok(()))
            .unwrap();
        response.finish(answer).unwrap()
    })
    .await
    .unwrap()
}

async fn independent_full(
    runtime: &Arc<trellis::daemon::registry::CheckoutRuntime>,
    pin: IndexPin,
) -> Value {
    let runtime = runtime.clone();
    let work_limit = trellis::references::ReferenceWorkLimitForTests::capture();
    tokio::task::spawn_blocking(move || {
        let _work_limit = work_limit.apply();
        let (response, _) = runtime.evidence_response().unwrap();
        let full = response.architecture_full_for_tests(pin).unwrap();
        response.finish(full).unwrap()
    })
    .await
    .unwrap()
}

fn current_pin(scheduler: &DaemonState) -> IndexPin {
    scheduler
        .architecture_build_paths_for_tests()
        .last()
        .unwrap()
        .0
}

fn published_attempt(scheduler: &DaemonState, pin: IndexPin) -> ArchitectureAttempt {
    let attempt = scheduler
        .architecture_attempts_for_tests()
        .into_iter()
        .rev()
        .find(|attempt| attempt.pin == pin && attempt.published)
        .unwrap();
    assert!(attempt.compared, "{attempt:?}");
    assert_eq!(
        scheduler.architecture_build_paths_for_tests().last(),
        Some(&(pin, attempt.path.clone()))
    );
    assert!(scheduler.architecture_differences_for_tests().is_empty());
    attempt
}

fn core(scheduler: &DaemonState, pin: IndexPin) -> ArchitectureCoreHandle {
    scheduler.architecture_core_for_tests(pin).unwrap()
}

fn source_paths(keys: &[trellis::native_records::DocumentKey]) -> Vec<&str> {
    keys.iter().map(|key| key.path.as_str()).collect()
}

fn assert_incremental(attempt: &ArchitectureAttempt, dirty: &[&str], patched: &[&str]) {
    match &attempt.path {
        ArchitectureBuildPath::Incremental {
            dirty: actual,
            patched: patches,
            reused,
        } => {
            assert_eq!(source_paths(actual), dirty);
            assert_eq!(source_paths(patches), patched);
            assert!(!reused.is_empty());
            let partition: std::collections::BTreeSet<_> =
                actual.iter().chain(patches).chain(reused).collect();
            let admitted: std::collections::BTreeSet<_> = attempt
                .references
                .batches
                .iter()
                .flat_map(|(keys, _)| keys)
                .collect();
            assert_eq!(
                partition, admitted,
                "the path partitions every admitted source"
            );
            assert_eq!(partition.len(), actual.len() + patches.len() + reused.len());
            assert_eq!(partition.len(), attempt.progress.first().unwrap().total);
            assert_eq!(source_paths(&attempt.references.file_references), dirty);
        }
        path => panic!("expected incremental, got {path:?}"),
    }
}

#[tokio::test]
async fn architecture_incremental_body_edit() {
    let (base, runtime, _browser, _key) =
        super::architecture_http::fixture_exact_sources(NATIVE_SOURCES).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let first = current_pin(&scheduler);
    let initial = published_attempt(&scheduler, first);
    assert_eq!(initial.base, None);
    assert_eq!(
        initial.path,
        ArchitectureBuildPath::Full(FullReason::NoBase)
    );
    let before = core(&scheduler, first);
    let pin = index_edit(
        &base,
        &runtime,
        &[("src/main.rs", Some("fn main() { let _value = 2; }\n"))],
    )
    .await;
    let attempt = published_attempt(&scheduler, pin);
    assert_eq!(attempt.base, Some(first));
    assert_incremental(&attempt, &["src/main.rs"], &[]);
    assert_eq!(source_paths(&attempt.semantic_sources), ["src/main.rs"]);
    assert_eq!(source_paths(&attempt.measured_sources), ["src/main.rs"]);
    let after = core(&scheduler, pin);
    assert_eq!(
        after.canonical().unwrap(),
        independent_full(&runtime, pin).await
    );
    for (key, sharing) in after.sharing(&before) {
        if key.path == "src/main.rs" {
            assert!(!sharing.source && !sharing.metric && !sharing.dependency);
        } else {
            assert!(
                sharing.source
                    && sharing.metric
                    && sharing.dependency
                    && sharing.fingerprint
                    && sharing.references
                    && sharing.endpoints
                    && sharing.plan,
                "{key:?}: {sharing:?}"
            );
        }
    }
    let measured: Vec<_> = attempt
        .progress
        .iter()
        .filter(|value| value.phase == "measuring")
        .map(|value| (value.completed, value.total))
        .collect();
    assert_eq!(measured, [(0, 5), (4, 5), (5, 5)]);
    let phases: Vec<_> =
        attempt
            .progress
            .iter()
            .map(|value| value.phase)
            .fold(Vec::new(), |mut phases, phase| {
                if phases.last() != Some(&phase) {
                    phases.push(phase);
                }
                phases
            });
    assert_eq!(phases, ["reading", "resolving", "evaluating", "measuring"]);
}

#[tokio::test]
async fn architecture_incremental_declaration_edit() {
    let (base, runtime, _, _) =
        super::architecture_http::fixture_exact_sources(NATIVE_SOURCES).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let before = core(&scheduler, current_pin(&scheduler))
        .canonical()
        .unwrap();
    let pin = index_edit(
        &base,
        &runtime,
        &[("src/target.rs", Some("pub fn renamed() {}\n"))],
    )
    .await;
    let attempt = published_attempt(&scheduler, pin);
    assert_eq!(
        attempt.path,
        ArchitectureBuildPath::Full(FullReason::RustCatalogChanged)
    );
    let after = core(&scheduler, pin).canonical().unwrap();
    let target = before["dependencies"]["order"]
        .as_array()
        .unwrap()
        .iter()
        .position(|key| key["path"] == "src/target.rs")
        .unwrap();
    assert_ne!(
        before["dependencies"]["documents"][target]["sids"],
        after["dependencies"]["documents"][target]["sids"]
    );
}

#[tokio::test]
async fn architecture_incremental_add_delete_and_rename() {
    let (base, runtime, _, _) =
        super::architecture_http::fixture_exact_sources(NATIVE_SOURCES).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    for edits in [
        vec![("src/added.rs", Some("pub fn added() {}\n"))],
        vec![("src/added.rs", None)],
        vec![
            ("src/clean.rs", None),
            ("src/renamed.rs", Some("pub fn clean() {}\n")),
        ],
    ] {
        let pin = index_edit(&base, &runtime, &edits).await;
        let attempt = published_attempt(&scheduler, pin);
        assert_eq!(
            attempt.path,
            ArchitectureBuildPath::Full(FullReason::DocumentSetChanged)
        );
        assert_eq!(
            core(&scheduler, pin).canonical().unwrap(),
            independent_full(&runtime, pin).await
        );
    }
}

#[tokio::test]
async fn architecture_incremental_retention() {
    let (base, runtime, _, _) =
        super::architecture_http::fixture_exact_sources(NATIVE_SOURCES).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let first = current_pin(&scheduler);
    let old = core(&scheduler, first);
    let weak = old.downgrade();
    let frozen = old.canonical().unwrap();
    let second = index_edit(
        &base,
        &runtime,
        &[("src/main.rs", Some("fn main() { let _value = 2; }\n"))],
    )
    .await;
    assert_incremental(
        &published_attempt(&scheduler, second),
        &["src/main.rs"],
        &[],
    );
    let middle = core(&scheduler, second);
    assert_eq!(old.canonical().unwrap(), frozen);
    assert!(
        middle
            .sharing(&old)
            .iter()
            .filter(|(key, _)| key.path != "src/main.rs")
            .all(|(_, sharing)| sharing.source
                && sharing.metric
                && sharing.dependency
                && sharing.plan)
    );
    drop(old);
    assert!(weak.is_alive(), "the prior core is retained");
    let third = index_edit(
        &base,
        &runtime,
        &[("src/main.rs", Some("fn main() { let _value = 3; }\n"))],
    )
    .await;
    let attempt = published_attempt(&scheduler, third);
    assert_eq!(attempt.base, Some(second));
    assert_incremental(&attempt, &["src/main.rs"], &[]);
    assert!(
        !weak.is_alive(),
        "no plan, oracle or trace retains the evicted core"
    );
    assert!(scheduler.architecture_core_for_tests(first).is_none());
    assert!(scheduler.architecture_core_for_tests(second).is_some());
    assert!(scheduler.architecture_core_for_tests(third).is_some());
}

#[tokio::test]
async fn architecture_incremental_base_selection_older_requests() {
    let mut sources = NATIVE_SOURCES.to_vec();
    sources.push((".trellis/rules.toml", RULES));
    let (base, runtime, browser, key) =
        super::architecture_http::fixture_exact_sources(&sources).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let first = current_pin(&scheduler);
    scheduler.set_architecture_on_demand_for_tests();
    let older = index_edit(
        &base,
        &runtime,
        &[("src/main.rs", Some("fn main() { let _value = 2; }\n"))],
    )
    .await;
    assert!(scheduler.architecture_core_for_tests(older).is_none());
    let head = index_edit(
        &base,
        &runtime,
        &[("src/main.rs", Some("fn main() { let _value = 3; }\n"))],
    )
    .await;
    reference_answer(&runtime, head, &[], None).await;
    runtime.settled_for_tests(Some(head)).await.unwrap();
    let head_build = published_attempt(&scheduler, head);
    assert_eq!(head_build.base, Some(first));
    assert_incremental(&head_build, &["src/main.rs"], &[]);
    // Older tool reads compute in their admitted read; a page explicitly demands
    // the owner's retained result instead.
    let query = format!(
        "?indexGeneration={}&indexRevision={}",
        older.index_generation, older.index_revision
    );
    let (status, answer) = summary(browser.router(), &key, &query).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    runtime.settled_for_tests(Some(head)).await.unwrap();
    let old_build = published_attempt(&scheduler, older);
    assert_eq!(old_build.base, None);
    assert_eq!(
        old_build.path,
        ArchitectureBuildPath::Full(FullReason::NoBase)
    );
    assert!(
        scheduler.architecture_core_for_tests(head).is_some(),
        "an older request never evicts the head"
    );
    let next = index_edit(
        &base,
        &runtime,
        &[("src/main.rs", Some("fn main() { let _value = 4; }\n"))],
    )
    .await;
    let next_build = published_attempt(&scheduler, next);
    assert_eq!(
        next_build.base,
        Some(head),
        "choose the newest revision, not the most recently published older request"
    );
    assert_incremental(&next_build, &["src/main.rs"], &[]);
}

#[tokio::test]
async fn architecture_incremental_rules_change_does_not_resurrect_a_base() {
    let (base, runtime, browser, key) =
        super::architecture_http::fixture_exact_sources(NATIVE_SOURCES).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let app = browser.router();
    for rules in [RULES, OTHER_RULES, RULES] {
        write_rules(&base, rules);
        scheduler.architecture_hint();
        let (status, body) = summary(app.clone(), &key, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        runtime.settled_for_tests(None).await.unwrap();
        let attempt = published_attempt(&scheduler, current_pin(&scheduler));
        assert_eq!(
            attempt.path,
            ArchitectureBuildPath::Full(FullReason::NoBase)
        );
        assert_eq!(attempt.base, None);
        let text = if rules == OTHER_RULES {
            "fn main() { let _value = 3; }\n"
        } else {
            "fn main() { let _value = 2; }\n"
        };
        let pin = index_edit(&base, &runtime, &[("src/main.rs", Some(text))]).await;
        assert_incremental(&published_attempt(&scheduler, pin), &["src/main.rs"], &[]);
    }
}

#[tokio::test]
async fn architecture_incremental_native_non_rust_and_tsconfig_gates() {
    let mut sources = NATIVE_SOURCES.to_vec();
    sources.push(("typed.ts", "export function typed() { return 1; }\n"));
    let (base, runtime, _, _) = super::architecture_http::fixture_exact_sources(&sources).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    for (path, text, reason) in [
        (
            "typed.ts",
            "export function typed() { return 2; }\n",
            FullReason::NonRustChanged,
        ),
        (
            "tsconfig.json",
            "{\"compilerOptions\":{\"baseUrl\":\".\"}}",
            FullReason::RootTsconfigChanged,
        ),
        (
            "tsconfig.json",
            "{\"compilerOptions\":{\"baseUrl\":\"src\"}}",
            FullReason::RootTsconfigChanged,
        ),
    ] {
        let pin = index_edit(&base, &runtime, &[(path, Some(text))]).await;
        assert_eq!(
            published_attempt(&scheduler, pin).path,
            ArchitectureBuildPath::Full(reason)
        );
    }
    let pin = index_edit(
        &base,
        &runtime,
        &[(
            "jsconfig.json",
            Some("{\"compilerOptions\":{\"baseUrl\":\".\"}}"),
        )],
    )
    .await;
    assert_incremental(&published_attempt(&scheduler, pin), &[], &[]);
    for path in ["tsconfig.json", "jsconfig.json"] {
        let pin = index_edit(&base, &runtime, &[(path, None)]).await;
        assert_eq!(
            published_attempt(&scheduler, pin).path,
            ArchitectureBuildPath::Full(FullReason::RootTsconfigChanged)
        );
    }
}

#[tokio::test]
async fn architecture_incremental_dirty_majority_is_strict() {
    let mut sources = NATIVE_SOURCES.to_vec();
    sources.push(("src/idle.rs", "pub fn idle() {}\n"));
    let (base, runtime, _, _) = super::architecture_http::fixture_exact_sources(&sources).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let pin = index_edit(
        &base,
        &runtime,
        &[
            ("src/main.rs", Some("fn main() { let _value = 2; }\n")),
            ("src/clean.rs", Some("pub fn clean() { 2; }\n")),
            ("src/idle.rs", Some("pub fn idle() { 2; }\n")),
        ],
    )
    .await;
    assert_incremental(
        &published_attempt(&scheduler, pin),
        &["src/clean.rs", "src/idle.rs", "src/main.rs"],
        &[],
    );
    let pin = index_edit(
        &base,
        &runtime,
        &[
            ("src/main.rs", Some("fn main() { let _value = 3; }\n")),
            ("src/clean.rs", Some("pub fn clean() { 3; }\n")),
            ("src/idle.rs", Some("pub fn idle() { 3; }\n")),
            (
                "src/worker.rs",
                Some("pub fn work() { 3; crate::target::run(); }\n"),
            ),
        ],
    )
    .await;
    assert_eq!(
        published_attempt(&scheduler, pin).path,
        ArchitectureBuildPath::Full(FullReason::DirtyMajority)
    );
}

#[tokio::test]
async fn architecture_incremental_catalog_cases() {
    for (before, after, reason) in [
        ("fn main() { 1; }\n", "fn main() { 2; }\n", None),
        (
            "const MARKER: u8 = 1;\nfn main() {}\n",
            "const MARKER: u8 = 1;\n// shifted\n\nfn main() {}\n",
            None,
        ),
        (
            "fn main() {}\n",
            "// shifted\n\nfn main() {}\n",
            // Rank encoding preserves equal positions: leading trivia splits the
            // file-start/declaration-start tie, unlike the preceding position shift.
            Some(FullReason::RustCatalogChanged),
        ),
        (
            "fn main() {}\n",
            "fn main() {}\nfn added() {}\n",
            Some(FullReason::RustCatalogChanged),
        ),
        (
            "fn main() {}\n",
            "use std::fmt;\nfn main() {}\n",
            Some(FullReason::RustCatalogChanged),
        ),
        (
            "fn main() {}\n",
            "mod added {}\nfn main() {}\n",
            Some(FullReason::RustCatalogChanged),
        ),
        (
            "fn main() {}\n",
            "macro_rules! added { () => {} }\nfn main() {}\n",
            Some(FullReason::RustCatalogChanged),
        ),
        (
            "enum Kind { A }\nfn main() {}\n",
            "enum Kind { A, B }\nfn main() {}\n",
            Some(FullReason::RustCatalogChanged),
        ),
    ] {
        let mut sources = NATIVE_SOURCES.to_vec();
        sources[0].1 = before;
        let (base, runtime, _, _) = super::architecture_http::fixture_exact_sources(&sources).await;
        let scheduler = runtime.browser_scheduler().unwrap();
        let pin = index_edit(&base, &runtime, &[("src/main.rs", Some(after))]).await;
        let attempt = published_attempt(&scheduler, pin);
        match reason {
            Some(reason) => assert_eq!(attempt.path, ArchitectureBuildPath::Full(reason)),
            None => assert_incremental(&attempt, &["src/main.rs"], &[]),
        }
    }
}

#[tokio::test]
async fn architecture_incremental_ordinary_stage_failures_retry_full() {
    for stage in ["prepare", "read_b", "finish"] {
        let (base, runtime, _, _) =
            super::architecture_http::fixture_exact_sources(NATIVE_SOURCES).await;
        let scheduler = runtime.browser_scheduler().unwrap();
        scheduler.set_architecture_fault_for_tests(
            stage,
            "store_unavailable: injected stage failure".into(),
        );
        let pin = index_edit(
            &base,
            &runtime,
            &[("src/main.rs", Some("fn main() { let _value = 2; }\n"))],
        )
        .await;
        let attempt = published_attempt(&scheduler, pin);
        assert_eq!(
            attempt.path,
            ArchitectureBuildPath::Full(FullReason::IncrementalFailure)
        );
        assert_eq!(attempt.full_retries, 1, "{stage}");
        assert_eq!(attempt.semantic_sources.len(), 5);
        assert_eq!(attempt.references.file_references.len(), 5);
        assert_eq!(
            core(&scheduler, pin).canonical().unwrap(),
            independent_full(&runtime, pin).await
        );
    }
}

#[tokio::test]
async fn architecture_incremental_stop_precedence() {
    for (stage, error) in [
        ("read_b", "cancelled: injected plain cancellation"),
        ("read_b", "storage_busy: injected contention"),
        ("oracle", "cancelled: injected oracle cancellation"),
        ("oracle", "storage_busy: injected oracle contention"),
        (
            "full_compute",
            "cancelled: injected oracle computation cancellation",
        ),
        (
            "full_compute",
            "storage_busy: injected oracle computation contention",
        ),
        ("admission", "corrupt admitted read"),
    ] {
        let (base, runtime, _, _) =
            super::architecture_http::fixture_exact_sources(NATIVE_SOURCES).await;
        let scheduler = runtime.browser_scheduler().unwrap();
        let original = current_pin(&scheduler);
        if stage == "admission" {
            scheduler.set_architecture_admission_fault_for_tests(error.into());
        } else {
            scheduler.set_architecture_fault_for_tests(stage, error.into());
        }
        let before_paths = scheduler.architecture_build_paths_for_tests().len();
        let before_verified = scheduler.architecture_verified_for_tests();
        let pin = index_edit(
            &base,
            &runtime,
            &[("src/main.rs", Some("fn main() { let _value = 2; }\n"))],
        )
        .await;
        let attempts: Vec<_> = scheduler
            .architecture_attempts_for_tests()
            .into_iter()
            .filter(|attempt| attempt.pin == pin)
            .collect();
        assert_eq!(attempts.len(), 2, "{stage}: {attempts:?}");
        let stopped = &attempts[0];
        assert!(!stopped.compared && !stopped.published, "{stopped:?}");
        assert_eq!(stopped.full_retries, 0);
        if matches!(stage, "read_b" | "admission") {
            assert!(stopped.measured_sources.is_empty(), "{stopped:?}");
        }
        assert_eq!(stopped.base, Some(original));
        assert_eq!(attempts[1].base, Some(original));
        assert_incremental(&published_attempt(&scheduler, pin), &["src/main.rs"], &[]);
        assert_eq!(
            scheduler.architecture_build_paths_for_tests().len(),
            before_paths + 1
        );
        assert_eq!(
            scheduler.architecture_verified_for_tests(),
            before_verified + 1
        );
    }
}

async fn give_way_at(point: &'static str) {
    let (base, runtime, _, _) =
        super::architecture_http::fixture_exact_sources(NATIVE_SOURCES).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let original = current_pin(&scheduler);
    let hold = Hold::at(&scheduler, point);
    let superseded = publish_edit(
        &base,
        &runtime,
        &[("src/main.rs", Some("fn main() { let _value = 2; }\n"))],
    )
    .await;
    hold.wait_reached();
    assert_eq!(scheduler.leases_for_tests(), 1);
    let release = hold.clone();
    runtime.set_publication_before_commit_hook_for_tests(move || release.release());
    let pin = index_edit(
        &base,
        &runtime,
        &[("src/main.rs", Some("fn main() { let _value = 3; }\n"))],
    )
    .await;
    let attempts = scheduler.architecture_attempts_for_tests();
    let yielded = attempts
        .iter()
        .find(|attempt| attempt.pin == superseded)
        .unwrap();
    assert!(!yielded.compared && !yielded.published);
    assert_eq!(yielded.full_retries, 0);
    assert_eq!(yielded.base, Some(original));
    assert!(scheduler.architecture_core_for_tests(superseded).is_none());
    let next = published_attempt(&scheduler, pin);
    assert_eq!(next.base, Some(original));
    assert_incremental(&next, &["src/main.rs"], &[]);
}

#[tokio::test]
async fn architecture_incremental_give_way_read_a() {
    give_way_at("reading").await;
}

#[tokio::test]
async fn architecture_incremental_give_way_read_b() {
    give_way_at("architecture_read_b_admitted").await;
}

#[tokio::test]
async fn architecture_incremental_mismatch_serves_full_once() {
    for pointer in [
        "/dependencies/documents/0/references/work",
        "/references/workUsed",
    ] {
        let (base, runtime, _, _) =
            super::architecture_http::fixture_exact_sources(NATIVE_SOURCES).await;
        let scheduler = runtime.browser_scheduler().unwrap();
        let before = core(&scheduler, current_pin(&scheduler));
        let paths = scheduler.architecture_build_paths_for_tests().len();
        let compared = scheduler.architecture_verified_for_tests();
        scheduler.set_architecture_mismatch_for_tests(pointer.into());
        let pin = index_edit(
            &base,
            &runtime,
            &[("src/main.rs", Some("fn main() { let _value = 2; }\n"))],
        )
        .await;
        let attempt = scheduler.architecture_attempts_for_tests().pop().unwrap();
        assert_incremental(&attempt, &["src/main.rs"], &[]);
        assert!(attempt.published && attempt.compared);
        assert_eq!(scheduler.architecture_differences_for_tests(), [pointer]);
        assert_eq!(
            scheduler.architecture_build_paths_for_tests().len(),
            paths + 1
        );
        assert_eq!(scheduler.architecture_verified_for_tests(), compared + 1);
        assert_eq!(
            core(&scheduler, pin).canonical().unwrap(),
            independent_full(&runtime, pin).await
        );
        assert!(
            core(&scheduler, pin)
                .sharing(&before)
                .values()
                .all(|sharing| !sharing.source && !sharing.metric && !sharing.dependency),
            "a mismatch must publish the independent full core, not the candidate"
        );
    }
}

#[tokio::test]
async fn architecture_incremental_external_provenance() {
    let (base, runtime, _, _) =
        super::architecture_http::fixture_exact_sources(PACKAGE_SOURCES).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let first = current_pin(&scheduler);
    published_attempt(&scheduler, first);
    let before = core(&scheduler, first);
    let frozen = before.canonical().unwrap();
    let old_answer = reference_answer(&runtime, first, &[], None).await;
    let pin = index_edit(
        &base,
        &runtime,
        &[("src/main.rs", Some("fn main() { let _value = 2; }\n"))],
    )
    .await;
    assert_incremental(&published_attempt(&scheduler, pin), &["src/main.rs"], &[]);
    let after = core(&scheduler, pin);
    let canonical = after.canonical().unwrap();
    assert_eq!(canonical, independent_full(&runtime, pin).await);
    assert_eq!(
        frozen["dependencies"]["manifests"],
        canonical["dependencies"]["manifests"]
    );
    for (key, sharing) in after.sharing(&before) {
        if key.path != "src/main.rs" {
            assert!(
                sharing.source && sharing.metric && sharing.plan,
                "{key:?}: {sharing:?}"
            );
        }
    }
    assert_eq!(before.canonical().unwrap(), frozen);
    assert_eq!(
        reference_answer(&runtime, first, &[], None).await,
        old_answer
    );
    let answer = reference_answer(&runtime, pin, &[], None).await;
    let admitted = {
        let (response, _) = runtime.evidence_response().unwrap();
        let manifests = response.architecture_manifests_for_tests(pin).unwrap();
        response.finish(manifests).unwrap()
    };
    let packages = answer["externalPackages"].as_array().unwrap();
    let mut manifests = 0;
    for package in packages {
        let evidence = package["evidence"].as_array().unwrap();
        let unique: std::collections::BTreeSet<_> = evidence.iter().map(Value::to_string).collect();
        assert_eq!(unique.len(), evidence.len(), "{package}");
        for proof in evidence {
            if proof["kind"] != "manifest" {
                continue;
            }
            manifests += 1;
            assert_eq!(proof.as_object().unwrap().len(), 10, "{proof}");
            let captured = admitted
                .iter()
                .find(|entry| {
                    proof["sourceSetId"] == entry.source_set_id && proof["path"] == entry.path
                })
                .unwrap();
            assert_eq!(proof["nativeRevisionId"], captured.native_revision_id);
            assert_eq!(proof["dependencyHash"], captured.dependency_hash);
            assert_eq!(
                proof["contentHash"],
                captured.content_hash.as_ref().unwrap().as_str()
            );
            assert!(
                proof["importRoot"].is_string()
                    && proof["key"].is_string()
                    && proof["section"].is_string()
                    && proof.get("versionConstraint").is_some()
            );
        }
    }
    assert!(manifests >= 8, "{answer}");
    let cargo = packages
        .iter()
        .find(|package| package["identity"]["package"] == "vendor")
        .unwrap();
    assert_eq!(cargo["ecosystem"], "cargo");
    assert_eq!(cargo["evidence"].as_array().unwrap().len(), 2);
    assert_ne!(old_answer["externalPackages"], answer["externalPackages"]);
}

#[tokio::test]
async fn architecture_incremental_external_source_selection() {
    let (base, runtime, _, _) =
        super::architecture_http::fixture_exact_sources(PACKAGE_SOURCES).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let pin = index_edit(
        &base,
        &runtime,
        &[("src/main.rs", Some("fn main() { let _value = 2; }\n"))],
    )
    .await;
    assert_incremental(&published_attempt(&scheduler, pin), &["src/main.rs"], &[]);
    let snapshot = {
        let (response, _) = runtime.evidence_response().unwrap();
        let snapshot = response.architecture_snapshot_for_tests(pin).unwrap();
        response.finish(snapshot).unwrap()
    };
    let full = trellis::references::resolve_all(&snapshot, &mut || Ok(())).unwrap();
    for (paths, prefix, manifest_paths, language) in [
        (vec!["root.js"], None, vec!["package.json"], "javascript"),
        (
            vec!["tools/nested.js"],
            None,
            vec!["tools/package.json"],
            "javascript",
        ),
        (
            vec!["root.js", "other.js", "tools/nested.js"],
            None,
            vec!["package.json", "tools/package.json"],
            "javascript",
        ),
        (vec!["typed.ts"], None, vec!["package.json"], "typescript"),
        (
            vec![],
            Some("tools"),
            vec!["tools/package.json"],
            "javascript",
        ),
    ] {
        let answer = reference_answer(&runtime, pin, &paths, prefix).await;
        let projected = full
            .select_under(
                &paths
                    .iter()
                    .map(|path| (*path).to_owned())
                    .collect::<Vec<_>>(),
                prefix,
                5,
                &mut || Ok(()),
            )
            .unwrap()
            .data;
        assert_eq!(answer["externalPackages"], projected["externalPackages"]);
        for field in ["production", "tests", "unresolved", "counts", "modules"] {
            assert_eq!(answer[field], projected[field], "{field}");
        }
        let package = answer["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|package| package["identity"]["package"] == "shared")
            .unwrap();
        assert_eq!(package["identity"]["language"], language);
        let actual: std::collections::BTreeSet<_> = package["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .map(|proof| proof["path"].as_str().unwrap())
            .collect();
        assert_eq!(actual, manifest_paths.into_iter().collect());
        assert_eq!(
            package["evidence"].as_array().unwrap().len(),
            actual.len() * 2
        );
    }
}

#[tokio::test]
async fn architecture_incremental_external_stdlib() {
    let (base, runtime, _, _) =
        super::architecture_http::fixture_exact_sources(PACKAGE_SOURCES).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let first = current_pin(&scheduler);
    let before = core(&scheduler, first);
    let previous = reference_answer(&runtime, first, &[], None).await;
    let pin = index_edit(
        &base,
        &runtime,
        &[("src/main.rs", Some("fn main() { let _value = 2; }\n"))],
    )
    .await;
    assert_incremental(&published_attempt(&scheduler, pin), &["src/main.rs"], &[]);
    let after = core(&scheduler, pin);
    assert_eq!(
        after.canonical().unwrap(),
        independent_full(&runtime, pin).await
    );
    assert!(
        after
            .sharing(&before)
            .iter()
            .filter(|(key, _)| key.path != "src/main.rs")
            .all(|(_, sharing)| sharing.source && sharing.metric)
    );
    let answer = reference_answer(&runtime, pin, &[], None).await;
    let stable = |answer: &Value| -> Vec<Value> {
        answer["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|package| package["identity"]["standardLibrary"] == true)
            .cloned()
            .collect()
    };
    assert_eq!(stable(&previous), stable(&answer));
    let catalogs: std::collections::BTreeSet<_> = stable(&answer)
        .into_iter()
        .flat_map(|package| {
            package["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .map(|proof| {
                    assert_eq!(proof["kind"], "standardLibraryCatalog");
                    proof["catalog"].as_str().unwrap().to_owned()
                })
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        catalogs,
        [
            "rust-standard-library:v1",
            "node-standard-library:v2",
            "java-standard-library:v2",
            "python-standard-library:v2"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
}

#[tokio::test]
async fn architecture_incremental_external_manifest_gate() {
    let (base, runtime, _, _) =
        super::architecture_http::fixture_exact_sources(PACKAGE_SOURCES).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    for path in ["Cargo.toml", "package.json", "tools/package.json"] {
        let original = std::fs::read_to_string(base.path().join("checkout").join(path)).unwrap();
        let changed = original
            .replace("version='0.1.0'", "version='0.2.0'")
            .replace("version='1'", "version='3'")
            .replace("\"shared\":\"1.0.0\"", "\"shared\":\"3.0.0\"")
            .replace("\"shared\":\"2.0.0\"", "\"shared\":\"4.0.0\"");
        assert_ne!(changed, original);
        let pin = index_edit(&base, &runtime, &[(path, Some(&changed))]).await;
        assert_eq!(
            published_attempt(&scheduler, pin).path,
            ArchitectureBuildPath::Full(FullReason::ManifestsChanged)
        );
        assert_eq!(
            core(&scheduler, pin).canonical().unwrap(),
            independent_full(&runtime, pin).await
        );
    }
}

fn source_work(canonical: &Value, path: &str) -> usize {
    let i = canonical["dependencies"]["order"]
        .as_array()
        .unwrap()
        .iter()
        .position(|key| key["path"] == path)
        .unwrap();
    canonical["dependencies"]["documents"][i]["references"]["work"]
        .as_u64()
        .unwrap() as usize
}

async fn work_boundaries(packages: bool) {
    let mut sources = if packages {
        PACKAGE_SOURCES
    } else {
        NATIVE_SOURCES
    }
    .to_vec();
    let main = format!("fn main() {{ {} }}\n", "std::mem::drop(1);".repeat(30));
    let initial_main = format!("{main}// z\n");
    sources
        .iter_mut()
        .find(|(path, _)| *path == "src/main.rs")
        .unwrap()
        .1 = &initial_main;
    let (base, runtime, _, _) = super::architecture_http::fixture_exact_sources(&sources).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let initial = core(&scheduler, current_pin(&scheduler))
        .canonical()
        .unwrap();
    let edited = format!("{main}// a\n");
    let pin = index_edit(&base, &runtime, &[("src/main.rs", Some(&edited))]).await;
    let attempt = published_attempt(&scheduler, pin);
    assert_incremental(&attempt, &["src/main.rs"], &[]);
    let canonical = core(&scheduler, pin).canonical().unwrap();
    let work = canonical["references"]["workUsed"].as_u64().unwrap() as usize;
    let margin = attempt.references.executed_work + source_work(&initial, "src/main.rs");
    assert!(margin > work, "margin={margin}, full={work}");
    let mut limit = trellis::references::set_reference_work_limit_for_tests(margin);
    scheduler.capture_architecture_work_limit_for_tests();
    let at = format!("{main}// b\n");
    let pin = index_edit(&base, &runtime, &[("src/main.rs", Some(&at))]).await;
    assert_incremental(&published_attempt(&scheduler, pin), &["src/main.rs"], &[]);
    limit.set(margin - 1);
    scheduler.capture_architecture_work_limit_for_tests();
    let below = format!("{main}// c\n");
    let pin = index_edit(&base, &runtime, &[("src/main.rs", Some(&below))]).await;
    let attempt = published_attempt(&scheduler, pin);
    assert_eq!(
        attempt.path,
        ArchitectureBuildPath::Full(FullReason::ReferenceWorkMargin)
    );
    assert_eq!(attempt.full_retries, 1);
    assert_eq!(
        core(&scheduler, pin).canonical().unwrap(),
        independent_full(&runtime, pin).await
    );
    let eligible = pin;
    limit.set(work - 1);
    scheduler.capture_architecture_work_limit_for_tests();
    let failed = format!("{main}// d\n");
    let pin = index_edit(&base, &runtime, &[("src/main.rs", Some(&failed))]).await;
    let attempt = published_attempt(&scheduler, pin);
    assert_eq!(
        attempt.path,
        ArchitectureBuildPath::Full(FullReason::IncrementalFailure)
    );
    assert_eq!(attempt.full_retries, 1);
    assert!(scheduler.architecture_core_for_tests(pin).is_none());
    let (response, _) = runtime.evidence_response().unwrap();
    let full_error = response.architecture_full_for_tests(pin).unwrap_err();
    let error = runtime
        .architecture_tool(
            &response,
            "references",
            &json!({"expectedBasis": pin}),
            &mut || Ok(()),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), full_error.to_string());
    assert!(error.to_string().starts_with("bounded_work:"), "{error:#}");
    drop(response);
    limit.set(usize::MAX);
    scheduler.capture_architecture_work_limit_for_tests();
    let recovered = format!("{main}// e\n");
    let pin = index_edit(&base, &runtime, &[("src/main.rs", Some(&recovered))]).await;
    let attempt = published_attempt(&scheduler, pin);
    assert_eq!(
        attempt.base,
        Some(eligible),
        "a kept failure cannot seed the next build"
    );
    assert_incremental(&attempt, &["src/main.rs"], &[]);
}

#[tokio::test]
async fn architecture_incremental_work_limits() {
    work_boundaries(false).await;
}

#[tokio::test]
async fn architecture_incremental_byte_scale_and_extraction() {
    let mut sources = NATIVE_SOURCES.to_vec();
    sources[0].1 = "fn main() { std::mem::drop(1); }\n";
    let (base, runtime, _, _) = super::architecture_http::fixture_exact_sources(&sources).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let literal = format!(
        "fn main() {{ std::mem::drop(\"{}\"); }}\n",
        "x".repeat(1100)
    );
    let pin = index_edit(&base, &runtime, &[("src/main.rs", Some(&literal))]).await;
    assert_incremental(&published_attempt(&scheduler, pin), &["src/main.rs"], &[]);
    let before = core(&scheduler, pin).canonical().unwrap();
    assert_eq!(before, independent_full(&runtime, pin).await);
    let extracted = literal.replace("); }", "); std::mem::drop(2); }");
    let pin = index_edit(&base, &runtime, &[("src/main.rs", Some(&extracted))]).await;
    assert_incremental(&published_attempt(&scheduler, pin), &["src/main.rs"], &[]);
    let after = core(&scheduler, pin).canonical().unwrap();
    assert_eq!(after, independent_full(&runtime, pin).await);
    assert!(
        after["references"]["workUsed"].as_u64().unwrap()
            > before["references"]["workUsed"].as_u64().unwrap()
    );
}

#[tokio::test]
async fn architecture_incremental_external_work_boundary() {
    work_boundaries(true).await;
}

#[tokio::test]
async fn architecture_incremental_site_limits() {
    let (base, runtime, _, _) =
        super::architecture_http::fixture_exact_sources(NATIVE_SOURCES).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let text = "fn main() { std::mem::drop(1); std::mem::drop(2); }\n";
    let pin = index_edit(&base, &runtime, &[("src/main.rs", Some(text))]).await;
    let attempt = published_attempt(&scheduler, pin);
    assert_incremental(&attempt, &["src/main.rs"], &[]);
    let value = core(&scheduler, pin).canonical().unwrap();
    let sites: usize = value["references"]["sourceFacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| source["sites"].as_u64().unwrap() as usize)
        .sum();
    assert!(sites >= 3);
    struct ResetSites;
    impl Drop for ResetSites {
        fn drop(&mut self) {
            trellis::references::set_site_limit_for_tests(250_000);
        }
    }
    let _reset = ResetSites;
    trellis::references::set_site_limit_for_tests(sites);
    let at = format!("{text}// at\n");
    let pin = index_edit(&base, &runtime, &[("src/main.rs", Some(&at))]).await;
    assert_incremental(&published_attempt(&scheduler, pin), &["src/main.rs"], &[]);
    trellis::references::set_site_limit_for_tests(sites - 1);
    let beyond = format!("{text}// beyond\n");
    let pin = index_edit(&base, &runtime, &[("src/main.rs", Some(&beyond))]).await;
    let attempt = published_attempt(&scheduler, pin);
    assert_eq!(
        attempt.path,
        ArchitectureBuildPath::Full(FullReason::IncrementalFailure)
    );
    let (response, _) = runtime.evidence_response().unwrap();
    let full = response.architecture_full_for_tests(pin).unwrap_err();
    let kept = runtime
        .architecture_tool(
            &response,
            "references",
            &json!({"expectedBasis": pin}),
            &mut || Ok(()),
        )
        .unwrap_err();
    assert_eq!(kept.to_string(), full.to_string());
    assert_eq!(kept.to_string(), "bounded_work: reference site count");
}

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

impl Drop for Hold {
    fn drop(&mut self) {
        self.release();
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
    let scheduler = runtime.browser_scheduler().unwrap();
    // Baseline and hold before the rules, so any build they make eligible is counted and held.
    let before = scheduler.architecture_builds_for_tests();
    let hold = Hold::at(&scheduler, "architecture_build_finished");
    write_rules(&base, RULES);
    let app = browser.clone().router();
    // The owner starts on the publication hint alone: no read asked.
    scheduler.architecture_hint();
    hold.wait_reached();
    hold.release();
    assert_eq!(scheduler.architecture_builds_for_tests() - before, 1);
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
    assert_eq!(scheduler.architecture_builds_for_tests() - before, 1);
}

#[tokio::test]
async fn a_read_during_the_build_answers_building_with_progress_then_the_result() {
    let (base, runtime, browser, key) = fixture().await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let before = scheduler.architecture_builds_for_tests();
    let hold = Hold::at(&scheduler, "evaluating");
    write_rules(&base, RULES);
    let app = browser.clone().router();
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
    assert_eq!(scheduler.architecture_builds_for_tests() - before, 1);
}

/// #706: a page's summary read samples the head, then waits on the owner. A revision
/// published during that wait ends it early with the typed `202 building` for the new head,
/// and the page asks again (docs/architecture-view.md). Both builds are held at a hook, so the
/// head moves at exactly that point.
#[tokio::test]
async fn a_summary_read_whose_head_moves_while_it_waits_answers_building_and_is_asked_again() {
    let (base, runtime, browser, key) = fixture().await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let (held_tx, held_rx) = mpsc::channel();
    let (waiting_tx, waiting_rx) = mpsc::channel();
    let (release_first, first_gate) = mpsc::channel::<()>();
    let (release_second, second_gate) = mpsc::channel::<()>();
    let gates = Mutex::new((first_gate, second_gate));
    let release_second = Mutex::new(Some(release_second));
    let builds = std::sync::atomic::AtomicUsize::new(0);
    let waits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = waits.clone();
    scheduler.set_architecture_hook_for_tests(Arc::new(move |point| match point {
        "architecture_build_started" => {
            builds.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        // After the build's read: it holds no lease, so a publication goes ahead.
        "resolving" => {
            let gates = gates.lock().unwrap();
            let gate = match builds.load(std::sync::atomic::Ordering::Acquire) {
                1 => &gates.0,
                2 => &gates.1,
                _ => return,
            };
            let _ = held_tx.send(());
            let _ = gate.recv_timeout(trellis::testing::HANG_BACKSTOP);
        }
        "architecture_read_waiting" => {
            // The page asked again, for the new head: let its build finish.
            if counted.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 1 {
                release_second.lock().unwrap().take();
            }
            let _ = waiting_tx.send(());
        }
        _ => {}
    }));
    // Rules after the hook, so the first ruled build is counted and held.
    write_rules(&base, RULES);
    let signalled = |rx: mpsc::Receiver<()>| {
        tokio::task::spawn_blocking(move || {
            rx.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap();
            rx
        })
    };
    let (app, page) = (browser.clone().router(), key.clone());
    let read = tokio::spawn(async move { summary(app, &page, "").await });
    // The first build is held after its read, and the page's read waits on it.
    let held_rx = signalled(held_rx).await.unwrap();
    let _waiting_rx = signalled(waiting_rx).await.unwrap();
    let root = base.path().join("checkout");
    std::fs::write(root.join("added.js"), "function added() {}\n").unwrap();
    let indexing = runtime.clone();
    tokio::task::spawn_blocking(move || indexing.cli_index(&IndexOptions::new(root)))
        .await
        .unwrap()
        .unwrap();
    scheduler.architecture_hint();
    drop(release_first);
    let (status, body) = read.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["payload"]["state"], "ready");
    assert_eq!(body["payload"]["modules"].as_array().unwrap().len(), 5);
    // The first read answered `building` for the new head; the page's second read waited.
    assert!(waits.load(std::sync::atomic::Ordering::Acquire) >= 2);
    drop(held_rx);
}

#[tokio::test]
async fn concurrent_readers_wait_for_one_build() {
    let (base, runtime, browser, key) = fixture().await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let before = scheduler.architecture_builds_for_tests();
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
    assert_eq!(scheduler.architecture_builds_for_tests() - before, 1);
    let pin = serde_json::from_value(json!({
        "indexGeneration": a.1["payload"]["indexGeneration"],
        "indexRevision": a.1["payload"]["indexRevision"]
    }))
    .unwrap();
    let (fresh, _) = runtime.evidence_response().unwrap();
    let computed = fresh
        .architecture_view(
            &MetricsOptions {
                pin: Some(pin),
                aggregate: "max".into(),
                ..MetricsOptions::default()
            },
            &mut || Ok(()),
        )
        .unwrap();
    fresh.finish(()).unwrap();
    assert_eq!(
        a.1["payload"], computed,
        "coalesced result matches fresh computation"
    );
}

#[tokio::test]
async fn a_rules_change_is_never_served_from_the_older_core() {
    let (base, runtime, browser, key) = fixture().await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let before = scheduler.architecture_builds_for_tests();
    write_rules(&base, RULES);
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
    assert_eq!(scheduler.architecture_builds_for_tests() - before, 2);
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
    runtime.hold_worker_step_for_tests(|| {
        std::fs::write(root.join("added.js"), "function added() {}\n").unwrap();
        // Capture the root directory change before publishing; rules stay absent.
        std::fs::create_dir(root.join(".trellis")).unwrap();
    });
    let indexing = runtime.clone();
    let indexed_root = root.clone();
    let publication =
        tokio::task::spawn_blocking(move || indexing.cli_index(&IndexOptions::new(indexed_root)))
            .await
            .unwrap()
            .unwrap();
    // #870: the owner builds the new head without rules on the publication's hint, and that
    // build's next turn sees rules written after it. Wait for that build to be published, then
    // take the baseline before the rules exist: the head's ruled build and the older one are
    // both counted, however the owner orders its turns.
    let published = serde_json::from_value(json!({
        "indexGeneration": publication["publishedRevision"]["indexGeneration"],
        "indexRevision": publication["publishedRevision"]["indexRevision"]
    }))
    .unwrap();
    runtime
        .settled_for_tests(Some(published))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let scheduler = runtime.browser_scheduler().unwrap();
    let before = scheduler.architecture_builds_for_tests();
    write_rules(&base, RULES);
    let (status, read) = request(app.clone(), &key, &older).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["payload"]["modules"].as_array().unwrap().len(), 4);
    assert_eq!(
        read["payload"]["indexRevision"],
        missing["payload"]["indexRevision"]
    );
    assert_eq!(scheduler.architecture_builds_for_tests() - before, 2);
    // An unchanged inventory must retain the head that the owner already built.
    let indexing = runtime.clone();
    let unchanged =
        tokio::task::spawn_blocking(move || indexing.cli_index(&IndexOptions::new(root)))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        unchanged["publishedRevision"],
        publication["publishedRevision"]
    );
    // Both stay served, without another build.
    let (status, current) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{current}");
    assert_eq!(current["payload"]["modules"].as_array().unwrap().len(), 5);
    assert_eq!(
        head(&current["payload"]),
        head(&publication["publishedRevision"])
    );
    let (status, again) = request(app, &key, &older).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(scheduler.architecture_builds_for_tests() - before, 2);
}

/// A rules read with no rules file fails at once: it does not ask for or wait on a build.
#[tokio::test]
async fn a_rules_read_without_rules_does_not_wait_for_a_build() {
    let (_base, runtime, _browser, _key) = fixture().await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let before = scheduler.architecture_builds_for_tests();
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
    assert_eq!(scheduler.architecture_builds_for_tests() - before, 0);
}

/// #1301: verify mode is on in this suite. Each owner build is compared with a full build of
/// the same revision, and reports the path it took; off, a build is not compared.
#[tokio::test]
async fn verify_mode_compares_each_build_with_a_full_build_and_reports_its_path() {
    use trellis::http::{ArchitectureBuildPath, FullReason};
    let (base, runtime, browser, key) = fixture().await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let builds = scheduler.architecture_builds_for_tests();
    let verified = scheduler.architecture_verified_for_tests();
    write_rules(&base, RULES);
    let app = browser.clone().router();
    let (status, first) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["payload"]["state"], "ready");
    assert_eq!(scheduler.architecture_builds_for_tests() - builds, 1);
    assert_eq!(scheduler.architecture_verified_for_tests() - verified, 1);
    let pin: trellis::model::IndexPin = serde_json::from_value(json!({
        "indexGeneration": first["payload"]["indexGeneration"],
        "indexRevision": first["payload"]["indexRevision"]
    }))
    .unwrap();
    assert_eq!(
        scheduler.architecture_build_paths_for_tests().last(),
        Some(&(pin, ArchitectureBuildPath::Full(FullReason::NoBase)))
    );

    scheduler.set_architecture_verify_incremental_for_tests(false);
    write_rules(&base, OTHER_RULES);
    let (status, second) = request(app, &key, "").await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(scheduler.architecture_builds_for_tests() - builds, 2);
    assert_eq!(scheduler.architecture_verified_for_tests() - verified, 1);
}

/// A running build holds no read: the head can move, and the build gives way to it.
#[tokio::test]
async fn a_new_revision_supersedes_a_build_that_is_computing() {
    let (base, runtime, browser, key) = fixture().await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let before = scheduler.architecture_builds_for_tests();
    let superseded = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hold = Hold::at_with(
        &scheduler,
        "evaluating",
        superseded.clone(),
        "architecture_build_superseded",
    );
    write_rules(&base, RULES);
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
    assert!(scheduler.architecture_builds_for_tests() - before >= 2);
}

/// #1245 (owner ruling 2026-11-20): publications never wait on the Architecture read. The
/// build is held inside its read, after its first document, and would hold again at its next
/// one. Inside the publication's transaction the hold is released: the read gives its snapshot
/// up at its next check, without reading on, and the publication commits. A status read during
/// the publication gets no `storage_busy` from the Architecture read. The yielded read
/// published nothing for the old head, and the owner, reading again once the publication
/// ended, builds the new head with the answer a fresh computation gives.
#[tokio::test]
async fn a_publication_does_not_wait_for_the_build_read_which_yields_and_reads_the_new_head() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let (base, runtime, browser, key) = fixture().await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let app = browser.clone().router();
    let (status, missing) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{missing}");
    let older: trellis::model::IndexPin = serde_json::from_value(json!({
        "indexGeneration": missing["payload"]["indexGeneration"],
        "indexRevision": missing["payload"]["indexRevision"]
    }))
    .unwrap();
    let before = scheduler.architecture_builds_for_tests();

    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_first, first_gate) = mpsc::channel::<()>();
    let (release_next, next_gate) = mpsc::channel::<()>();
    let gates = Mutex::new((first_gate, next_gate));
    let builds = Arc::new(AtomicUsize::new(0));
    let documents = AtomicUsize::new(0);
    let read_on = Arc::new(AtomicBool::new(false));
    let yielded_in = Arc::new(AtomicUsize::new(0));
    let (counted, went_on, yielded) = (builds.clone(), read_on.clone(), yielded_in.clone());
    scheduler.set_architecture_hook_for_tests(Arc::new(move |point| match point {
        "architecture_build_started" => {
            counted.fetch_add(1, Ordering::AcqRel);
        }
        "reading" if counted.load(Ordering::Acquire) == 1 => {
            let gates = gates.lock().unwrap();
            if documents.fetch_add(1, Ordering::AcqRel) == 0 {
                let _ = reached_tx.send(());
                let _ = gates.0.recv_timeout(trellis::testing::HANG_BACKSTOP);
            } else {
                went_on.store(true, Ordering::Release);
                let _ = gates.1.recv_timeout(trellis::testing::HANG_BACKSTOP);
            }
        }
        "architecture_read_yielded" => {
            yielded.store(counted.load(Ordering::Acquire), Ordering::Release);
        }
        _ => {}
    }));
    // Rules make this head's build eligible; it is held inside its read.
    write_rules(&base, RULES);
    scheduler.architecture_hint();
    reached_rx
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("the build is inside its read");
    assert_eq!(scheduler.leases_for_tests(), 1);

    // Inside the publication's transaction, its intent raised: the held read may go on, and a
    // status read starts while the publication is in flight.
    let (status_tx, status_rx) = mpsc::channel();
    let reader = runtime.clone();
    runtime.set_publication_before_commit_hook_for_tests(move || {
        drop(release_first);
        std::thread::spawn(move || {
            let _ = status_tx.send(reader.cli_status());
        });
    });
    let root = base.path().join("checkout");
    std::fs::write(root.join("added.js"), "function added() {}\n").unwrap();
    let indexing = runtime.clone();
    let published =
        tokio::task::spawn_blocking(move || indexing.cli_index(&IndexOptions::new(root)))
            .await
            .unwrap()
            .unwrap();
    let during = tokio::task::spawn_blocking(move || {
        status_rx
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .expect("the status read ran")
    })
    .await
    .unwrap();
    if let Err(error) = during {
        panic!("a status read during the publication failed: {error:#}");
    }
    assert!(
        !read_on.load(Ordering::Acquire),
        "the read gave way without reading on"
    );
    assert_eq!(
        yielded_in.load(Ordering::Acquire),
        1,
        "the first build yielded"
    );

    // Nothing of the yielded read is published: the old head has no ruled core. (A read of
    // that pin would ask the owner to build it, so this looks at the owner's published cores.)
    assert!(!scheduler.architecture_ruled_core_published_for_tests(older));

    // The owner read again after the publication ended, and built its head.
    let (status, next) = summary(app, &key, "").await;
    assert_eq!(status, StatusCode::OK, "{next}");
    assert_eq!(
        head(&next["payload"]),
        head(&published["publishedRevision"])
    );
    assert_eq!(next["payload"]["modules"].as_array().unwrap().len(), 5);
    assert!(scheduler.architecture_builds_for_tests() - before >= 2);
    let pin = serde_json::from_value(json!({
        "indexGeneration": next["payload"]["indexGeneration"],
        "indexRevision": next["payload"]["indexRevision"]
    }))
    .unwrap();
    let (fresh, _) = runtime.evidence_response().unwrap();
    let computed = fresh
        .architecture_view(
            &MetricsOptions {
                pin: Some(pin),
                aggregate: "max".into(),
                ..MetricsOptions::default()
            },
            &mut || Ok(()),
        )
        .unwrap();
    fresh.finish(()).unwrap();
    assert_eq!(
        next["payload"], computed,
        "the new head's answer is unchanged"
    );
    drop(release_next);
}

/// A read for a rules file newer than the running build waits for that file's build.
#[tokio::test]
async fn a_rules_edit_during_a_build_is_built_next() {
    let (base, runtime, browser, key) =
        fixture_with_sources(false, &[(".trellis/fixture", "rules build fixture\n")]).await;
    let (initial, _) = runtime.evidence_response().unwrap();
    let initial_pin = initial.status().unwrap().revision;
    initial.finish(()).unwrap();
    let scheduler = runtime.browser_scheduler().unwrap();
    let before = scheduler.architecture_builds_for_tests();
    let hold = Hold::at(&scheduler, "evaluating");
    write_rules(&base, RULES);
    scheduler.architecture_hint();
    hold.wait_reached();
    write_rules(&base, OTHER_RULES);
    let app = browser.clone().router();
    let reading = tokio::spawn(async move { request(app, &key, "").await });
    hold.release();
    let (status, read) = reading.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["payload"]["layers"][0]["name"], "other");
    assert_eq!(scheduler.architecture_builds_for_tests() - before, 2);
    let pin = serde_json::from_value(json!({
        "indexGeneration": read["payload"]["indexGeneration"],
        "indexRevision": read["payload"]["indexRevision"]
    }))
    .unwrap();
    assert_eq!(
        pin, initial_pin,
        "rules edits do not publish a native revision"
    );
    assert_eq!(read["payload"]["modules"].as_array().unwrap().len(), 4);
    let (fresh, _) = runtime.evidence_response().unwrap();
    let computed = fresh
        .architecture_view(
            &MetricsOptions {
                pin: Some(pin),
                aggregate: "max".into(),
                ..MetricsOptions::default()
            },
            &mut || Ok(()),
        )
        .unwrap();
    fresh.finish(()).unwrap();
    assert_eq!(
        read["payload"], computed,
        "edited rules match fresh computation"
    );
}

#[tokio::test]
async fn a_panicked_build_is_reported_once_and_the_next_read_builds_again() {
    let (base, runtime, browser, key) = fixture().await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let before = scheduler.architecture_builds_for_tests();
    let panics = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let injected = panics.clone();
    scheduler.set_architecture_hook_for_tests(Arc::new(move |point| {
        if point == "resolving" && injected.swap(false, std::sync::atomic::Ordering::AcqRel) {
            panic!("injected build panic");
        }
    }));
    // Rules make this build eligible; install the panic hook before publishing them.
    write_rules(&base, RULES);
    let app = browser.clone().router();
    let (status, failed) = request(app.clone(), &key, "").await;
    assert!(!panics.load(std::sync::atomic::Ordering::Acquire));
    assert!(status.is_server_error(), "{status} {failed}");
    let (status, ready) = request(app, &key, "").await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    assert_eq!(scheduler.architecture_builds_for_tests() - before, 2);
}

#[tokio::test]
async fn a_failure_of_the_revision_is_kept_and_answered_typed() {
    let (base, runtime, browser, key) = fixture().await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let before = scheduler.architecture_builds_for_tests();
    // Valid rules that only evaluation finds ambiguous.
    write_rules(
        &base,
        "version=1\n[layers.a]\norder=1\npaths=['**/*.js']\n[layers.b]\norder=2\npaths=['**/*.js']",
    );
    let app = browser.clone().router();
    for _ in 0..2 {
        let (status, failed) = request(app.clone(), &key, "").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{failed}");
        assert_eq!(failed["error"]["code"], "invalid_rules");
    }
    assert_eq!(scheduler.architecture_builds_for_tests() - before, 1);
}

/// #798: while the owner builds the head, a tool read with or without `paths` answers the
/// typed retryable `building` with the build's progress. A scoped read does not compute its own
/// answer beside the build.
#[tokio::test]
async fn a_tool_read_during_the_build_answers_building_without_computing() {
    let (base, runtime, _browser, _key) = fixture().await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let before = scheduler.architecture_builds_for_tests();
    let hold = Hold::at(&scheduler, "evaluating");
    write_rules(&base, RULES);
    scheduler.architecture_hint();
    hold.wait_reached();
    let tool = runtime.clone();
    let (whole, scoped) = tokio::task::spawn_blocking(move || {
        let (response, _) = tool.evidence_response().unwrap();
        let read =
            |arguments| tool.architecture_tool(&response, "check", &arguments, &mut || Ok(()));
        (
            read(json!({})).unwrap_err(),
            read(json!({"paths":["sample.js"]})).unwrap_err(),
        )
    })
    .await
    .unwrap();
    for building in [&whole, &scoped] {
        assert!(
            building
                .downcast_ref::<trellis::http::ArchitectureBuilding>()
                .is_some(),
            "{building}"
        );
        let envelope = trellis::mcp::architecture::failure(&json!(1), building);
        let error = &envelope["error"];
        assert_eq!(error["code"], "building", "{envelope}");
        assert_eq!(error["retryable"], true);
        assert_eq!(error["retryAfterMs"], 250);
        assert_eq!(
            error["progress"],
            json!({"phase":"evaluating","completed":0,"total":4})
        );
        assert_eq!(
            error["message"],
            "Architecture is being computed for this revision (evaluating 0 of 4); retry shortly"
        );
    }

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
    assert_eq!(scheduler.architecture_builds_for_tests() - before, 1);
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
    let scheduler = runtime.browser_scheduler().unwrap();
    let hold = Hold::at(&scheduler, "evaluating");
    write_rules(&base, RULES);
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
    let scheduler = runtime.browser_scheduler().unwrap();
    let before = scheduler.architecture_builds_for_tests();
    write_rules(&base, RULES);
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
    assert_eq!(scheduler.architecture_builds_for_tests() - before, 1);
}
