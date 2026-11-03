//! Model successive published heads over one initialized, private MCP connection.
use super::*;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug)]
enum Form {
    Inherent,
    Generic,
    Trait,
}

#[derive(Clone, Debug)]
struct State {
    forms: Vec<Form>,
    noise: bool,
}

fn render(state: &State) -> (String, BTreeSet<(u64, u64)>) {
    let mut text = String::from("struct Foo<T>(T);\ntrait Action { fn run(); }\n");
    if state.noise {
        text.push_str("struct Other;\nimpl Other { fn extra() {} }\n");
    }
    let mut ranges = BTreeSet::new();
    for form in &state.forms {
        text.push_str(match form {
            Form::Inherent => "impl Foo<u8> { ",
            Form::Generic => "impl<T> Foo<T> { ",
            Form::Trait => "impl Action for Foo<u8> { ",
        });
        let start = text.len() as u64;
        text.push_str("fn run() { helper(); }");
        ranges.insert((start, text.len() as u64));
        text.push_str(" }\n");
    }
    text.push_str("fn helper() {}\n");
    (text, ranges)
}

struct Session {
    client: std::os::unix::net::UnixStream,
    reader: BufReader<std::os::unix::net::UnixStream>,
    thread: std::thread::JoinHandle<std::io::Result<()>>,
    release: mpsc::SyncSender<()>,
    id: i64,
}

impl Session {
    fn call(&mut self, name: &str, args: Value, error: Option<&str>) -> Value {
        self.id += 1;
        writeln!(
            self.client,
            "{}",
            request(
                json!(self.id),
                "tools/call",
                json!({"name":name,"arguments":args})
            )
        )
        .unwrap();
        let reply = next_reply(&mut self.reader);
        assert_eq!(reply["id"], self.id);
        tool(&reply, error, false);
        reply["result"]["structuredContent"].clone()
    }

    fn close(self) {
        self.release.send(()).unwrap();
        drop(self.reader);
        drop(self.client);
        self.thread.join().unwrap().unwrap();
    }
}

async fn run_case(case: usize, path: &str, history: &[State]) -> (tempfile::TempDir, i64, usize) {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let mut publisher = graph_fixtures::Publisher::open(temp.path(), &root);
    publisher
        .store
        .mcp_prepare_virgin_queue_for_tests()
        .unwrap();
    let (first, _) = render(&history[0]);
    let mut pin = publisher.publish(&graph_fixtures::Spec::syntax_only(&[(path, &first)]));
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry
        .attach_launch(80000 + case as u64, &identity)
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release, release_rx) = mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
    }));
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, thread) =
        socket_session(&identity, &registry, 80000 + case as u64, None);
    writeln!(
        client,
        "{}",
        request(
            json!(0),
            "initialize",
            fixture()["legacyInitialize"].clone()
        )
    )
    .unwrap();
    assert_eq!(
        next_reply(&mut reader)["result"]["protocolVersion"],
        "2025-11-25"
    );
    writeln!(
        client,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    let mut session = Session {
        client,
        reader,
        thread,
        release,
        id: 0,
    };
    // This call starts reconciliation; the hook proves it cannot overwrite our heads.
    session.id += 1;
    writeln!(
        session.client,
        "{}",
        request(
            json!(session.id),
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}})
        )
    )
    .unwrap();
    entered_rx
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    let reply = next_reply(&mut session.reader);
    tool(&reply, None, false);
    assert_eq!(
        reply["result"]["structuredContent"]["evidenceBasis"],
        lookup_basis(pin)
    );
    let checkout_runtime = registry.lock().await.runtime(&identity.root_key).unwrap();

    let key = format!("{path}::Foo::run");
    let mut prior_basis = None;
    let mut old_sid: Option<String> = None;
    let mut initial_sid: Option<String> = None;
    let mut stale_sid_heads = 0;
    for (step, state) in history.iter().enumerate() {
        let (text, expected) = render(state);
        if step > 0 {
            let old_pin = pin;
            pin = publisher.publish(&graph_fixtures::Spec::syntax_only(&[(path, &text)]));
            assert_eq!(pin.index_generation, old_pin.index_generation);
            assert!(pin.index_revision > old_pin.index_revision);
        }
        let basis = lookup_basis(pin);
        if let Some(old) = prior_basis.take() {
            let conflict = session.call(
                "trellis_find_symbols",
                json!({"schemaVersion":1,"lookupKey":key,"expectedBasis":old}),
                Some("revision_conflict"),
            );
            assert_eq!(
                conflict["error"]["currentBasis"], basis,
                "case={case} step={step}"
            );
        }
        let found = session.call(
            "trellis_find_symbols",
            json!({"schemaVersion":1,"lookupKey":key,"limit":1}),
            None,
        );
        assert_eq!(
            found["evidenceBasis"], basis,
            "case={case} step={step} history={history:?}"
        );
        assert_eq!(found["truncated"], false);
        let symbols = found["data"]["symbols"].as_array().unwrap();
        let mut ranges = BTreeSet::new();
        let mut sids = BTreeSet::new();
        for symbol in symbols {
            assert_eq!(symbol["stableLookupKey"], key);
            assert_eq!(symbol["name"], "run");
            assert_eq!(
                emitted_document(&found["data"], symbol)["document"]["path"],
                path
            );
            assert!(ranges.insert((
                symbol["range"]["start"].as_u64().unwrap(),
                symbol["range"]["end"].as_u64().unwrap()
            )));
            assert!(sids.insert(symbol["symbolId"].as_str().unwrap().to_owned()));
        }
        assert_eq!(
            ranges, expected,
            "case={case} step={step} history={history:?}"
        );
        if step == 0 {
            initial_sid = sids.iter().next().cloned();
        }
        if step == history.len() - 1 {
            assert_eq!(
                sids.iter().next(),
                initial_sid.as_ref(),
                "restored exact source must restore SID"
            );
        }
        if old_sid.as_ref().is_some_and(|id| !sids.contains(id)) {
            stale_sid_heads += 1;
        }
        for name in LOOKUP_ROOT_TOOLS {
            let args = json!({"schemaVersion":1,"lookupKey":key,"expectedBasis":basis});
            let result = session.call(
                name,
                args,
                if expected.is_empty() {
                    Some("not_found")
                } else {
                    None
                },
            );
            if expected.is_empty() {
                assert_eq!(result["error"]["currentBasis"], basis);
            } else {
                assert_eq!(result["evidenceBasis"], basis);
                if expected.len() == 1 {
                    let sid = sids.iter().next().unwrap();
                    let by_sid = session.call(
                        name,
                        json!({"schemaVersion":1,"symbolId":sid,"expectedBasis":basis}),
                        None,
                    );
                    assert_eq!(by_sid["evidenceBasis"], basis);
                    assert_eq!(
                        result["data"], by_sid["data"],
                        "case={case} step={step} {name}"
                    );
                    assert_eq!(result["data"]["stableLookupKey"], key);
                } else {
                    let data = &result["data"];
                    let fields: BTreeSet<_> = data
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(String::as_str)
                        .collect();
                    assert_eq!(
                        fields,
                        [
                            "lookupKey",
                            "candidates",
                            "documents",
                            "coverage",
                            "scopeCoverage",
                            "provenance"
                        ]
                        .into_iter()
                        .collect()
                    );
                    assert_eq!(data["lookupKey"], key);
                    assert_eq!(data["candidates"], found["data"]["symbols"]);
                    assert_eq!(data["documents"], found["data"]["documents"]);
                    assert_eq!(data["coverage"], found["data"]["coverage"]);
                    assert_eq!(data["scopeCoverage"], found["data"]["scopeCoverage"]);
                    assert_eq!(data["provenance"], found["data"]["provenance"]);
                    assert_eq!(result["warnings"], found["warnings"]);
                    assert_eq!(result["partial"], found["partial"]);
                    assert_eq!(result["truncated"], false);
                }
            }
            if let Some(id) = old_sid.as_ref().filter(|id| !sids.contains(*id)) {
                let missing = session.call(
                    name,
                    json!({"schemaVersion":1,"symbolId":id,"expectedBasis":basis}),
                    Some("not_found"),
                );
                assert_eq!(missing["error"]["currentBasis"], basis);
            }
        }
        if step == 0 {
            old_sid = initial_sid.clone();
        }
        prior_basis = Some(basis);
    }
    let calls = session.id - 1;
    session.close();
    drop(publisher);
    // Await worker state transitions and retirement before removing temporary files.
    {
        let mut registry = registry.lock().await;
        registry.disconnect(80000 + case as u64);
    }
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let seen = checkout_runtime.state_version();
            if registry.lock().await.release(&identity.root_key).unwrap() {
                break;
            }
            let worker = checkout_runtime.clone();
            tokio::task::spawn_blocking(move || {
                worker.wait_state_change(seen, trellis::testing::HANG_BACKSTOP)
            })
            .await
            .unwrap();
        }
    })
    .await
    .expect("checkout did not retire");
    (temp, calls, stale_sid_heads)
}

#[test]
fn generated_long_lived_stable_lookup_histories() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let cases: usize = std::env::var("TRELLIS_STABLE_SESSION_CASES")
        .ok()
        .map(|s| s.parse().unwrap())
        .unwrap_or(4);
    let mut seed: u64 = 20261102;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let forms = [Form::Inherent, Form::Generic, Form::Trait];
    let mut counts = [0usize; 3];
    let mut delimited = 0;
    let mut roots = Vec::new();
    let mut calls = 0;
    let mut stale_sid_heads = 0;
    for case in 0..cases {
        let ai = (next() % 3) as usize;
        let bi = (next() % 3) as usize;
        counts[ai] += 1;
        counts[bi] += 1;
        let a = forms[ai];
        let b = forms[bi];
        let noise = next() % 2 == 0;
        let path = if next() % 2 == 0 {
            "src/lib.rs"
        } else {
            delimited += 1;
            "src::lib.rs"
        };
        let history = [
            State {
                forms: vec![a],
                noise,
            },
            State {
                forms: vec![a, b],
                noise: !noise,
            },
            State {
                forms: vec![b],
                noise,
            },
            State {
                forms: vec![],
                noise: !noise,
            },
            State {
                forms: vec![a],
                noise,
            },
        ];
        let (root, case_calls, stale_heads) = runtime.block_on(run_case(case, path, &history));
        roots.push(root);
        calls += case_calls;
        stale_sid_heads += stale_heads;
    }
    eprintln!(
        "long-lived MCP sessions={cases} heads={} forms={counts:?} delimited_paths={delimited} tool_calls={calls} stale_sid_heads={stale_sid_heads} seed=20261102",
        cases * 5
    );
    drop(runtime);
    drop(roots);
}
