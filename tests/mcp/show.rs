#![cfg(feature = "test-support")]
//! `trellis_show` (#818) end to end in one daemon process: MCP calls on a daemon session and
//! the browser's status routes share one checkout registry, as `trellis serve` shares them.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
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
        status["viewRequest"]["generation"], third,
        "no refusal replaced the request"
    );

    // Every answer validates against the catalog's closed output schema.
    crate::mcp_protocol::validate_structured_content(&pairs);
    served.await.unwrap().unwrap();
}
