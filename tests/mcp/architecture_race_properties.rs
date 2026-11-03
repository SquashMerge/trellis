//! MCP/owner differential: fresh computation shares architecture algorithms, not an
//! independent semantic oracle. Only the caching/admission/transport path differs.
use super::*;
use proptest::prelude::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use trellis::daemon::registry::{CheckoutRegistry, CheckoutRuntime};

const INITIAL: &str = "version=1\n[layers.all]\norder=1\npaths=['*.js']\n";
const CURRENT: &str = "version=1\n[layers.high]\norder=1\npaths=['a.js']\n[layers.low]\norder=2\npaths=['b.js','c.js']\n";

// Dropping this on any unwind also unblocks the owner.
struct Release(Option<mpsc::Sender<()>>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.take();
    }
}

fn fresh(runtime: &CheckoutRuntime, args: Value) -> Value {
    let (read, _) = runtime.evidence_response().unwrap();
    let result = read
        .architecture_response("check", &args, &mut || Ok(()))
        .unwrap();
    read.finish(result).unwrap()
}

// Our small fixtures fit in one page. Reproduce only the documented per-instance
// evidence sampling, then remove the transport's page descriptor.
fn compare(envelope: &Value, mut expected: Value, trace: &str) {
    let basis = json!({"indexGeneration":expected["indexGeneration"],"indexRevision":expected["indexRevision"]});
    assert_eq!(envelope["evidenceBasis"], basis, "basis: {trace}");
    let mut actual = envelope["data"].clone();
    assert!(
        actual["page"]["nextOffset"].is_null(),
        "unexpected pagination: {trace}: {actual}"
    );
    assert_eq!(actual["page"]["offset"], 0, "{trace}");
    let total: usize = ["instances", "testOnly", "unassigned", "externalPackages"]
        .iter()
        .map(|field| actual[*field].as_array().unwrap().len())
        .sum();
    assert_eq!(actual["page"]["total"], total, "{trace}");
    actual.as_object_mut().unwrap().remove("page");
    for field in ["instances", "testOnly"] {
        for row in expected[field].as_array_mut().unwrap() {
            let evidence = row["evidence"].as_array_mut().unwrap();
            let omitted = evidence.len().saturating_sub(50);
            evidence.truncate(50);
            row["omittedEvidence"] = json!(omitted);
        }
    }
    assert_eq!(actual, expected, "MCP vs fresh: {trace}");
}

fn send(client: &mut std::os::unix::net::UnixStream, id: i64, args: Value) {
    writeln!(
        client,
        "{}",
        modern(
            json!(id),
            "tools/call",
            json!({"name":"trellis_rules_check","arguments":args})
        )
    )
    .unwrap();
}

async fn scenario(extra: Vec<bool>) {
    let trace = format!("extra imports (false=b,true=c)={extra:?}");
    eprintln!("architecture race: {trace}");
    let temp = tempfile::tempdir().unwrap();
    let (identity, topology) = published_inactive_checkout(
        temp.path(),
        &[
            ("a.js", "import './b.js';\n"),
            ("b.js", "export const b = 1;\n"),
            ("c.js", "export const c = 1;\n"),
        ],
    );
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    let rules = identity.root.join(".trellis/rules.toml");
    fs::write(&rules, INITIAL).unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(5567, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    let scheduler = runtime.browser_scheduler().unwrap();
    runtime.await_architecture_for_tests();
    let (entered, reached) = mpsc::channel();
    let (waiting, reader_waits) = mpsc::channel();
    let (release, held) = mpsc::channel::<()>();
    let guard = Release(Some(release));
    let held = Mutex::new(held);
    let first = AtomicBool::new(true);
    scheduler.set_architecture_hook_for_tests(Arc::new(move |point| match point {
        "architecture_read_waiting" => {
            let _ = waiting.send(());
        } // never block with owner lock held
        "resolving" if first.swap(false, Ordering::SeqCst) => {
            let _ = entered.send(());
            let _ = held
                .lock()
                .unwrap()
                .recv_timeout(trellis::testing::HANG_BACKSTOP);
        }
        _ => {}
    }));
    fs::write(&rules, CURRENT).unwrap();
    scheduler.architecture_hint();
    reached
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("owner resolving");
    let old = fresh(&runtime, json!({}));
    scheduler.set_architecture_browser_wait_for_tests(Duration::ZERO);
    let registry = Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, session) =
        socket_session(&identity, &registry, 5567, Some(Duration::from_secs(30)));
    send(
        &mut client,
        1,
        json!({"schemaVersion":1,"paths":["a.js"],"limit":50}),
    );
    let scoped = next_reply(&mut reader);
    tool(&scoped, None, true);
    compare(
        &scoped["result"]["structuredContent"],
        fresh(&runtime, json!({"paths":["a.js"]})),
        &trace,
    );
    // The scoped read may itself have signaled before falling back. Only a signal
    // after this point establishes that the unscoped MCP call reached the wait.
    while reader_waits.try_recv().is_ok() {}
    send(&mut client, 2, json!({"schemaVersion":1,"limit":50}));
    reader_waits
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("MCP reader waiting");
    eprintln!("{trace}: resolving -> scoped fallback -> unscoped waiting");
    let mut source = String::from("import './b.js';\nimport './c.js';\n");
    for c in &extra {
        source.push_str(if *c {
            "import './c.js';\n"
        } else {
            "import './b.js';\n"
        });
    }
    fs::write(identity.root.join("a.js"), source).unwrap();
    let indexing = runtime.clone();
    let root = identity.root.clone();
    tokio::task::spawn_blocking(move || {
        indexing.cli_index(&trellis::indexer::IndexOptions::new(root))
    })
    .await
    .unwrap()
    .unwrap();
    let (read, _) = runtime.evidence_response().unwrap();
    let pin = read.status().unwrap().revision;
    read.finish(()).unwrap();
    drop(read);
    let new = fresh(&runtime, json!({"expectedBasis":pin}));
    assert_ne!(old["indexRevision"], new["indexRevision"], "{trace}");
    assert_ne!(
        old["instances"], new["instances"],
        "source edit must change substantive data: {trace}"
    );
    let has_c = |data: &Value| {
        data["instances"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["rule"] == "$layers" && row["destination"]["path"] == "c.js")
    };
    assert!(!has_c(&old) && has_c(&new), "new layer edge: {trace}");
    eprintln!(
        "{trace}: published {} -> {}",
        old["indexRevision"], new["indexRevision"]
    );
    scheduler.architecture_hint();
    drop(guard);
    let reply = next_reply(&mut reader);
    tool(&reply, None, true);
    compare(&reply["result"]["structuredContent"], new, &trace);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    registry.lock().await.disconnect(5567);
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if registry.lock().await.release(&identity.root_key).unwrap() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("checkout retirement");
}

#[test]
fn deterministic_head_race_witness() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(scenario(vec![]));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_PROPTEST_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(4), ..ProptestConfig::default() })]
    #[test]
    fn generated_mcp_architecture_head_race(extra in prop::collection::vec(any::<bool>(), 0..5)) {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(scenario(extra));
    }
}
