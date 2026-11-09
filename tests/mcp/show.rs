#![cfg(feature = "test-support")]
//! `trellis_show` (#818) end to end in one daemon process: MCP calls on a daemon session and
//! the browser's status routes share one checkout registry, as `trellis serve` shares them.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use trellis::daemon::registry::{CheckoutOptions, CheckoutRegistry};
use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

async fn browser(app: &axum::Router, path: &str) -> Value {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK, "{path}");
    serde_json::from_slice(&to_bytes(response.into_body(), 4 << 20).await.unwrap()).unwrap()
}

/// One legacy MCP session on a socket pair, served by the daemon's own coordinator.
struct Session {
    writer: std::os::unix::net::UnixStream,
    reader: BufReader<std::os::unix::net::UnixStream>,
    next: u64,
}
impl Session {
    fn ask(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let frame = json!({"jsonrpc":"2.0","id":self.next,"method":method,"params":params});
        self.writer
            .write_all(format!("{frame}\n").as_bytes())
            .unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], self.next, "{reply}");
        reply
    }
    fn show(&mut self, arguments: Value) -> Value {
        let reply = self.ask(
            "tools/call",
            json!({"name":"trellis_show","arguments":arguments}),
        );
        reply["result"]["structuredContent"].clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn show_checks_targets_records_one_latest_request_and_answers_typed_outcomes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(
        root.join("a.js"),
        "function Owner() { return Target(); }\nfunction Target() { return 1; }\nclass Shape {\n  area() { return 2; }\n}\n",
    )
    .unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(roots);
    registry
        .register(&identity, CheckoutOptions(json!({})))
        .unwrap();
    registry.attach_launch(1, &identity).unwrap();
    let registry = Arc::new(tokio::sync::Mutex::new(registry));
    let app = trellis::http::ProvisionedBrowser::new(
        registry.clone(),
        TOKEN.to_owned(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap()
    .router();
    let prefix = format!("/api/checkouts/{}", identity.root_key);
    let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
    let workspace = trellis::mcp::OpenedWorkspace::new(identity);
    let handle = tokio::runtime::Handle::current();
    let served = {
        let registry = registry.clone();
        tokio::task::spawn_blocking(move || {
            trellis::mcp::run_socket(workspace, server, registry, 1, handle)
        })
    };
    let pairs = tokio::task::spawn_blocking(move || {
        let mut mcp = Session {
            writer: client.try_clone().unwrap(),
            reader: BufReader::new(client),
            next: 0,
        };
        mcp.ask(
            "initialize",
            json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"show-test","version":"1"}}),
        );
        mcp.writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .unwrap();
        // Describe starts indexing. Wait for the first published head over MCP only: no
        // viewer has read status yet.
        mcp.ask(
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}}),
        );
        let started = std::time::Instant::now();
        let (owner, shape, area) = loop {
            let found = mcp.ask(
                "tools/call",
                json!({"name":"trellis_find_symbols","arguments":{"schemaVersion":1,"query":"a"}}),
            )["result"]["structuredContent"]
                .clone();
            let symbols = found["data"]["symbols"].as_array().cloned().unwrap_or_default();
            let id = |name: &str| {
                symbols
                    .iter()
                    .find(|symbol| symbol["name"] == name)
                    .map(|symbol| symbol["symbolId"].as_str().unwrap().to_owned())
            };
            if found["catchingUp"] == false
                && let (Some(owner), Some(shape), Some(area)) = (id("Owner"), id("Shape"), id("area"))
            {
                break (owner, shape, area);
            }
            assert!(started.elapsed() < trellis::testing::HANG_BACKSTOP, "{found}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let mut pairs = Vec::new();
        let mut show = |mcp: &mut Session, arguments: Value| {
            let answer = mcp.show(arguments);
            pairs.push(json!({"name":"trellis_show","structuredContent":answer.clone()}));
            answer
        };
        let sequence = json!({"schemaVersion":1,"view":"sequence","symbolId":owner});
        let no_viewer = show(&mut mcp, sequence.clone());
        (mcp, owner, shape, area, sequence, no_viewer, pairs)
    })
    .await
    .unwrap();
    let (mut mcp, owner, shape, area, sequence, no_viewer, mut pairs) = pairs;
    assert_eq!(no_viewer["data"]["outcome"], "no_viewer", "{no_viewer}");
    assert_eq!(no_viewer["data"]["generation"], Value::Null);
    let basis = no_viewer["evidenceBasis"].clone();
    // The browser's first poll is a viewer; no_viewer recorded nothing.
    let status = browser(&app, &format!("{prefix}/status")).await;
    assert_eq!(status["viewRequest"], Value::Null, "{status}");
    assert_eq!(status["revision"], basis);

    let mut ask = |mcp: Session, arguments: Value| {
        let (answer, mcp) = std::thread::spawn(move || {
            let mut mcp = mcp;
            (mcp.show(arguments), mcp)
        })
        .join()
        .unwrap();
        pairs.push(json!({"name":"trellis_show","structuredContent":answer.clone()}));
        (answer, mcp)
    };
    let (shown, next) = ask(mcp, sequence.clone());
    mcp = next;
    assert_eq!(shown["data"]["outcome"], "shown", "{shown}");
    let first = shown["data"]["generation"].as_u64().unwrap();
    let head = browser(&app, &format!("{prefix}/status/head")).await;
    assert_eq!(
        head["viewRequest"],
        json!({"generation":first,"view":"sequence","symbolId":owner,"revision":basis}),
        "status/head publishes the request"
    );

    // Each later target replaces it under a newer generation.
    let (class, next) = ask(
        mcp,
        json!({"schemaVersion":1,"view":"class","symbolId":area}),
    );
    mcp = next;
    assert_eq!(class["data"]["outcome"], "shown", "{class}");
    let second = class["data"]["generation"].as_u64().unwrap();
    assert!(second > first);
    let (source, next) = ask(
        mcp,
        json!({"schemaVersion":1,"view":"source","path":"a.js","line":3,"expectedBasis":basis}),
    );
    mcp = next;
    assert_eq!(source["data"]["outcome"], "shown", "{source}");
    let third = source["data"]["generation"].as_u64().unwrap();
    assert!(third > second);
    let status = browser(&app, &format!("{prefix}/status")).await;
    assert_eq!(
        status["viewRequest"],
        json!({"generation":third,"view":"source","path":"a.js","line":3,"revision":basis})
    );

    // Typed refusals record nothing.
    let other = json!({"indexGeneration":basis["indexGeneration"],
        "indexRevision":basis["indexRevision"].as_u64().unwrap() + 1});
    let refusals = [
        (
            json!({"schemaVersion":1,"view":"sequence","symbolId":owner,"expectedBasis":other}),
            "stale",
        ),
        (
            json!({"schemaVersion":1,"view":"sequence","symbolId":format!("sid:v1:{}", "0".repeat(32))}),
            "not_found",
        ),
        (
            json!({"schemaVersion":1,"view":"class","lookupKey":"a.js::Missing"}),
            "not_found",
        ),
        (
            json!({"schemaVersion":1,"view":"source","path":"missing.js","line":1}),
            "not_found",
        ),
        (
            json!({"schemaVersion":1,"view":"sequence","symbolId":shape}),
            "invalid",
        ),
        (
            json!({"schemaVersion":1,"view":"class","symbolId":owner}),
            "invalid",
        ),
        (
            json!({"schemaVersion":1,"view":"source","path":"a.js","line":99}),
            "invalid",
        ),
    ];
    for (arguments, outcome) in refusals {
        let (answer, next) = ask(mcp, arguments.clone());
        mcp = next;
        assert_eq!(answer["data"]["outcome"], outcome, "{arguments}: {answer}");
        assert_eq!(answer["data"]["generation"], Value::Null);
        assert_eq!(
            answer["evidenceBasis"], basis,
            "the admitted basis, to read again at"
        );
    }
    // #824: the launch root's final identity check runs at the call's send. A root that
    // changed there is `root_changed`, and the show it would have answered is never
    // recorded. The control releases the same pause with the root intact.
    let marker = root.join(".git/trellis/workspace-id");
    let phase = temp.path().join("phase.sock");
    let target = json!({"schemaVersion":1,"view":"source","path":"a.js","line":2});
    for drift in [true, false] {
        // The marker is moved aside and back, so the restored checkout keeps its identity.
        let aside = marker.with_extension("aside");
        let (answer, next) = paused_at_launch_final(mcp, target.clone(), &phase, || {
            if drift {
                std::fs::rename(&marker, &aside).unwrap();
            }
        });
        if drift {
            std::fs::rename(&aside, &marker).unwrap();
        }
        mcp = next;
        let status = browser(&app, &format!("{prefix}/status")).await;
        if drift {
            assert_eq!(answer["error"]["code"], "root_changed", "{answer}");
            assert!(answer.get("data").is_none());
            assert_eq!(
                status["viewRequest"]["generation"], third,
                "a failed call records nothing"
            );
        } else {
            assert_eq!(answer["data"]["outcome"], "shown", "{answer}");
            let generation = answer["data"]["generation"].as_u64().unwrap();
            assert!(generation > third);
            assert_eq!(status["viewRequest"]["generation"], generation);
            assert_eq!(status["viewRequest"]["line"], 2);
        }
        pairs.push(json!({"name":"trellis_show","structuredContent":answer}));
    }
    // A malformed target is the shared argument failure, before any read.
    let malformed = std::thread::spawn(move || {
        let answer = mcp.show(json!({"schemaVersion":1,"view":"source","symbolId":owner}));
        (answer, mcp)
    })
    .join()
    .unwrap();
    assert_eq!(malformed.0["error"]["code"], "invalid_request");
    pairs.push(json!({"name":"trellis_show","structuredContent":malformed.0.clone()}));
    drop(malformed.1);
    let status = browser(&app, &format!("{prefix}/status")).await;
    assert_eq!(
        status["viewRequest"]["line"], 2,
        "only the control's show replaced the request; no refusal did"
    );
    assert!(status["viewRequest"]["generation"].as_u64().unwrap() > third);

    // Every answer validates against the catalog's closed output schema.
    crate::mcp_protocol::validate_structured_content(&pairs);
    served.await.unwrap().unwrap();
}

/// One `trellis_show` call held at the coordinator's `launch_final` phase, the send-time root
/// check; `mutate` runs while the call is held.
fn paused_at_launch_final(
    mut mcp: Session,
    arguments: Value,
    socket: &std::path::Path,
    mutate: impl FnOnce(),
) -> (Value, Session) {
    use std::io::Read;
    let _ = std::fs::remove_file(socket);
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    // SAFETY: this nextest process runs only this test; nothing else reads these variables
    // concurrently with the write.
    unsafe {
        std::env::set_var("TRELLIS_TEST_MCP_PHASE_SOCKET", socket);
        std::env::set_var("TRELLIS_TEST_MCP_PHASE", "launch_final");
    }
    let call = std::thread::spawn(move || {
        let answer = mcp.show(arguments);
        (answer, mcp)
    });
    let (mut held, _) = listener.accept().unwrap();
    let mut stage = [0u8; 13];
    held.read_exact(&mut stage).unwrap();
    assert_eq!(&stage, b"launch_final\n");
    mutate();
    held.write_all(&[1]).unwrap();
    let (answer, mcp) = call.join().unwrap();
    unsafe {
        std::env::remove_var("TRELLIS_TEST_MCP_PHASE");
        std::env::remove_var("TRELLIS_TEST_MCP_PHASE_SOCKET");
    }
    (answer, mcp)
}

/// #824: a show is recorded only at its call's final successful send decision. Each call is
/// held at the coordinator's `send_decision` hook; the deadline case then passes its deadline
/// on the transport clock, the cancel case has its cancel read, and the control is released
/// unchanged. A source line over the 16 KiB read cap is the typed `invalid` outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn show_records_only_at_a_successful_send_decision() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use std::time::{Duration, Instant};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("a.js"), "function Owner() { return 1; }\n").unwrap();
    std::fs::write(
        root.join("long.js"),
        format!("//{}\nfunction Short() {{}}\n", "x".repeat(16_384)),
    )
    .unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let key = identity.root_key.clone();
    let mut registry = CheckoutRegistry::with_roots(TopologyRoots::isolated_for_tests(
        temp.path().join("cache"),
        temp.path().join("data"),
    ));
    registry
        .register(&identity, CheckoutOptions(json!({})))
        .unwrap();
    registry.attach_launch(1, &identity).unwrap();
    let registry = Arc::new(tokio::sync::Mutex::new(registry));
    // The transport clock reads real time until a held call freezes it at its acceptance.
    let frozen = Arc::new(Mutex::new(None::<Instant>));
    let skew = Arc::new(AtomicU64::new(0));
    let hold = Arc::new(AtomicBool::new(false));
    let counted = Arc::new(AtomicBool::new(false));
    let (entered, at_decision): (SyncSender<()>, Receiver<()>) = sync_channel(1);
    let (release, released) = sync_channel::<()>(1);
    let released = Mutex::new(released);
    let (frame, framed) = sync_channel::<()>(4);
    let hooks = trellis::mcp::TransportHooksForTests {
        paused_clock_for_tests: Some({
            let (frozen, skew) = (frozen.clone(), skew.clone());
            Arc::new(move || match *frozen.lock().unwrap() {
                Some(base) => base + Duration::from_millis(skew.load(Ordering::SeqCst)),
                None => Instant::now(),
            })
        }),
        send_decision: Some({
            let hold = hold.clone();
            Arc::new(move |_id| {
                if hold.swap(false, Ordering::SeqCst) {
                    entered.send(()).unwrap();
                    released.lock().unwrap().recv().unwrap();
                }
            })
        }),
        frame_read: Some({
            let counted = counted.clone();
            Arc::new(move || {
                if counted.swap(false, Ordering::SeqCst) {
                    frame.send(()).unwrap();
                }
            })
        }),
        ..Default::default()
    };
    let workspace =
        trellis::mcp::OpenedWorkspace::new(identity).with_transport_hooks_for_tests(hooks);
    let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
    let served = {
        let (registry, handle) = (registry.clone(), tokio::runtime::Handle::current());
        tokio::task::spawn_blocking(move || {
            trellis::mcp::run_socket(workspace, server, registry, 1, handle)
        })
    };
    tokio::task::spawn_blocking(move || {
        let mut mcp = Session {
            writer: client.try_clone().unwrap(),
            reader: BufReader::new(client),
            next: 0,
        };
        mcp.ask(
            "initialize",
            json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"show-test","version":"1"}}),
        );
        mcp.writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .unwrap();
        mcp.ask(
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}}),
        );
        let started = Instant::now();
        loop {
            let found = mcp.ask(
                "tools/call",
                json!({"name":"trellis_find_symbols","arguments":{"schemaVersion":1,"query":"Short"}}),
            )["result"]["structuredContent"]
                .clone();
            if found["catchingUp"] == false
                && found["data"]["symbols"]
                    .as_array()
                    .is_some_and(|symbols| !symbols.is_empty())
            {
                break;
            }
            assert!(started.elapsed() < trellis::testing::HANG_BACKSTOP, "{found}");
            std::thread::sleep(Duration::from_millis(20));
        }
        let runtime = registry.blocking_lock().runtime(&key).unwrap();
        runtime.view_requests().note_viewer(Instant::now());
        let requests = || runtime.view_requests().status_json();

        // Long line: typed `invalid`, not a range failure; the control's next line shows.
        let long = mcp.show(json!({"schemaVersion":1,"view":"source","path":"long.js","line":1}));
        assert_eq!(long["data"]["outcome"], "invalid", "{long}");
        assert!(long.get("error").is_none());
        assert_eq!(requests(), Value::Null);
        let short = mcp.show(json!({"schemaVersion":1,"view":"source","path":"long.js","line":2}));
        assert_eq!(short["data"]["outcome"], "shown", "{short}");
        let before = requests();
        assert_eq!(before["generation"], short["data"]["generation"]);

        let target = json!({"name":"trellis_show","arguments":
            {"schemaVersion":1,"view":"source","path":"a.js","line":1}});
        let held = |mcp: &mut Session| {
            mcp.next += 1;
            let frame = json!({"jsonrpc":"2.0","id":mcp.next,"method":"tools/call","params":target});
            *frozen.lock().unwrap() = Some(Instant::now());
            skew.store(0, Ordering::SeqCst);
            hold.store(true, Ordering::SeqCst);
            mcp.writer.write_all(format!("{frame}\n").as_bytes()).unwrap();
            at_decision.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap();
            assert_eq!(requests(), before, "nothing is recorded before the send decision");
            mcp.next
        };
        let reply = |mcp: &mut Session| {
            let mut line = String::new();
            mcp.reader.read_line(&mut line).unwrap();
            serde_json::from_str::<Value>(&line).unwrap()
        };

        // Deadline: the held call's deadline passes before its send decision.
        let id = held(&mut mcp);
        skew.store(6_000, Ordering::SeqCst);
        release.send(()).unwrap();
        let answer = reply(&mut mcp);
        assert_eq!(answer["id"], id);
        assert_eq!(
            answer["result"]["structuredContent"]["error"]["code"],
            "deadline_exceeded",
            "{answer}"
        );
        assert_eq!(requests(), before, "a deadline answer records nothing");

        // Cancel: the cancel is read before the held call's send decision, so the call is
        // never answered; a later call's reply proves the decision was made.
        let id = held(&mut mcp);
        counted.store(true, Ordering::SeqCst);
        mcp.writer
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}})
                )
                .as_bytes(),
            )
            .unwrap();
        framed.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap();
        release.send(()).unwrap();
        *frozen.lock().unwrap() = None;
        let after = mcp.ask("tools/list", json!({}));
        assert!(after["result"]["tools"].is_array(), "{after}");
        assert_eq!(requests(), before, "a cancelled call records nothing");

        // Control: the same held call, released unchanged, records exactly its answer.
        let id = held(&mut mcp);
        release.send(()).unwrap();
        let answer = reply(&mut mcp);
        assert_eq!(answer["id"], id);
        let data = &answer["result"]["structuredContent"]["data"];
        assert_eq!(data["outcome"], "shown", "{answer}");
        assert_eq!(requests()["generation"], data["generation"]);
        assert!(data["generation"].as_u64() > before["generation"].as_u64());
        assert_eq!(requests()["path"], "a.js");
        *frozen.lock().unwrap() = None;
    })
    .await
    .unwrap();
    served.await.unwrap().unwrap();
}
