use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    time::Duration,
};
use tempfile::TempDir;
use trellis::store::topology::WorkspaceIdentity;

async fn selected_json(
    app: axum::Router,
    method: &str,
    path: &str,
    body: Value,
) -> (axum::http::StatusCode, axum::http::HeaderMap, Value) {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("host", "127.0.0.1:7331")
                .header(
                    "authorization",
                    "Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, headers, body)
}

fn command(home: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_trellis"));
    cmd.env("HOME", home)
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME");
    cmd
}

fn git(root: &std::path::Path, args: &[&str]) {
    let result = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn connected_peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "macos")]
    unsafe {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of_val(&pid) as libc::socklen_t;
        let rc = libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut len,
        );
        (rc == 0 && len as usize == std::mem::size_of_val(&pid) && pid > 0).then_some(pid as u32)
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
        let rc = libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        );
        (rc == 0 && len as usize == std::mem::size_of_val(&cred) && cred.pid > 0)
            .then_some(cred.pid as u32)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = stream;
        None
    }
}

/// Test-scope guard for only the daemon elected under this disposable HOME.
struct FixtureDaemon(std::path::PathBuf);
impl FixtureDaemon {
    fn new(home: &std::path::Path) -> Self {
        Self(home.to_path_buf())
    }
}
impl Drop for FixtureDaemon {
    fn drop(&mut self) {
        let marker = self.0.join("auto-daemon.pid");
        // Test-support emits a PID marker; the default binary does not. In
        // either lane the private socket peer and exact executable identify
        // the fixture-owned daemon before a TERM is sent.
        let marker_pid = fs::read_to_string(&marker)
            .ok()
            .and_then(|raw| raw.parse::<u32>().ok());
        let mut dirs = vec![self.0.clone()];
        let mut socket = None;
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.file_name().is_some_and(|name| name == "daemon.lock") {
                    socket = path
                        .parent()
                        .and_then(std::path::Path::parent)
                        .map(|data| trellis::daemon::SocketPaths::new(data).socket);
                    break;
                }
                if path.is_dir() {
                    dirs.push(path);
                }
            }
            if socket.is_some() {
                break;
            }
        }
        let Some(socket) = socket else {
            return;
        };
        let owned = || {
            let connection = std::os::unix::net::UnixStream::connect(&socket).ok()?;
            let pid = connected_peer_pid(&connection)?;
            if marker_pid.is_some_and(|expected| expected != pid) {
                return None;
            }
            let output = Command::new("/bin/ps")
                .args(["-p", &pid.to_string(), "-o", "command="])
                .output()
                .ok()?;
            (output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim()
                    == format!("{} daemon", env!("CARGO_BIN_EXE_trellis")))
            .then_some((pid, connection))
        };
        let Some((pid, connection)) = owned() else {
            return;
        };
        let sent = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !sent {
            return;
        }
        let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
        while std::time::Instant::now() < deadline {
            if owned().is_none() {
                if marker_pid.is_some() {
                    let _ = fs::remove_file(&marker);
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Never SIGKILL a numeric PID after the socket identity becomes ambiguous.
        drop(connection);
    }
}

struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn production_browser_selects_two_real_worktrees_without_global_attachment() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let _daemon = FixtureDaemon::new(&home);
    fs::create_dir(&home).unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    fs::create_dir(&a).unwrap();
    git(&a, &["init", "-q"]);
    git(&a, &["config", "user.email", "test@example.invalid"]);
    git(&a, &["config", "user.name", "Test"]);
    fs::write(
        a.join("core.js"),
        "function alpha_checkout() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        a.join("core.py"),
        "class AlphaClass:\n    def execute(self):\n        return 1\n",
    )
    .unwrap();
    git(&a, &["add", "core.js", "core.py"]);
    git(&a, &["commit", "-qm", "base"]);
    git(
        &a,
        &["worktree", "add", "-qb", "other", b.to_str().unwrap()],
    );
    fs::write(
        b.join("core.js"),
        "function beta_checkout() { return 2; }\n",
    )
    .unwrap();
    fs::write(
        b.join("core.py"),
        "class BetaClass:\n    def execute(self):\n        return 2\n",
    )
    .unwrap();
    for root in [&a, &b] {
        let result = command(&home)
            .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
            .arg("index")
            .arg("--workspace")
            .arg(root)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "index: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let a_key = WorkspaceIdentity::discover(Some(&a), &a).unwrap().root_key;
    let b_key = WorkspaceIdentity::discover(Some(&b), &b).unwrap().root_key;
    assert_ne!(a_key, b_key);
    let seeds: Vec<String> = [(&a, "alpha_checkout"), (&b, "beta_checkout")]
        .into_iter()
        .map(|(root, name)| {
            let output = command(&home)
                .arg("symbols")
                .arg("--workspace")
                .arg(root)
                .output()
                .unwrap();
            assert!(output.status.success());
            let symbols: Value = serde_json::from_slice(&output.stdout).unwrap();
            symbols["data"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|symbol| symbol["name"] == name)
                .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    // CLI indexing activated both checkouts. Start browser registration from
    // a fresh daemon, not during the prior writer's ordinary watcher work.
    let marker = home.join("auto-daemon.pid");
    #[cfg(feature = "test-support")]
    assert!(marker.exists());
    drop(FixtureDaemon::new(&home));
    let data = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/dev.squashmerge.trellis")
    } else {
        home.join(".local/share/trellis")
    };
    let socket = trellis::daemon::SocketPaths::new(&data).socket;
    let stop_deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    while std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        assert!(
            std::time::Instant::now() < stop_deadline,
            "prior CLI daemon still accepts connections"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!marker.exists(), "prior CLI daemon was not safely stopped");
    let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let token_file = home.join("token");
    fs::write(&token_file, token).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let log = temp.path().join("serve-stderr");
    let mut serve = command(&home);
    #[cfg(feature = "test-support")]
    serve.env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"));
    let child = serve
        .arg("serve")
        .arg("--workspace")
        .arg(&a)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--token-file")
        .arg(token_file)
        .arg("--jev-budget-dir")
        .arg(temp.path().join("jev-budget"))
        .arg("--jev-budget-cents")
        .arg("10")
        .env("JEV_KEY", "offline-synthetic-key")
        .stdout(Stdio::null())
        .stderr(Stdio::from(fs::File::create(&log).unwrap()))
        .spawn()
        .unwrap();
    let mut server = Server(child);
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok(response) = client.get(format!("{base}/healthz")).send().await
                && response.status().is_success()
            {
                break;
            }
            assert!(
                server.0.try_wait().unwrap().is_none(),
                "serve exited: {}",
                fs::read_to_string(&log).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    #[cfg(feature = "test-support")]
    assert!(home.join("auto-daemon.pid").exists());
    let global = client
        .get(format!("{base}/api/daemon/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(global.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(global.headers()["cache-control"], "no-store");
    let global = client
        .get(format!("{base}/api/daemon/status"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(global.status(), reqwest::StatusCode::OK);
    let state: Value = global.json().await.unwrap();
    // The other checkout indexed through the daemon may remain active during
    // its normal release grace period.
    assert!(state["activeCheckouts"].as_u64().unwrap() <= 1);
    let listing = client
        .get(format!("{base}/api/checkouts"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(listing.status(), reqwest::StatusCode::OK);
    let listing: Value = listing.json().await.unwrap();
    for key in [&a_key, &b_key] {
        assert!(
            listing["checkouts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["rootKey"] == *key),
            "{listing}"
        );
    }
    let inert = client
        .get(format!("{base}/api/daemon/status"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert!(
        inert.json::<Value>().await.unwrap()["activeCheckouts"]
            .as_u64()
            .unwrap()
            <= 1
    );
    let bad_host = client
        .get(format!("{base}/api/checkouts/{a_key}/status"))
        .header("Host", "not-local:1")
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(bad_host.status(), reqwest::StatusCode::FORBIDDEN);
    assert!(!bad_host.headers().contains_key("X-Trellis-Workspace"));
    let bad_origin = client
        .get(format!("{base}/api/checkouts/{a_key}/status"))
        .header("Origin", "http://not-local:1")
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(bad_origin.status(), reqwest::StatusCode::FORBIDDEN);
    assert!(!bad_origin.headers().contains_key("X-Trellis-Workspace"));
    for (key, root, expected) in [
        (&a_key, &a, "alpha_checkout"),
        (&b_key, &b, "beta_checkout"),
    ] {
        let verified_root = root.canonicalize().unwrap();
        let status = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            loop {
                let response = client
                    .get(format!("{base}/api/checkouts/{key}/status"))
                    .bearer_auth(token)
                    .send()
                    .await
                    .unwrap();
                if response.status() == reqwest::StatusCode::OK {
                    break response;
                }
                assert_eq!(
                    response.status(),
                    reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    "{}",
                    response.text().await.unwrap_or_default()
                );
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            status.headers()["X-Trellis-Workspace"],
            verified_root.to_str().unwrap()
        );
        let status_header = status.headers()["X-Trellis-Catching-Up"]
            .to_str()
            .unwrap()
            .to_owned();
        let status: Value = status.json().await.unwrap();
        assert_eq!(status["workspaceRoot"], verified_root.to_str().unwrap());
        // TODO(runtime-recovery): prior-head admission must make this read
        // immediately available during H, as the preceding status was 200.
        // Until that slice lands, only a typed cold/H index_not_ready may wait.
        let jev = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            loop {
                let response = client
                    .get(format!("{base}/api/checkouts/{key}/jev/status"))
                    .bearer_auth(token)
                    .send()
                    .await
                    .unwrap();
                if response.status() == reqwest::StatusCode::OK {
                    break response;
                }
                let code = response.status();
                let body = response.text().await.unwrap_or_default();
                assert_eq!(code, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
                assert!(body.contains("index_not_ready"), "{body}");
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("JEV selected read stayed unavailable: root={root:?} key={key} status={status} catching-up-header={status_header}"));
        assert_eq!(
            jev.headers()["X-Trellis-Workspace"],
            verified_root.to_str().unwrap()
        );
        assert_eq!(jev.headers()["cache-control"], "no-store");
        let jev: Value = jev.json().await.unwrap();
        assert_eq!(jev["enabled"], root == &a, "{jev}");
        if root == &a {
            assert!(!jev["budget"].is_null());
        } else {
            assert!(jev["budget"].is_null());
        }
        assert!(status["catchingUp"].is_boolean());
        assert_eq!(
            status["catchingUp"].as_bool().unwrap().to_string(),
            status_header
        );
        // This test checks stable selected routes, not the prior-head/H gap.
        // Wait for this checkout's daemon-owned reconciliation before asserting
        // application errors such as directory_missing.
        tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            loop {
                let response = client
                    .get(format!("{base}/api/checkouts/{key}/status"))
                    .bearer_auth(token)
                    .send()
                    .await
                    .unwrap();
                if response.status() == reqwest::StatusCode::OK {
                    let state: Value = response.json().await.unwrap();
                    if state["catchingUp"] == false {
                        break;
                    }
                } else {
                    let code = response.status();
                    let body = response.text().await.unwrap_or_default();
                    assert_eq!(code, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
                    assert!(body.contains("index_not_ready"), "{body}");
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .expect("selected checkout did not finish reconciliation");
        let tree = client
            .get(format!("{base}/api/checkouts/{key}/tree"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(tree.status(), reqwest::StatusCode::OK);
        assert_eq!(
            tree.headers()["X-Trellis-Workspace"],
            verified_root.to_str().unwrap()
        );
        let tree: Value = tree.json().await.unwrap();
        assert!(tree["items"].to_string().contains("core.py"), "{tree}");
        let missing_dir = client
            .get(format!("{base}/api/checkouts/{key}/tree"))
            .query(&[("path", "absent")])
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(missing_dir.status(), reqwest::StatusCode::NOT_FOUND);
        assert_eq!(
            missing_dir.json::<Value>().await.unwrap()["error"]["code"],
            "directory_missing"
        );
        let invalid_dir = client
            .get(format!("{base}/api/checkouts/{key}/tree"))
            .query(&[("path", "core.py")])
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(
            invalid_dir.status(),
            reqwest::StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            invalid_dir.json::<Value>().await.unwrap()["error"]["code"],
            "invalid_directory"
        );
        let files = client
            .get(format!("{base}/api/checkouts/{key}/files"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(files.status(), reqwest::StatusCode::OK);
        assert_eq!(
            files.headers()["X-Trellis-Workspace"],
            verified_root.to_str().unwrap()
        );
        let files: Value = files.json().await.unwrap();
        assert!(files["items"].to_string().contains("core.js"), "{files}");
        let methods = client
            .get(format!("{base}/api/checkouts/{key}/methods"))
            .query(&[("path", "core.js")])
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(methods.status(), reqwest::StatusCode::OK);
        let methods: Value = methods.json().await.unwrap();
        assert!(methods.to_string().contains(expected), "{methods}");
        let class_name = if key == &a_key {
            "AlphaClass"
        } else {
            "BetaClass"
        };
        let foreign_class = if key == &a_key {
            "BetaClass"
        } else {
            "AlphaClass"
        };
        let classes = client
            .get(format!("{base}/api/checkouts/{key}/classes"))
            .query(&[("q", class_name)])
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(classes.status(), reqwest::StatusCode::OK);
        let classes: Value = classes.json().await.unwrap();
        assert!(classes.to_string().contains(class_name), "{classes}");
        assert!(!classes.to_string().contains(foreign_class), "{classes}");
        for bad in ["limit=101", "offset=1000001", "path=../core.py", "q=%00"] {
            let invalid = client
                .get(format!("{base}/api/checkouts/{key}/classes?{bad}"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap();
            assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(
                invalid.json::<Value>().await.unwrap()["error"]["code"],
                "invalid_class_request"
            );
        }
        let class_id = classes["items"][0]["symbol"]["id"].as_str().unwrap();
        let diagram = client
            .post(format!("{base}/api/checkouts/{key}/class-diagram"))
            .bearer_auth(token)
            .json(&serde_json::json!({"seed":class_id,"expectedRevision":classes["revision"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            diagram.status(),
            reqwest::StatusCode::OK,
            "{}",
            diagram.text().await.unwrap_or_default()
        );
        assert!(diagram.headers().contains_key("X-Trellis-Catching-Up"));
        let diagram: Value = diagram.json().await.unwrap();
        assert!(diagram.to_string().contains(class_name), "{diagram}");
        let navigation = client.post(format!("{base}/api/checkouts/{key}/navigation"))
            .bearer_auth(token).json(&serde_json::json!({"expectedRevision":classes["revision"],"path":"core.py","line":2}))
            .send().await.unwrap();
        assert_eq!(
            navigation.status(),
            reqwest::StatusCode::OK,
            "{}",
            navigation.text().await.unwrap_or_default()
        );
        assert!(navigation.headers().contains_key("X-Trellis-Workspace"));
        let navigation: Value = navigation.json().await.unwrap();
        assert_eq!(navigation["revision"], classes["revision"]);
        let foreign_key = if key == &a_key { &b_key } else { &a_key };
        let wrong = client
            .post(format!("{base}/api/checkouts/{foreign_key}/class-diagram"))
            .bearer_auth(token)
            .json(&serde_json::json!({"seed":class_id,"expectedRevision":classes["revision"]}))
            .send()
            .await
            .unwrap();
        assert!(!wrong.status().is_success());
        assert!(wrong.headers().contains_key("X-Trellis-Workspace"));

        let symbols = client
            .get(format!("{base}/api/checkouts/{key}/symbols"))
            .query(&[("q", expected)])
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(symbols.status(), reqwest::StatusCode::OK);
        let symbols: Value = symbols.json().await.unwrap();
        assert!(symbols.to_string().contains(expected), "{symbols}");
        let source = client
            .get(format!("{base}/api/checkouts/{key}/source"))
            .query(&[("path", "core.js")])
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(source.status(), reqwest::StatusCode::OK);
        assert_eq!(
            source.headers()["X-Trellis-Workspace"],
            verified_root.to_str().unwrap()
        );
        assert!(source.headers().contains_key("X-Trellis-Catching-Up"));
        let source: Value = source.json().await.unwrap();
        assert!(
            source["file"]["text"].as_str().unwrap().contains(expected),
            "{source}"
        );
    }
    for (key, other, seed, expected_name) in [
        (&a_key, &b_key, &seeds[0], "alpha_checkout"),
        (&b_key, &a_key, &seeds[1], "beta_checkout"),
    ] {
        let query = client
            .post(format!("{base}/api/checkouts/{key}/query"))
            .bearer_auth(token)
            .json(&serde_json::json!({"seed":seed}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            query.status(),
            reqwest::StatusCode::OK,
            "{}",
            query.text().await.unwrap_or_default()
        );
        assert!(query.headers().contains_key("X-Trellis-Catching-Up"));
        let view: Value = query.json().await.unwrap();
        assert_eq!(view["query"]["seed"], seed.as_str());
        assert!(view.to_string().contains(expected_name), "{view}");
        let revision = &view["revision"];
        let pinned = format!(
            "indexGeneration={}&indexRevision={}",
            revision["indexGeneration"].as_str().unwrap(),
            revision["indexRevision"].as_u64().unwrap()
        );
        let record = serde_json::json!({"id":"saved","title":"Mine","query":{"seed":seed},"pins":{},"hidden":[]});
        let saved = client
            .put(format!("{base}/api/checkouts/{key}/views/saved?{pinned}"))
            .bearer_auth(token)
            .json(&record)
            .send()
            .await
            .unwrap();
        assert_eq!(
            saved.status(),
            reqwest::StatusCode::OK,
            "{}",
            saved.text().await.unwrap_or_default()
        );
        assert!(saved.headers().contains_key("X-Trellis-Workspace"));
        let other_view = client
            .get(format!("{base}/api/checkouts/{other}/views/saved"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        if key == &a_key {
            assert_eq!(other_view.status(), reqwest::StatusCode::NOT_FOUND);
        } else {
            assert_eq!(other_view.status(), reqwest::StatusCode::OK);
            let other_value: Value = other_view.json().await.unwrap();
            assert_eq!(other_value["view"]["query"]["seed"], seeds[0]);
        }
        let note = serde_json::json!({"id":"note","nodeId":seed,"body":"Only here"});
        let saved = client
            .put(format!(
                "{base}/api/checkouts/{key}/annotations/note?{pinned}"
            ))
            .bearer_auth(token)
            .json(&note)
            .send()
            .await
            .unwrap();
        assert_eq!(
            saved.status(),
            reqwest::StatusCode::OK,
            "{}",
            saved.text().await.unwrap_or_default()
        );
        let other_notes = client
            .get(format!("{base}/api/checkouts/{other}/annotations"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(other_notes.status(), reqwest::StatusCode::OK);
        let other_notes: Value = other_notes.json().await.unwrap();
        if key == &a_key {
            assert_eq!(other_notes, serde_json::json!([]));
        } else {
            assert_eq!(other_notes[0]["annotation"]["nodeId"], seeds[0]);
        }
        let preview = client.post(format!("{base}/api/checkouts/{key}/questions/preview"))
            .bearer_auth(token).json(&serde_json::json!({"seed":seed,"question":"What calls?","expectedRevision":revision}))
            .send().await.unwrap();
        assert_eq!(
            preview.status(),
            reqwest::StatusCode::OK,
            "{}",
            preview.text().await.unwrap_or_default()
        );
        let preview: Value = preview.json().await.unwrap();
        let packet_id = preview["packet"]["packetId"].as_str().unwrap();
        let exported = client
            .get(format!(
                "{base}/api/checkouts/{key}/questions/{packet_id}/jev-request"
            ))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(exported.status(), reqwest::StatusCode::OK);
        if key == &a_key {
            assert_eq!(
                preview["packet"]["context"]["calls"]
                    .as_array()
                    .unwrap()
                    .len(),
                0
            );
            let jev_status_url = format!("{base}/api/checkouts/{a_key}/jev/status");
            let before: Value = client
                .get(&jev_status_url)
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(before["enabled"], true);
            let refused = client
                .post(format!(
                    "{base}/api/checkouts/{a_key}/questions/{packet_id}/jev-run"
                ))
                .bearer_auth(token)
                .json(&serde_json::json!({}))
                .send()
                .await
                .unwrap();
            assert_eq!(refused.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(
                refused.headers()["X-Trellis-Workspace"],
                a.canonicalize().unwrap().to_str().unwrap()
            );
            assert_eq!(refused.headers()["cache-control"], "no-store");
            let refusal: Value = refused.json().await.unwrap();
            assert_eq!(refusal["error"]["code"], "jev_no_candidates", "{refusal}");
            let after: Value = client
                .get(&jev_status_url)
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(
                after["budget"], before["budget"],
                "pre-network refusal must not reserve"
            );
            let foreign = client
                .post(format!(
                    "{base}/api/checkouts/{b_key}/questions/{packet_id}/jev-run"
                ))
                .bearer_auth(token)
                .json(&serde_json::json!({}))
                .send()
                .await
                .unwrap();
            assert_eq!(foreign.status(), reqwest::StatusCode::NOT_FOUND);
            assert_eq!(
                foreign.headers()["X-Trellis-Workspace"],
                b.canonicalize().unwrap().to_str().unwrap()
            );
            assert_eq!(
                foreign.json::<Value>().await.unwrap()["error"]["code"],
                "not_found"
            );
        }
        let imported = client
            .post(format!(
                "{base}/api/checkouts/{key}/questions/{packet_id}/jev-response"
            ))
            .bearer_auth(token)
            .json(&serde_json::json!({"model":"jev-1.13.0","answers":{}}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            imported.status(),
            reqwest::StatusCode::OK,
            "{}",
            imported.text().await.unwrap_or_default()
        );
        assert_eq!(
            imported.headers()["X-Trellis-Workspace"],
            (if key == &a_key {
                a.canonicalize().unwrap()
            } else {
                b.canonicalize().unwrap()
            })
            .to_str()
            .unwrap()
        );
        let selection = client
            .post(format!(
                "{base}/api/checkouts/{key}/questions/{packet_id}/selection"
            ))
            .bearer_auth(token)
            .json(&preview["selection"])
            .send()
            .await
            .unwrap();
        assert_eq!(
            selection.status(),
            reqwest::StatusCode::OK,
            "{}",
            selection.text().await.unwrap_or_default()
        );
        let invalid_selection = client
            .post(format!(
                "{base}/api/checkouts/{key}/questions/{packet_id}/selection"
            ))
            .bearer_auth(token)
            .json(&serde_json::json!({"packetId":"wrong","decisions":[]}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            invalid_selection.status(),
            reqwest::StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            invalid_selection.json::<Value>().await.unwrap()["error"]["code"],
            "invalid_question_selection"
        );
        let missing_seed = client.post(format!("{base}/api/checkouts/{key}/questions/preview"))
            .bearer_auth(token).json(&serde_json::json!({"seed":"missing","question":"What calls?","expectedRevision":revision}))
            .send().await.unwrap();
        assert_eq!(missing_seed.status(), reqwest::StatusCode::NOT_FOUND);
        assert_eq!(
            missing_seed.json::<Value>().await.unwrap()["error"]["code"],
            "not_found"
        );
        let foreign = client
            .get(format!(
                "{base}/api/checkouts/{other}/questions/{packet_id}/jev-request"
            ))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(foreign.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(foreign.headers().contains_key("X-Trellis-Workspace"));
        for (action, payload) in [
            (
                "jev-response",
                serde_json::json!({"model":"jev-1.13.0","answers":{}}),
            ),
            ("selection", preview["selection"].clone()),
        ] {
            let foreign = client
                .post(format!(
                    "{base}/api/checkouts/{other}/questions/{packet_id}/{action}"
                ))
                .bearer_auth(token)
                .json(&payload)
                .send()
                .await
                .unwrap();
            assert_eq!(foreign.status(), reqwest::StatusCode::NOT_FOUND);
            assert!(foreign.headers().contains_key("X-Trellis-Workspace"));
        }
        let sequence = client
            .post(format!("{base}/api/checkouts/{key}/sequence"))
            .bearer_auth(token)
            .json(&serde_json::json!({"seed":seed,"expectedRevision":view["revision"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            sequence.status(),
            reqwest::StatusCode::OK,
            "{}",
            sequence.text().await.unwrap_or_default()
        );
        let foreign_symbol = client
            .get(format!("{base}/api/checkouts/{other}/symbol"))
            .query(&[("id", seed)])
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(foreign_symbol.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(foreign_symbol.headers().contains_key("X-Trellis-Workspace"));
        let foreign_query = client
            .post(format!("{base}/api/checkouts/{other}/query"))
            .bearer_auth(token)
            .json(&serde_json::json!({"seed":seed}))
            .send()
            .await
            .unwrap();
        assert_eq!(foreign_query.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(foreign_query.headers().contains_key("X-Trellis-Workspace"));
        let accepted = client
            .post(format!("{base}/api/checkouts/{key}/index"))
            .bearer_auth(token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(accepted.status(), reqwest::StatusCode::ACCEPTED);
        let accepted: Value = accepted.json().await.unwrap();
        let id = accepted["id"].as_str().unwrap();
        let current = client
            .get(format!("{base}/api/checkouts/{key}/jobs/current"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(current.status(), reqwest::StatusCode::OK);
        assert!(current.headers().contains_key("X-Trellis-Workspace"));
        let current: Value = current.json().await.unwrap();
        assert!(current.is_null() || current["id"] == id, "{current}");
        let own = client
            .get(format!("{base}/api/checkouts/{key}/jobs/{id}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(own.status(), reqwest::StatusCode::OK);
        assert_eq!(own.json::<Value>().await.unwrap()["id"], id);
        let foreign = client
            .get(format!("{base}/api/checkouts/{other}/jobs/{id}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(foreign.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(foreign.headers().contains_key("X-Trellis-Workspace"));
        let cancelled = client
            .post(format!("{base}/api/checkouts/{key}/jobs/{id}/cancel"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert!(matches!(
            cancelled.status(),
            reqwest::StatusCode::CONFLICT | reqwest::StatusCode::OK
        ));
        assert!(cancelled.headers().contains_key("X-Trellis-Catching-Up"));
        let foreign_cancel = client
            .post(format!("{base}/api/checkouts/{other}/jobs/{id}/cancel"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(foreign_cancel.status(), reqwest::StatusCode::NOT_FOUND);
    }
    for suffix in ["views/saved", "annotations/note"] {
        let deleted = client
            .delete(format!("{base}/api/checkouts/{a_key}/{suffix}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(deleted.status(), reqwest::StatusCode::NO_CONTENT);
        assert_eq!(
            deleted.headers()["X-Trellis-Workspace"],
            a.canonicalize().unwrap().to_str().unwrap()
        );
        assert!(deleted.headers().contains_key("X-Trellis-Catching-Up"));
        let surviving = client
            .get(format!(
                "{base}/api/checkouts/{b_key}/{}",
                suffix.split('/').next().unwrap()
            ))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(surviving.status(), reqwest::StatusCode::OK);
        assert_eq!(
            surviving
                .json::<Value>()
                .await
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let deleted = client
            .delete(format!("{base}/api/checkouts/{b_key}/{suffix}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(deleted.status(), reqwest::StatusCode::NO_CONTENT);
        assert_eq!(
            deleted.headers()["X-Trellis-Workspace"],
            b.canonicalize().unwrap().to_str().unwrap()
        );
    }
    let missing = client
        .get(format!("{base}/api/status"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    assert!(!missing.headers().contains_key("X-Trellis-Workspace"));
    let unknown = client
        .get(format!("{base}/api/checkouts/{}/status", "a".repeat(64)))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert!(!unknown.status().is_success());
    assert!(!unknown.headers().contains_key("X-Trellis-Workspace"));
    let unknown: Value = unknown.json().await.unwrap();
    assert_eq!(unknown["error"]["code"], "workspace_selection_failed");
    let resolved = client
        .get(format!("{base}/api/checkouts/{a_key}/source"))
        .query(&[("path", "absent.js")])
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resolved.status(), reqwest::StatusCode::NOT_FOUND);
    assert_eq!(
        resolved.headers()["X-Trellis-Workspace"],
        a.canonicalize().unwrap().to_str().unwrap()
    );
    assert!(resolved.headers().contains_key("X-Trellis-Catching-Up"));
    let invalid = client
        .get(format!("{base}/api/checkouts/not-a-key/status"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(!invalid.headers().contains_key("X-Trellis-Workspace"));
    // Test the forbidden directory after all pinned mutations. Creating it
    // wakes the daemon watcher, so it must not invalidate their captured pin.
    for (key, root) in [(&a_key, &a), (&b_key, &b)] {
        let verified_root = root.canonicalize().unwrap();
        let private = root.join("private");
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o000)).unwrap();
        let forbidden_dir = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            loop {
                let response = client
                    .get(format!("{base}/api/checkouts/{key}/tree"))
                    .query(&[("path", "private")])
                    .bearer_auth(token)
                    .send()
                    .await
                    .unwrap();
                if response.status() != reqwest::StatusCode::SERVICE_UNAVAILABLE {
                    break response;
                }
                let headers = response.headers().clone();
                let error: Value = response.json().await.unwrap();
                assert_eq!(
                    headers["X-Trellis-Workspace"],
                    verified_root.to_str().unwrap()
                );
                assert_eq!(error["error"]["code"], "index_not_ready", "{error}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("selected tree did not leave transient H admission");
        assert_eq!(forbidden_dir.status(), reqwest::StatusCode::FORBIDDEN);
        assert_eq!(
            forbidden_dir.json::<Value>().await.unwrap()["error"]["code"],
            "directory_forbidden"
        );
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let changed = temp.path().join("moved-a");
    fs::rename(&a, &changed).unwrap();
    let drift = client
        .get(format!("{base}/api/checkouts/{a_key}/status"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(drift.status(), reqwest::StatusCode::CONFLICT);
    assert!(!drift.headers().contains_key("X-Trellis-Workspace"));
}

#[test]
fn selected_browser_clock_has_separate_disconnect_release_and_exit_delays() {
    use trellis::{daemon::registry::CheckoutRegistry, store::topology::TopologyRoots};
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let start = std::time::Instant::now();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots_at(roots, start);
    registry.browser_request_at(&identity, start).unwrap();
    assert_eq!(registry.active_count(), 1);
    assert_eq!(registry.idle_exit_deadline(), None);
    assert!(
        !registry
            .advance(start + Duration::from_secs(15 * 60 - 1))
            .unwrap()
            .exit
    );
    assert_eq!(registry.active_count(), 1);
    registry
        .advance(start + Duration::from_secs(15 * 60))
        .unwrap();
    assert_eq!(
        registry.idle_exit_deadline(),
        Some(start + Duration::from_secs(45 * 60))
    );
    assert!(
        registry
            .advance(start + Duration::from_secs(30 * 60 - 1))
            .unwrap()
            .released
            .is_empty()
    );
    let release = registry
        .advance(start + Duration::from_secs(30 * 60))
        .unwrap();
    assert_eq!(release.released, vec![identity.root_key.clone()]);
    assert!(!release.exit);
    assert!(
        !registry
            .advance(start + Duration::from_secs(45 * 60 - 1))
            .unwrap()
            .exit
    );
    assert!(
        registry
            .advance(start + Duration::from_secs(45 * 60))
            .unwrap()
            .exit
    );
}

#[test]
fn selected_browser_renewal_does_not_extend_other_checkout() {
    use trellis::{daemon::registry::CheckoutRegistry, store::topology::TopologyRoots};
    let temp = TempDir::new().unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    let a = WorkspaceIdentity::discover(Some(&a), &a).unwrap();
    let b = WorkspaceIdentity::discover(Some(&b), &b).unwrap();
    let start = std::time::Instant::now();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots_at(roots, start);
    registry.browser_request_at(&a, start).unwrap();
    registry.browser_request_at(&b, start).unwrap();
    registry
        .browser_request_at(&a, start + Duration::from_secs(10 * 60))
        .unwrap();
    let tick = registry
        .advance(start + Duration::from_secs(30 * 60))
        .unwrap();
    assert_eq!(tick.released, vec![b.root_key]);
    assert_eq!(registry.active_count(), 1);
    assert_eq!(
        registry.idle_exit_deadline(),
        Some(start + Duration::from_secs(55 * 60))
    );
    let tick = registry
        .advance(start + Duration::from_secs(40 * 60))
        .unwrap();
    assert_eq!(tick.released, vec![a.root_key]);
    assert!(!tick.exit);
}

#[test]
fn global_list_marks_corrupt_existing_index_without_attaching() {
    use trellis::{daemon::registry::CheckoutRegistry, store::topology::TopologyRoots};
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let index = roots.index_db(&identity);
    drop(roots.index_use(&identity).unwrap());
    fs::write(&index, b"not a sqlite index").unwrap();
    fs::set_permissions(&index, fs::Permissions::from_mode(0o600)).unwrap();
    let registry = CheckoutRegistry::with_roots(roots);
    let rows = registry.browser_checkouts();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["rootKey"], identity.root_key);
    assert_eq!(rows[0]["state"], "corrupt");
    assert_eq!(rows[0]["active"], false);
    assert_eq!(registry.active_count(), 0);
}

#[test]
fn global_discovery_reuses_live_sqlite_witness_and_never_repairs() {
    use trellis::{
        daemon::registry::CheckoutRegistry,
        store::{Store, topology::TopologyRoots},
    };
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("core.js"), "function stable() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let store = Store::open(
        roots.clone(),
        WorkspaceIdentity::discover(Some(&root), &root).unwrap(),
    )
    .unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (_, session) = trellis::index_coordinator::reconcile_workspace(
        &store,
        &trellis::indexer::IndexOptions::new(identity.root.clone()),
        &cancel,
        |_| {},
    )
    .unwrap();
    let dir = roots.index_dir(&identity);
    let names = || {
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let before = names();
    let witness_count = open_descriptors(&roots.index_db(&identity));
    let registry = CheckoutRegistry::with_roots(roots.clone());
    for _ in 0..4 {
        let rows = registry.browser_checkouts();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["state"], "available", "{rows:?}");
        assert_eq!(
            Store::browser_index_root_existing(&roots, &identity.root_key).unwrap(),
            identity.root.to_str().unwrap()
        );
        assert_eq!(open_descriptors(&roots.index_db(&identity)), witness_count);
        session.verify().unwrap();
        store.status().unwrap();
        assert_eq!(names(), before);
    }
    let sidecar = dir.join("index.db-journal");
    fs::write(&sidecar, b"hot journal").unwrap();
    let blocked = registry.browser_checkouts();
    assert_eq!(blocked[0]["state"], "storage_busy");
    assert_eq!(fs::read(&sidecar).unwrap(), b"hot journal");
}

#[tokio::test]
async fn resolved_root_change_retains_headers_even_when_read_fence_fails() {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tower::ServiceExt;
    use trellis::{
        daemon::registry::{CheckoutOptions, CheckoutRegistry},
        http::ProvisionedBrowser,
        store::topology::TopologyRoots,
    };
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    for (stage, route_suffix) in [
        ("after_handler", "status"),
        ("before_read_finish", "status"),
        ("after_handler", "files"),
        ("before_read_finish", "files"),
        ("after_handler", "tree"),
        ("before_read_finish", "classes"),
        ("after_handler", "symbols"),
        ("before_read_finish", "views"),
    ] {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("core.js"), "function stable() {}\n").unwrap();
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let roots =
            TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
        let queue_path = roots.requests_db(&identity);
        let mut registry = CheckoutRegistry::with_roots(roots);
        registry
            .register(&identity, CheckoutOptions(serde_json::json!({})))
            .unwrap();
        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let address = "127.0.0.1:7331".parse().unwrap();
        let browser = ProvisionedBrowser::new(registry.clone(), TOKEN.to_owned(), address).unwrap();
        let app = browser.clone().router();
        let route = format!("/api/checkouts/{}/{route_suffix}", identity.root_key);
        let call = |app: axum::Router| {
            let route = route.clone();
            async move {
                app.oneshot(
                    Request::builder()
                        .uri(route)
                        .header("host", "127.0.0.1:7331")
                        .header("authorization", format!("Bearer {TOKEN}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };
        let mut last_code = None;
        let mut last_error_code = None;
        let mut saw_runtime = false;
        let mut first_h_error = None;
        let mut saw_retry_waiting = false;
        tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            loop {
                let response = call(app.clone()).await;
                last_code = Some(response.status());
                if response.status() == axum::http::StatusCode::OK {
                    break;
                }
                last_error_code = to_bytes(response.into_body(), 1024 * 1024)
                    .await
                    .ok()
                    .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
                    .and_then(|body| body["error"]["code"].as_str().map(str::to_owned));
                let runtime = registry.lock().await.runtime(&identity.root_key);
                saw_runtime |= runtime.is_some();
                if let Some(runtime) = runtime {
                    let waiting = runtime.retry_waiting_for_tests();
                    saw_retry_waiting |= waiting;
                    if first_h_error.is_none() {
                        first_h_error = runtime.reconciliation_error().map(|error| (error, waiting));
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{stage}/{route_suffix} cold selected route did not become ready: {last_code:?}/{last_error_code:?}; runtime seen: {saw_runtime}; first H error/retry: {first_h_error:?}; saw retry waiting: {saw_retry_waiting}; queue exists: {}", queue_path.exists()));
        let changed = Arc::new(AtomicBool::new(false));
        let source = identity.root.clone();
        let destination = temp.path().join("moved");
        let changed_for_hook = changed.clone();
        browser.set_selected_response_hook_for_tests(Arc::new(move |at| {
            if at == stage && !changed_for_hook.swap(true, Ordering::AcqRel) {
                fs::rename(&source, &destination).unwrap();
            }
        }));
        let response = call(app).await;
        assert!(changed.load(Ordering::Acquire));
        assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
        assert_eq!(
            response.headers()["X-Trellis-Workspace"],
            identity.root.to_str().unwrap()
        );
        assert!(response.headers().contains_key("X-Trellis-Catching-Up"));
        let error: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(error["error"]["code"], "root_changed", "{stage}: {error}");
    }
}

#[tokio::test]
async fn selected_question_export_keeps_legacy_oversize_error() {
    use std::sync::Arc;
    use trellis::{
        daemon::registry::{CheckoutOptions, CheckoutRegistry},
        http::ProvisionedBrowser,
        store::topology::TopologyRoots,
    };
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("large.js"),
        format!(
            "function big() {{\n/*{}*/\nreturn 1;\n}}\n",
            "x".repeat(180_000)
        ),
    )
    .unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(roots);
    registry
        .register(&identity, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    let app = ProvisionedBrowser::new(
        Arc::new(tokio::sync::Mutex::new(registry)),
        TOKEN.to_owned(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap()
    .router();
    let prefix = format!("/api/checkouts/{}", identity.root_key);
    let pin = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (status, _, value) =
                selected_json(app.clone(), "GET", &format!("{prefix}/status"), Value::Null).await;
            if status == axum::http::StatusCode::OK && value["catchingUp"] == false {
                break value["revision"].clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let (status, _, symbols) = selected_json(
        app.clone(),
        "GET",
        &format!("{prefix}/symbols?q=big"),
        Value::Null,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let seed = symbols["items"][0]["id"].as_str().unwrap();
    let (status, _, preview) = selected_json(
        app.clone(),
        "POST",
        &format!("{prefix}/questions/preview"),
        serde_json::json!({"seed":seed,"question":"Explain this","expectedRevision":pin}),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "{status}: {}",
        preview["error"]
    );
    let id = preview["packet"]["packetId"].as_str().unwrap();
    let (status, headers, error) = selected_json(
        app,
        "GET",
        &format!("{prefix}/questions/{id}/jev-request"),
        Value::Null,
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "{error}"
    );
    assert_eq!(error["error"]["code"], "evidence_too_large");
    assert_eq!(
        headers["X-Trellis-Workspace"],
        identity.root.to_str().unwrap()
    );
}

#[tokio::test]
async fn selected_question_preview_keeps_legacy_oversize_error() {
    use std::sync::Arc;
    use trellis::{
        daemon::registry::{CheckoutOptions, CheckoutRegistry},
        http::ProvisionedBrowser,
        store::topology::TopologyRoots,
    };
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("large.js"),
        format!(
            "function big() {{\n/*{}*/\nreturn 1;\n}}\n",
            "x".repeat(1_060_000)
        ),
    )
    .unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(roots);
    registry
        .register(&identity, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    let registry = Arc::new(tokio::sync::Mutex::new(registry));
    let app = ProvisionedBrowser::new(
        registry.clone(),
        TOKEN.to_owned(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap()
    .router();
    let prefix = format!("/api/checkouts/{}", identity.root_key);
    let mut last_status = None;
    let mut first_h_error = None;
    let mut saw_retry_waiting = false;
    let pin = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (status, _, value) =
                selected_json(app.clone(), "GET", &format!("{prefix}/status"), Value::Null).await;
            last_status = Some((status, value.clone()));
            // Sample while H is still retrying. The first error can disappear
            // after a successful retry, before the final timeout diagnostic.
            if (status != axum::http::StatusCode::OK || value["catchingUp"] != false)
                && let Some(runtime) = registry.lock().await.runtime(&identity.root_key)
            {
                let waiting = runtime.retry_waiting_for_tests();
                saw_retry_waiting |= waiting;
                if first_h_error.is_none() {
                    first_h_error = runtime.reconciliation_error().map(|error| (error, waiting));
                }
            }
            if status == axum::http::StatusCode::OK && value["catchingUp"] == false {
                break value["revision"].clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let pin = pin.unwrap_or_else(|_| {
        panic!("cold selected preview status stayed not-ready: {last_status:?}; first H error/retry: {first_h_error:?}; saw retry waiting: {saw_retry_waiting}")
    });
    let (status, _, symbols) = selected_json(
        app.clone(),
        "GET",
        &format!("{prefix}/symbols?q=big"),
        Value::Null,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let seed = symbols["items"][0]["id"].as_str().unwrap();
    let (status, headers, error) = selected_json(
        app,
        "POST",
        &format!("{prefix}/questions/preview"),
        serde_json::json!({"seed":seed,"question":"Explain this","expectedRevision":pin}),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "{error}"
    );
    assert_eq!(error["error"]["code"], "evidence_too_large");
    assert_eq!(
        headers["X-Trellis-Workspace"],
        identity.root.to_str().unwrap()
    );
}

#[tokio::test]
async fn selected_saved_mutation_reports_committed_drift_and_preserves_real_record_state() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use trellis::{
        daemon::registry::{CheckoutOptions, CheckoutRegistry},
        http::ProvisionedBrowser,
        store::topology::{DurableRecords, TopologyRoots},
    };
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    for kind in ["view", "annotation"] {
        for method in ["PUT", "DELETE"] {
            for stage in ["before_mutation", "after_handler"] {
                let temp = TempDir::new().unwrap();
                let root = temp.path().join("checkout");
                fs::create_dir(&root).unwrap();
                fs::write(root.join("core.js"), "function stable() {}\n").unwrap();
                let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
                let roots = TopologyRoots::isolated_for_tests(
                    temp.path().join("cache"),
                    temp.path().join("data"),
                );
                let mut registry = CheckoutRegistry::with_roots(roots.clone());
                registry
                    .register(&identity, CheckoutOptions(serde_json::json!({})))
                    .unwrap();
                let browser = ProvisionedBrowser::new(
                    Arc::new(tokio::sync::Mutex::new(registry)),
                    TOKEN.to_owned(),
                    "127.0.0.1:7331".parse().unwrap(),
                )
                .unwrap();
                let app = browser.clone().router();
                let prefix = format!("/api/checkouts/{}", identity.root_key);
                let pin = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
                    loop {
                        let (status, _, value) = selected_json(
                            app.clone(),
                            "GET",
                            &format!("{prefix}/status"),
                            Value::Null,
                        )
                        .await;
                        if status == axum::http::StatusCode::OK && value["catchingUp"] == false {
                            break value["revision"].clone();
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
                let (status, _, symbols) = selected_json(
                    app.clone(),
                    "GET",
                    &format!("{prefix}/symbols?q=stable"),
                    Value::Null,
                )
                .await;
                assert_eq!(status, axum::http::StatusCode::OK);
                let seed = symbols["items"][0]["id"].as_str().unwrap();
                let pin = format!(
                    "indexGeneration={}&indexRevision={}",
                    pin["indexGeneration"].as_str().unwrap(),
                    pin["indexRevision"].as_u64().unwrap()
                );
                let (suffix, payload) = if kind == "view" {
                    (
                        "views/saved",
                        serde_json::json!({"id":"saved","title":"Saved","query":{"seed":seed},"pins":{},"hidden":[]}),
                    )
                } else {
                    (
                        "annotations/note",
                        serde_json::json!({"id":"note","nodeId":seed,"body":"Saved"}),
                    )
                };
                let uri = format!("{prefix}/{suffix}?{pin}");
                if method == "DELETE" {
                    let (status, _, value) =
                        selected_json(app.clone(), "PUT", &uri, payload.clone()).await;
                    assert_eq!(status, axum::http::StatusCode::OK, "{value}");
                }
                let moved = temp.path().join("moved");
                let changed = Arc::new(AtomicBool::new(false));
                let changed_hook = changed.clone();
                let old_root = root.clone();
                let target = moved.clone();
                browser.set_selected_response_hook_for_tests(Arc::new(move |at| {
                    if at == stage && !changed_hook.swap(true, Ordering::AcqRel) {
                        fs::rename(&old_root, &target).unwrap();
                    }
                }));
                let (status, headers, value) =
                    selected_json(app.clone(), method, &uri, payload).await;
                assert!(changed.load(Ordering::Acquire));
                assert_eq!(
                    status,
                    axum::http::StatusCode::CONFLICT,
                    "{kind} {method} {stage}: {value}"
                );
                assert_eq!(value["error"]["code"], "root_changed");
                assert_eq!(
                    headers["X-Trellis-Workspace"],
                    identity.root.to_str().unwrap()
                );
                assert!(headers.contains_key("X-Trellis-Catching-Up"));
                if stage == "after_handler" {
                    assert_eq!(value["mutationOutcome"], "committed");
                    assert_eq!(headers["X-Trellis-Mutation-Outcome"], "committed");
                } else {
                    assert!(value.get("mutationOutcome").is_none());
                    assert!(!headers.contains_key("X-Trellis-Mutation-Outcome"));
                }
                fs::rename(&moved, &root).unwrap();
                let records = DurableRecords::new(&roots, &identity);
                let exists = if kind == "view" {
                    records.view_record("saved").unwrap().is_some()
                } else {
                    records
                        .annotation_records()
                        .unwrap()
                        .iter()
                        .any(|row| row.id == "note")
                };
                assert_eq!(
                    exists,
                    (method == "PUT") == (stage == "after_handler"),
                    "{kind} {method} {stage}"
                );
            }
        }
    }
}

#[cfg(unix)]
fn open_descriptors(path: &std::path::Path) -> usize {
    use std::os::unix::fs::MetadataExt;
    let expected = fs::metadata(path).unwrap();
    (0..1024)
        .filter(|fd| {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe { libc::fstat(*fd, stat.as_mut_ptr()) } != 0 {
                return false;
            }
            let stat = unsafe { stat.assume_init() };
            #[cfg(target_os = "macos")]
            let dev = stat.st_dev as u64;
            #[cfg(not(target_os = "macos"))]
            let dev = stat.st_dev;
            (dev, stat.st_ino) == (expected.dev(), expected.ino())
        })
        .count()
}

#[tokio::test]
async fn browser_only_live_listener_releases_resources_then_exits_with_control_present() {
    use std::sync::Arc;
    use trellis::{
        daemon::{
            BrowserProvisioner,
            registry::{CheckoutOptions, CheckoutRegistry},
        },
        store::topology::TopologyRoots,
    };
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("core.js"), "function stable() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let index_lock = roots.index_use_lock(&identity);
    let registry = Arc::new(tokio::sync::Mutex::new(CheckoutRegistry::with_roots(roots)));
    let mut provisioner = BrowserProvisioner::new();
    let address = provisioner
        .register_serve(
            &registry,
            &identity,
            CheckoutOptions(serde_json::json!({})),
            "127.0.0.1:0".parse().unwrap(),
            &temp.path().join("token"),
        )
        .await
        .unwrap();
    let token = fs::read_to_string(temp.path().join("token")).unwrap();
    // Match run_daemon: its idle driver signals the provisioned listener and
    // closes the dispatch-owned serve control connection on daemon exit.
    let (mut serve_control, daemon_control) = std::os::unix::net::UnixStream::pair().unwrap();
    serve_control
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let serve_exit = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::io::Read;
        let mut one = [0u8; 1];
        if serve_control.read(&mut one)? == 0 {
            Ok(())
        } else {
            Err(std::io::Error::other(
                "serve control received data instead of EOF",
            ))
        }
    });
    let (shutdown, signal) = tokio::sync::watch::channel(false);
    let listener = provisioner.spawn_with_shutdown(signal).unwrap();
    let idle_registry = registry.clone();
    let daemon_exit = tokio::spawn(async move {
        trellis::daemon::run_idle_lifecycle(idle_registry)
            .await
            .unwrap();
        shutdown.send(true).unwrap();
        drop(daemon_control);
    });
    let client = reqwest::Client::new();
    let base = format!("http://{address}");
    let before_request = std::time::Instant::now();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let response = client
                .get(format!("{base}/api/checkouts/{}/status", identity.root_key))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap();
            if response.status().is_success()
                && !response.json::<Value>().await.unwrap()["catchingUp"]
                    .as_bool()
                    .unwrap()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let after_request = std::time::Instant::now();
    let runtime = registry.lock().await.runtime(&identity.root_key).unwrap();
    assert!(runtime.has_active_resources());
    assert!(open_descriptors(&index_lock) > 0);
    for route in ["/healthz", "/", "/api/checkouts", "/api/daemon/status"] {
        let request = client.get(format!("{base}{route}"));
        let response = if route.starts_with("/api/") {
            request.bearer_auth(&token).send().await.unwrap()
        } else {
            request.send().await.unwrap()
        };
        assert!(response.status().is_success());
    }
    let mut checked = registry.lock().await;
    checked.set_clock_override_for_tests(
        before_request + Duration::from_secs(15 * 60) - Duration::from_nanos(1),
    );
    assert!(!checked.advance(std::time::Instant::now()).unwrap().exit);
    assert!(runtime.has_active_resources());
    checked.set_clock_override_for_tests(after_request + Duration::from_secs(15 * 60));
    checked.advance(std::time::Instant::now()).unwrap();
    assert!(runtime.has_active_resources());
    checked.set_clock_override_for_tests(
        before_request + Duration::from_secs(30 * 60) - Duration::from_nanos(1),
    );
    checked.advance(std::time::Instant::now()).unwrap();
    assert!(runtime.has_active_resources());
    assert!(
        open_descriptors(&index_lock) > 0,
        "active browser runtime must retain its index-use descriptor"
    );
    checked.set_clock_override_for_tests(after_request + Duration::from_secs(30 * 60));
    checked.advance(std::time::Instant::now()).unwrap();
    drop(checked);
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while runtime.has_active_resources() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(open_descriptors(&index_lock), 0);
    assert!(!listener.is_finished());
    assert!(!serve_exit.is_finished());
    assert!(!daemon_exit.is_finished());
    registry.lock().await.set_clock_override_for_tests(
        before_request + Duration::from_secs(45 * 60) - Duration::from_nanos(1),
    );
    assert!(!listener.is_finished());
    assert!(!serve_exit.is_finished());
    registry
        .lock()
        .await
        .set_clock_override_for_tests(after_request + Duration::from_secs(45 * 60));
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, daemon_exit)
        .await
        .unwrap()
        .unwrap();
    let observed_eof = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, serve_exit).await;
    assert!(
        observed_eof.is_ok(),
        "serve control did not reach EOF after daemon exit"
    );
    observed_eof.unwrap().unwrap().unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, listener)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(provisioner.address().is_none());
    // The joined serve task owned and dropped the TcpListener, so the port is
    // released. Probing that nothing listens there races with any other
    // process taking the freed port (#131).
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn cold_selected_native_read_is_not_ready_without_a_fabricated_basis() {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use std::sync::Arc;
    use tower::ServiceExt;
    use trellis::{
        daemon::registry::{CheckoutOptions, CheckoutRegistry},
        http::ProvisionedBrowser,
        store::topology::TopologyRoots,
    };
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("cold");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("core.js"), "function cold() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(roots);
    registry
        .register(&identity, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    let registry = Arc::new(tokio::sync::Mutex::new(registry));
    let browser = ProvisionedBrowser::new(
        registry.clone(),
        TOKEN.to_owned(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let app = browser.router();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    let runtime = {
        let mut registry = registry.lock().await;
        registry
            .browser_request_at(&identity, std::time::Instant::now())
            .unwrap();
        let runtime = registry.activate(&identity.root_key).unwrap();
        runtime.set_pre_h_hook_for_tests(Arc::new(move || {
            entered_tx.send(()).unwrap();
            resume_rx.lock().unwrap().recv().unwrap();
        }));
        runtime
    };
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while entered_rx.try_recv().is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/checkouts/{}/files", identity.root_key))
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        response.headers()["X-Trellis-Workspace"],
        identity.root.to_str().unwrap()
    );
    assert_eq!(response.headers()["X-Trellis-Catching-Up"], "true");
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(body["error"]["code"], "index_not_ready");
    assert!(body.get("revision").is_none());
    resume_tx.send(()).unwrap();
    drop(runtime);
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn selected_status_serves_validated_pre_h_head_and_clears_freshness_after_h() {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use std::sync::Arc;
    use tower::ServiceExt;
    use trellis::{
        daemon::registry::{CheckoutOptions, CheckoutRegistry},
        http::ProvisionedBrowser,
        store::topology::TopologyRoots,
    };
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("core.js"), "function old_step() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(roots.clone());
    registry
        .register(&identity, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    let registry = Arc::new(tokio::sync::Mutex::new(registry));
    let app = ProvisionedBrowser::new(
        registry.clone(),
        TOKEN.to_owned(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap()
    .router();
    let path = format!("/api/checkouts/{}/status", identity.root_key);
    let status = |app: axum::Router| {
        let path = path.clone();
        async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("host", "127.0.0.1:7331")
                        .header("authorization", format!("Bearer {TOKEN}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let headers = response.headers().clone();
            let code = response.status();
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            (code, headers, body)
        }
    };
    let old_revision = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (code, headers, body) = status(app.clone()).await;
            if code == axum::http::StatusCode::OK && body["catchingUp"] == false {
                assert_eq!(headers["X-Trellis-Catching-Up"], "false");
                break body["revision"].clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let prefix = format!("/api/checkouts/{}", identity.root_key);
    let (code, _, symbols) = selected_json(
        app.clone(),
        "GET",
        &format!("{prefix}/symbols?q=old_step"),
        Value::Null,
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::OK);
    let seed = symbols["items"][0]["id"].as_str().unwrap().to_owned();
    let pin = format!(
        "indexGeneration={}&indexRevision={}",
        old_revision["indexGeneration"].as_str().unwrap(),
        old_revision["indexRevision"].as_u64().unwrap()
    );
    let view = serde_json::json!({"id":"saved","title":"Original","query":{"seed":seed},"pins":{},"hidden":[]});
    let note = serde_json::json!({"id":"note","nodeId":seed,"body":"Original"});
    for (path, value) in [("views/saved", &view), ("annotations/note", &note)] {
        let (code, _, body) = selected_json(
            app.clone(),
            "PUT",
            &format!("{prefix}/{path}?{pin}"),
            value.clone(),
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::OK, "{body}");
    }
    let release_at = std::time::Instant::now() + Duration::from_secs(30 * 60);
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let released = {
                let mut registry = registry.lock().await;
                registry.advance(release_at).unwrap()
            };
            if !released.released.is_empty() {
                assert_eq!(released.released, vec![identity.root_key.clone()]);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("selected checkout did not become releasable after worker quiescence");
    fs::write(root.join("core.js"), "function fresh_step() {}\n").unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    let runtime = {
        let mut registry = registry.lock().await;
        registry
            .browser_request_at(&identity, std::time::Instant::now())
            .unwrap();
        let runtime = registry.activate(&identity.root_key).unwrap();
        runtime.set_pre_h_hook_for_tests(Arc::new(move || {
            entered_tx.send(()).unwrap();
            resume_rx.lock().unwrap().recv().unwrap();
        }));
        runtime
    };
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while entered_rx.try_recv().is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for (path, value) in [("views/saved", &view), ("annotations/note", &note)] {
        let mut changed = value.clone();
        if path.starts_with("views/") {
            changed["title"] = serde_json::json!("Forbidden pre-H");
        } else {
            changed["body"] = serde_json::json!("Forbidden pre-H");
        }
        let (code, headers, body) = selected_json(
            app.clone(),
            "PUT",
            &format!("{prefix}/{path}?{pin}"),
            changed,
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"]["code"], "index_not_ready");
        assert_eq!(headers["X-Trellis-Catching-Up"], "true");
        let (code, _, body) = selected_json(
            app.clone(),
            "DELETE",
            &format!("{prefix}/{path}"),
            Value::Null,
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"]["code"], "index_not_ready");
    }
    let records = trellis::store::topology::DurableRecords::new(&roots, &identity);
    assert_eq!(
        records.view_record("saved").unwrap().unwrap().title,
        "Original"
    );
    assert_eq!(records.annotation_records().unwrap()[0].body, "Original");
    let (code, headers, prior) = status(app.clone()).await;
    assert_eq!(code, axum::http::StatusCode::OK, "{prior}");
    assert_eq!(headers["X-Trellis-Catching-Up"], "true");
    assert_eq!(prior["catchingUp"], true);
    assert_eq!(prior["revision"], old_revision);
    let file_path = format!("/api/checkouts/{}/files", identity.root_key);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&file_path)
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(response.headers()["X-Trellis-Catching-Up"], "true");
    let prior_files: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(prior_files["revision"], old_revision);
    assert!(prior_files["items"].to_string().contains("core.js"));
    let pinned = format!(
        "{file_path}?indexGeneration={}&indexRevision={}",
        old_revision["indexGeneration"].as_str().unwrap(),
        old_revision["indexRevision"].as_u64().unwrap()
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(pinned)
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(response.headers()["X-Trellis-Catching-Up"], "true");
    let mismatch = format!(
        "{file_path}?indexGeneration={}&indexRevision={}",
        old_revision["indexGeneration"].as_str().unwrap(),
        old_revision["indexRevision"].as_u64().unwrap() + 1
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(mismatch)
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(response.headers()["X-Trellis-Catching-Up"], "true");

    let (code, headers, dependencies) = selected_json(
        app.clone(),
        "GET",
        &format!("{prefix}/dependencies"),
        Value::Null,
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::OK, "{dependencies}");
    assert_eq!(headers["X-Trellis-Catching-Up"], "true");
    assert_eq!(dependencies["workspaceRevision"], old_revision);
    for route in [
        "dependencies/refresh",
        "questions/foreign/jev-run",
        "questions/foreign/acp-answer",
    ] {
        let (code, headers, error) = selected_json(
            app.clone(),
            "POST",
            &format!("{prefix}/{route}"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(
            code,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "{route}: {error}"
        );
        assert_eq!(
            error["error"]["code"], "index_not_ready",
            "{route}: {error}"
        );
        assert_eq!(headers["X-Trellis-Catching-Up"], "true");
    }
    resume_tx.send(()).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if !runtime.catching_up() {
                let (code, headers, current) = status(app.clone()).await;
                if code == axum::http::StatusCode::OK && current["catchingUp"] == false {
                    assert_eq!(headers["X-Trellis-Catching-Up"], "false");
                    assert_ne!(current["revision"], old_revision);
                    let response = app
                        .clone()
                        .oneshot(
                            Request::builder()
                                .uri(&file_path)
                                .header("host", "127.0.0.1:7331")
                                .header("authorization", format!("Bearer {TOKEN}"))
                                .body(Body::empty())
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    // A later watcher wake can begin between the status and
                    // files requests. Require an eventual matching, fresh pair
                    // rather than treating two responses as one atomic read.
                    let code = response.status();
                    let fresh = response.headers()["X-Trellis-Catching-Up"] == "false";
                    let files: Value = serde_json::from_slice(
                        &to_bytes(response.into_body(), 1024 * 1024).await.unwrap(),
                    )
                    .unwrap();
                    if code == axum::http::StatusCode::OK {
                        if fresh && files["revision"] == current["revision"] {
                            break;
                        }
                    } else {
                        assert_eq!(code, axum::http::StatusCode::SERVICE_UNAVAILABLE, "{files}");
                        assert_eq!(files["error"]["code"], "index_not_ready", "{files}");
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn provider_routes_use_only_the_selected_checkout() {
    use std::sync::Arc;
    use trellis::{
        daemon::registry::{CheckoutOptions, CheckoutRegistry},
        http::ProvisionedBrowser,
        store::topology::TopologyRoots,
    };
    let temp = TempDir::new().unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    for root in [&a, &b] {
        fs::create_dir(root).unwrap();
        fs::write(
            root.join("core.js"),
            "function selected_root() { return 1; }\n",
        )
        .unwrap();
        fs::create_dir(root.join("library")).unwrap();
    }
    let external_library = temp.path().join("dependency-library");
    fs::create_dir_all(external_library.join("std/src")).unwrap();
    fs::create_dir(a.join("src")).unwrap();
    fs::create_dir(temp.path().join("empty-cargo")).unwrap();
    fs::write(
        a.join("Cargo.toml"),
        "[package]\nname = 'selected_a'\nversion = '0.1.0'\nedition = '2021'\n",
    )
    .unwrap();
    fs::write(a.join("src/lib.rs"), "pub fn workspace_only() {}\n").unwrap();
    fs::write(
        external_library.join("std/Cargo.toml"),
        "[package]\nname = 'std'\nversion = '0.0.0'\n",
    )
    .unwrap();
    let library_text =
        "// café\npub struct LibraryType;\nimpl LibraryType { pub fn method(&self) {} }\n";
    fs::write(external_library.join("std/src/lib.rs"), library_text).unwrap();
    fs::write(a.join("library/source.rs"), "pub fn from_a() {}\n").unwrap();
    fs::write(b.join("library/source.rs"), "pub fn from_b() {}\n").unwrap();
    let a_id = WorkspaceIdentity::discover(Some(&a), &a).unwrap();
    let b_id = WorkspaceIdentity::discover(Some(&b), &b).unwrap();
    let runner = temp.path().join("acp-runner");
    fs::write(
        &runner,
        format!(
            "#!/bin/sh\ncat > /dev/null\ncat '{}'\n",
            temp.path().join("acp-response.json").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
    let mut registry = CheckoutRegistry::with_roots(roots);
    for (identity, root) in [(&a_id, &a), (&b_id, &b)] {
        registry.register(identity, CheckoutOptions(serde_json::json!({
            "rustSourceRoots": if root == &a {
                serde_json::json!([["library",root.join("library")],["a_only",root.join("library")]])
            } else {
                serde_json::json!([["library",root.join("library")]])
            },
            "acpRunner": if root == &a { Some(runner.clone()) } else { None },
            "acpStateDir": if root == &a { Some(temp.path().join("acp-state")) } else { None },
            "acpMaxAttempts": if root == &a { Some(2) } else { None },
            "rustLibrary": if root == &a { Some(external_library.clone()) } else { None },
            "cargoHome": temp.path().join("empty-cargo")
        }))).unwrap();
    }
    let app = ProvisionedBrowser::new(
        Arc::new(tokio::sync::Mutex::new(registry)),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap()
    .router();
    let a_prefix = format!("/api/checkouts/{}", a_id.root_key);
    let b_prefix = format!("/api/checkouts/{}", b_id.root_key);
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (a_code, _, _) = selected_json(
                app.clone(),
                "GET",
                &format!("{a_prefix}/status"),
                Value::Null,
            )
            .await;
            let (b_code, _, _) = selected_json(
                app.clone(),
                "GET",
                &format!("{b_prefix}/status"),
                Value::Null,
            )
            .await;
            if a_code.is_success() && b_code.is_success() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    for (prefix, identity, expected) in [(&a_prefix, &a_id, "from_a"), (&b_prefix, &b_id, "from_b")]
    {
        for (suffix, field) in [("jev/status", "budget"), ("acp/status", "status")] {
            let (code, headers, body) = selected_json(
                app.clone(),
                "GET",
                &format!("{prefix}/{suffix}"),
                Value::Null,
            )
            .await;
            assert_eq!(code, axum::http::StatusCode::OK, "{body}");
            if suffix == "acp/status" && prefix == &a_prefix {
                assert_eq!(body["enabled"], true);
                assert!(!body[field].is_null());
            } else {
                assert_eq!(body["enabled"], false);
                assert!(body[field].is_null());
            }
            assert_eq!(
                headers["X-Trellis-Workspace"],
                identity.root.to_str().unwrap()
            );
            assert_eq!(headers["cache-control"], "no-store");
        }
        let (code, _, roots) = selected_json(
            app.clone(),
            "GET",
            &format!("{prefix}/rust-sources"),
            Value::Null,
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::OK);
        assert_eq!(roots["roots"][0]["label"], "library");
        let (code, _, tree) = selected_json(
            app.clone(),
            "GET",
            &format!("{prefix}/rust-sources/tree?root=library"),
            Value::Null,
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::OK, "{tree}");
        assert_eq!(tree["items"][0]["path"], "source.rs");
        let (code, _, file) = selected_json(
            app.clone(),
            "GET",
            &format!("{prefix}/rust-sources/file?root=library&path=source.rs"),
            Value::Null,
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::OK, "{file}");
        assert!(file.to_string().contains(expected), "{file}");
        if prefix == &b_prefix {
            let (code, _, error) = selected_json(
                app.clone(),
                "GET",
                &format!("{prefix}/rust-sources/file?root=a_only&path=source.rs"),
                Value::Null,
            )
            .await;
            assert_eq!(code, axum::http::StatusCode::NOT_FOUND, "{error}");
        }
        let (code, _, deps) = selected_json(
            app.clone(),
            "GET",
            &format!("{prefix}/dependencies"),
            Value::Null,
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::OK, "{deps}");
        let (code, _, error) = selected_json(
            app.clone(),
            "GET",
            &format!("{prefix}/dependencies/symbols?catalogId=foreign"),
            Value::Null,
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::CONFLICT, "{error}");
        assert_eq!(error["error"]["code"], "stale_catalog");
        let (code, _, error) = selected_json(
            app.clone(),
            "GET",
            &format!("{prefix}/dependencies/source?catalogId=foreign&sourceRef=foreign"),
            Value::Null,
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::CONFLICT, "{error}");
        let (code, _, error) = selected_json(
            app.clone(),
            "POST",
            &format!("{prefix}/questions/foreign/acp-answer"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::NOT_FOUND, "{error}");
        let (code, _, error) = selected_json(
            app.clone(),
            "POST",
            &format!("{prefix}/questions/foreign/jev-run"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::NOT_FOUND, "{error}");
    }
    let (code, headers, initial_refresh) = selected_json(
        app.clone(),
        "POST",
        &format!("{a_prefix}/dependencies/refresh"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::ACCEPTED, "{initial_refresh}");
    assert_eq!(headers["X-Trellis-Workspace"], a_id.root.to_str().unwrap());
    let a_catalog = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (code, _, status) = selected_json(
                app.clone(),
                "GET",
                &format!("{a_prefix}/dependencies"),
                Value::Null,
            )
            .await;
            assert_eq!(code, axum::http::StatusCode::OK, "{status}");
            if status["state"] == "ready" && status["catalogId"].is_string() {
                break status;
            }
            if status["state"] == "failed" {
                panic!("A dependency catalog failed: {status}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("selected A catalog did not become ready");
    let catalog_id = a_catalog["catalogId"].as_str().unwrap();
    assert!(
        a_catalog["symbolCount"].as_u64().unwrap() >= 1,
        "{a_catalog}"
    );
    let symbol_url =
        format!("{a_prefix}/dependencies/symbols?catalogId={catalog_id}&q=LibraryType");
    let (code, headers, symbols) =
        selected_json(app.clone(), "GET", &symbol_url, Value::Null).await;
    assert_eq!(code, axum::http::StatusCode::OK, "{symbols}");
    assert_eq!(headers["X-Trellis-Workspace"], a_id.root.to_str().unwrap());
    assert!(headers.contains_key("X-Trellis-Catching-Up"));
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(symbols["catalogId"], catalog_id);
    let library_symbol = symbols["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "LibraryType")
        .expect("selected A library symbol");
    let source_ref = library_symbol["sourceRef"].as_str().unwrap();
    let source_url =
        format!("{a_prefix}/dependencies/source?catalogId={catalog_id}&sourceRef={source_ref}");
    let (code, headers, source) = selected_json(app.clone(), "GET", &source_url, Value::Null).await;
    assert_eq!(code, axum::http::StatusCode::OK, "{source}");
    assert_eq!(headers["X-Trellis-Workspace"], a_id.root.to_str().unwrap());
    assert!(headers.contains_key("X-Trellis-Catching-Up"));
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(source["rootLabel"], "std");
    assert_eq!(source["file"]["text"], library_text);
    assert_eq!(source["id"], source_ref);
    for route in [
        format!("{b_prefix}/dependencies/symbols?catalogId={catalog_id}&q=LibraryType"),
        format!("{b_prefix}/dependencies/source?catalogId={catalog_id}&sourceRef={source_ref}"),
    ] {
        let (code, headers, error) = selected_json(app.clone(), "GET", &route, Value::Null).await;
        assert_eq!(code, axum::http::StatusCode::CONFLICT, "{error}");
        assert_eq!(error["error"]["code"], "stale_catalog");
        assert_eq!(headers["X-Trellis-Workspace"], b_id.root.to_str().unwrap());
    }
    let (code, headers, b_refresh) = selected_json(
        app.clone(),
        "POST",
        &format!("{b_prefix}/dependencies/refresh"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::ACCEPTED, "{b_refresh}");
    assert_eq!(headers["X-Trellis-Workspace"], b_id.root.to_str().unwrap());
    let b_catalog_before = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (code, _, status) = selected_json(
                app.clone(),
                "GET",
                &format!("{b_prefix}/dependencies"),
                Value::Null,
            )
            .await;
            assert_eq!(code, axum::http::StatusCode::OK, "{status}");
            if status["state"] != "loading" {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("selected B catalog did not settle");
    let (code, headers, refresh) = selected_json(
        app.clone(),
        "POST",
        &format!("{a_prefix}/dependencies/refresh"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::ACCEPTED, "{refresh}");
    assert_eq!(refresh["state"], "loading");
    assert_eq!(headers["X-Trellis-Workspace"], a_id.root.to_str().unwrap());
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (code, _, status) = selected_json(
                app.clone(),
                "GET",
                &format!("{a_prefix}/dependencies"),
                Value::Null,
            )
            .await;
            assert_eq!(code, axum::http::StatusCode::OK, "{status}");
            if status["state"] == "ready" {
                break;
            }
            if status["state"] == "failed" {
                panic!("selected A refresh failed: {status}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("selected A refresh did not finish");
    let (code, headers, b_catalog_after) = selected_json(
        app.clone(),
        "GET",
        &format!("{b_prefix}/dependencies"),
        Value::Null,
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::OK, "{b_catalog_after}");
    assert_eq!(headers["X-Trellis-Workspace"], b_id.root.to_str().unwrap());
    assert_eq!(
        b_catalog_after, b_catalog_before,
        "A refresh must not touch B catalog"
    );
    let (code, _, symbols) = selected_json(
        app.clone(),
        "GET",
        &format!("{a_prefix}/symbols?q=selected_root"),
        Value::Null,
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::OK, "{symbols}");
    let (code, _, status) = selected_json(
        app.clone(),
        "GET",
        &format!("{a_prefix}/status"),
        Value::Null,
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::OK);
    let seed = symbols["items"][0]["id"].as_str().unwrap();
    let (code, _, preview) = selected_json(app.clone(), "POST", &format!("{a_prefix}/questions/preview"),
        serde_json::json!({"seed":seed,"question":"Describe the function","expectedRevision":status["revision"]})).await;
    assert_eq!(code, axum::http::StatusCode::OK, "{preview}");
    let packet = preview["packet"]["packetId"].as_str().unwrap();
    for action in ["acp-answer", "jev-run"] {
        let (code, headers, error) = selected_json(
            app.clone(),
            "POST",
            &format!("{b_prefix}/questions/{packet}/{action}"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::NOT_FOUND, "{error}");
        assert_eq!(headers["X-Trellis-Workspace"], b_id.root.to_str().unwrap());
    }
    let response = serde_json::json!({"answer":{"packetId":packet,"summary":[{"text":"Returns 1.",
        "citations":[{"path":"core.js","startLine":1,"endLine":1,
            "quote":"function selected_root() { return 1; }"}]}],
        "branches":[],"limitations":[]},"estimatedUsd":0.01});
    fs::write(temp.path().join("acp-response.json"), response.to_string()).unwrap();
    let (code, _, answer) = selected_json(
        app.clone(),
        "POST",
        &format!("{a_prefix}/questions/{packet}/acp-answer"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::OK, "{answer}");
    assert_eq!(answer["packetId"], packet);
    assert_eq!(answer["source"], "liveAcp");
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    for (host, authorization, origin, expected) in [
        (
            "127.0.0.1:7331",
            "",
            "",
            axum::http::StatusCode::UNAUTHORIZED,
        ),
        (
            "example.invalid",
            "Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "",
            axum::http::StatusCode::FORBIDDEN,
        ),
        (
            "127.0.0.1:7331",
            "Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "https://example.invalid",
            axum::http::StatusCode::FORBIDDEN,
        ),
    ] {
        let response = app
            .clone()
            .oneshot({
                let mut request = Request::builder()
                    .uri(format!("{b_prefix}/jev/status"))
                    .header("host", host);
                if !authorization.is_empty() {
                    request = request.header("authorization", authorization);
                }
                if !origin.is_empty() {
                    request = request.header("origin", origin);
                }
                request.body(Body::empty()).unwrap()
            })
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(!response.headers().contains_key("X-Trellis-Workspace"));
    }
    for suffix in ["jev/status", "acp/status", "dependencies", "rust-sources"] {
        let (code, headers, _) =
            selected_json(app.clone(), "GET", &format!("/api/{suffix}"), Value::Null).await;
        assert_eq!(code, axum::http::StatusCode::NOT_FOUND);
        assert!(!headers.contains_key("X-Trellis-Workspace"));
    }
}

#[tokio::test]
async fn production_browser_rejects_every_selector_free_checkout_route_without_attachment() {
    use std::sync::Arc;
    use trellis::{
        daemon::registry::{CheckoutOptions, CheckoutRegistry},
        http::ProvisionedBrowser,
        store::topology::TopologyRoots,
    };

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    // The same inventory must hold with exactly one registered checkout: no
    // implicit default is allowed merely because selection is unambiguous.
    let old_routes = [
        ("GET", "/api/status"),
        ("GET", "/api/tree"),
        ("GET", "/api/files"),
        ("GET", "/api/methods"),
        ("GET", "/api/classes"),
        ("GET", "/api/symbols"),
        ("GET", "/api/symbol"),
        ("GET", "/api/source"),
        ("GET", "/api/jev/status"),
        ("GET", "/api/acp/status"),
        ("GET", "/api/dependencies"),
        ("GET", "/api/dependencies/symbols"),
        ("GET", "/api/dependencies/source"),
        ("GET", "/api/rust-sources"),
        ("GET", "/api/rust-sources/tree"),
        ("GET", "/api/rust-sources/file"),
        ("GET", "/api/views"),
        ("GET", "/api/views/saved"),
        ("GET", "/api/annotations"),
        ("GET", "/api/questions/packet/jev-request"),
        ("GET", "/api/jobs/current"),
        ("GET", "/api/jobs/job"),
        ("POST", "/api/index"),
        ("POST", "/api/jobs/job/cancel"),
        ("POST", "/api/dependencies/refresh"),
        ("POST", "/api/sequence"),
        ("POST", "/api/class-diagram"),
        ("POST", "/api/navigation"),
        ("POST", "/api/query"),
        ("POST", "/api/questions/preview"),
        ("POST", "/api/questions/packet/jev-response"),
        ("POST", "/api/questions/packet/selection"),
        ("POST", "/api/questions/packet/acp-answer"),
        ("POST", "/api/questions/packet/jev-run"),
        ("PUT", "/api/views/saved"),
        ("DELETE", "/api/views/saved"),
        ("PUT", "/api/annotations/note"),
        ("DELETE", "/api/annotations/note"),
        ("GET", "/api/not-a-route"),
        ("POST", "/api/not-a-route"),
    ];
    for count in [1, 2] {
        let temp = TempDir::new().unwrap();
        let roots =
            TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
        let mut registry = CheckoutRegistry::with_roots(roots);
        let mut identities = Vec::new();
        for name in ["first", "second"].into_iter().take(count) {
            let root = temp.path().join(name);
            fs::create_dir(&root).unwrap();
            fs::write(root.join("core.js"), "function selected() { return 1; }\n").unwrap();
            let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
            registry
                .register(&identity, CheckoutOptions(serde_json::json!({})))
                .unwrap();
            identities.push(identity);
        }
        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let app = ProvisionedBrowser::new(
            registry.clone(),
            TOKEN.into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap()
        .router();
        let original = registry.lock().await.browser_checkouts();
        for (method, path) in old_routes {
            let (code, headers, body) =
                selected_json(app.clone(), method, path, serde_json::json!({})).await;
            assert_eq!(
                code,
                axum::http::StatusCode::NOT_FOUND,
                "{count} roots: {method} {path}: {body}"
            );
            assert_eq!(headers["cache-control"], "no-store", "{method} {path}");
            assert!(
                !headers.contains_key("X-Trellis-Workspace"),
                "{method} {path}"
            );
            assert!(
                !headers.contains_key("X-Trellis-Catching-Up"),
                "{method} {path}"
            );
            let state = registry.lock().await;
            assert_eq!(state.active_count(), 0, "{method} {path} attached a client");
            assert_eq!(
                state.browser_checkouts(),
                original,
                "{method} {path} changed browser state"
            );
            for identity in &identities {
                assert!(
                    state.runtime(&identity.root_key).is_none(),
                    "{method} {path} activated a runtime"
                );
            }
        }
        for path in ["/healthz", "/api/checkouts", "/api/daemon/status"] {
            let (code, headers, _) = selected_json(app.clone(), "GET", path, Value::Null).await;
            assert_eq!(code, axum::http::StatusCode::OK, "{path}");
            assert_eq!(headers["cache-control"], "no-store");
            assert!(!headers.contains_key("X-Trellis-Workspace"));
            assert_eq!(
                registry.lock().await.active_count(),
                0,
                "{path} attached a client"
            );
        }
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        for path in ["/", "/app.js"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("host", "127.0.0.1:7331")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::OK, "{path}");
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert!(!response.headers().contains_key("X-Trellis-Workspace"));
            assert_eq!(registry.lock().await.active_count(), 0);
        }
        for (host, auth, origin, expected) in [
            (
                "127.0.0.1:7331",
                None,
                None,
                axum::http::StatusCode::UNAUTHORIZED,
            ),
            (
                "127.0.0.1:7331",
                Some("Bearer wrong"),
                None,
                axum::http::StatusCode::UNAUTHORIZED,
            ),
            (
                "example.invalid",
                Some("Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
                None,
                axum::http::StatusCode::FORBIDDEN,
            ),
            (
                "127.0.0.1:7331",
                Some("Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
                Some("https://example.invalid"),
                axum::http::StatusCode::FORBIDDEN,
            ),
        ] {
            for path in ["/api/status", "/api/index"] {
                let mut request = Request::builder()
                    .method(if path == "/api/index" { "POST" } else { "GET" })
                    .uri(path)
                    .header("host", host);
                if let Some(auth) = auth {
                    request = request.header("authorization", auth);
                }
                if let Some(origin) = origin {
                    request = request.header("origin", origin);
                }
                let response = app
                    .clone()
                    .oneshot(request.body(Body::from("{}")).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), expected, "{path} {host}");
                assert_eq!(response.headers()["cache-control"], "no-store");
                assert!(!response.headers().contains_key("X-Trellis-Workspace"));
                assert!(!response.headers().contains_key("X-Trellis-Catching-Up"));
            }
        }
        assert_eq!(registry.lock().await.browser_checkouts(), original);
        let first = &identities[0];
        let prefix = format!("/api/checkouts/{}", first.root_key);
        let (code, headers, status) =
            tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
                loop {
                    let result =
                        selected_json(app.clone(), "GET", &format!("{prefix}/status"), Value::Null)
                            .await;
                    if result.0 == axum::http::StatusCode::OK {
                        break result;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        assert_eq!(code, axum::http::StatusCode::OK, "{status}");
        assert_eq!(headers["X-Trellis-Workspace"], first.root.to_str().unwrap());
        assert_eq!(
            headers["X-Trellis-Catching-Up"],
            status["catchingUp"].as_bool().unwrap().to_string()
        );
        assert_eq!(status["workspaceRoot"], first.root.to_str().unwrap());
        let (code, headers, files) =
            selected_json(app.clone(), "GET", &format!("{prefix}/files"), Value::Null).await;
        assert_eq!(code, axum::http::StatusCode::OK, "{files}");
        assert_eq!(headers["X-Trellis-Workspace"], first.root.to_str().unwrap());
        assert!(headers.contains_key("X-Trellis-Catching-Up"));
        assert!(
            files["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file["path"] == "core.js"),
            "{files}"
        );
        let (code, headers, provider) = selected_json(
            app.clone(),
            "GET",
            &format!("{prefix}/jev/status"),
            Value::Null,
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::OK, "{provider}");
        assert_eq!(headers["X-Trellis-Workspace"], first.root.to_str().unwrap());
        assert!(headers.contains_key("X-Trellis-Catching-Up"));
        let (code, headers, mutation) = selected_json(
            app,
            "POST",
            &format!("{prefix}/index"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(code, axum::http::StatusCode::ACCEPTED, "{mutation}");
        assert!(mutation["id"].is_string(), "{mutation}");
        assert_eq!(headers["X-Trellis-Workspace"], first.root.to_str().unwrap());
        assert!(headers.contains_key("X-Trellis-Catching-Up"));
        assert_eq!(registry.lock().await.active_count(), 1);
    }
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn virgin_queue_use_lock_contention_retries_h_before_claiming_fifo() {
    use std::sync::Arc;
    use trellis::{
        daemon::registry::{CheckoutOptions, CheckoutRegistry},
        http::ProvisionedBrowser,
        indexer::IndexOptions,
        store::{Store, topology::TopologyRoots},
    };
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("core.js"), "function stable() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    // Store::open creates only the virgin revision-0 index, not the queue.
    let store = Store::open(roots.clone(), identity.verified_clone_for_tests().unwrap()).unwrap();
    let queue_path = roots.requests_db(&identity);
    assert!(!queue_path.exists());
    let use_lock = roots.index_use(&identity).unwrap();
    let mut registry = CheckoutRegistry::with_roots(roots.clone());
    registry
        .register(&identity, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    let registry = Arc::new(tokio::sync::Mutex::new(registry));
    let app = ProvisionedBrowser::new(
        registry.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap()
    .router();
    {
        let mut checked = registry.lock().await;
        checked
            .browser_request_at(&identity, std::time::Instant::now())
            .unwrap();
        // Bootstrap now precedes publication. An outstanding SH prevents its
        // EX lock, so activation must fail without exposing an H runtime or
        // publishing a queue that could accept a FIFO request before H.
        let error = match checked.activate(&identity.root_key) {
            Ok(_) => panic!("virgin queue activation must not publish under SH contention"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "storage_busy", "{error:#}");
        assert!(checked.runtime(&identity.root_key).is_none());
    }
    let prefix = format!("/api/checkouts/{}", identity.root_key);
    let (code, _, blocked) =
        selected_json(app.clone(), "GET", &format!("{prefix}/status"), Value::Null).await;
    assert_eq!(code, axum::http::StatusCode::CONFLICT, "{blocked}");
    assert_eq!(blocked["error"]["code"], "storage_busy");
    assert!(registry.lock().await.runtime(&identity.root_key).is_none());
    assert!(!queue_path.exists());
    drop(use_lock);
    let runtime = {
        let mut checked = registry.lock().await;
        checked.activate(&identity.root_key).unwrap()
    };
    assert!(registry.lock().await.runtime(&identity.root_key).is_some());
    // Activation returns only after the staged queue is complete and readable
    // as v1. The normal Store queue read also validates root identity and
    // SQLite integrity; a bare existence check would miss a half-built queue.
    assert!(queue_path.exists());
    assert!(store.earliest_unfinished_request().unwrap().is_none());
    let queued = store
        .enqueue_request(&IndexOptions::new(root), None)
        .unwrap();
    assert_eq!(queued.state, "queued");
    let deadline = tokio::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    let terminal = Arc::new(tokio::sync::Notify::new());
    runtime
        .notify_on_terminal_request_for_tests(&queued.id, terminal.clone())
        .unwrap();
    let mut last_status = None;
    let ready = tokio::time::timeout_at(deadline, async {
        // The request may have committed between enqueue and registration.
        // Otherwise wait for this ID's durable terminal COMMIT without a
        // stream of protected status/queue SH reads competing with H's EX.
        let mut finished = loop {
            match store.request_by_id(&queued.id) {
                Ok(Some(row)) => break row,
                Err(error) if trellis::store::cli_read_retryable_contention(&error) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                other => panic!("accepted virgin FIFO lookup failed: {other:?}"),
            }
        };
        if finished.finished_at.is_none() {
            terminal.notified().await;
            finished = loop {
                match store.request_by_id(&queued.id) {
                    Ok(Some(row)) => break row,
                    Err(error) if trellis::store::cli_read_retryable_contention(&error) => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    other => panic!("terminal virgin FIFO lookup failed: {other:?}"),
                }
            };
        }
        assert_eq!(finished.state, "done", "{finished:?}");
        assert!(finished.finished_at.is_some());
        assert_eq!(finished.id, queued.id);
        assert_eq!(finished.seq, queued.seq);
        assert!(
            finished.revision.unwrap().index_revision >= 2,
            "FIFO request must commit after H"
        );
        loop {
            let (code, _, status) =
                selected_json(app.clone(), "GET", &format!("{prefix}/status"), Value::Null).await;
            if code == axum::http::StatusCode::OK && status["catchingUp"] == false {
                assert!(
                    status["revision"]["indexRevision"].as_u64().unwrap() >= 2,
                    "mandatory H must publish before FIFO completion: {status}"
                );
                break;
            }
            last_status = Some((code, status));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    if ready.is_err() {
        let at_deadline = store.request_by_id(&queued.id).map(|row| {
            row.map(|row| {
                (
                    row.state,
                    row.error_code,
                    row.finished_at.is_some(),
                    row.revision,
                )
            })
        });
        let head_at_deadline = store.status().map(|status| status.revision.index_revision);
        let owner_valid = runtime
            .browser_scheduler()
            .ok()
            .and_then(|scheduler| scheduler.checkout_serving_owner())
            .is_some();
        let error = runtime.reconciliation_error();
        let retry_waiting = runtime.retry_waiting_for_tests();
        let catching_up = runtime.catching_up();
        // Failure-only: a late completion remains RED, but distinguishes a
        // stuck worker from a reader-induced delay after polling stops.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let after_quiet = store.request_by_id(&queued.id).map(|row| {
            row.map(|row| {
                (
                    row.state,
                    row.error_code,
                    row.finished_at.is_some(),
                    row.revision,
                )
            })
        });
        panic!(
            "H and queued work did not become ready after SH release: last_status={last_status:?}, at_deadline={at_deadline:?}, head_at_deadline={head_at_deadline:?}, owner_valid={owner_valid}, error={error:?}, retry_waiting={retry_waiting}, catching_up={catching_up}, after_quiet={after_quiet:?}"
        );
    }
    assert!(!runtime.catching_up());
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn selected_reads_validated_predecessor_through_owner_handoff() {
    use std::sync::{Arc, Mutex, mpsc};
    use trellis::{
        daemon::registry::{CheckoutOptions, CheckoutRegistry},
        http::ProvisionedBrowser,
        index_coordinator::establish_serving_session,
        indexer::IndexOptions,
        store::{Store, topology::TopologyRoots},
    };
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("core.js"), "function oldHead() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let published = Store::open(
        roots.clone(),
        WorkspaceIdentity::discover(Some(&root), &root).unwrap(),
    )
    .unwrap();
    let options = IndexOptions::new(root.clone());
    let old_owner = establish_serving_session(
        &published,
        Some(&options),
        &Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    let prior = serde_json::to_value(published.status().unwrap().revision).unwrap();
    fs::write(root.join("core.js"), "function newHead() {}\n").unwrap();
    let queued = published.enqueue_request(&options, None).unwrap();
    drop(old_owner);
    let mut registry = CheckoutRegistry::with_roots(roots);
    registry
        .register(&identity, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    let registry = Arc::new(tokio::sync::Mutex::new(registry));
    let browser = ProvisionedBrowser::new(
        registry.clone(),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap()
    .router();
    let runtime = {
        let mut checked = registry.lock().await;
        checked
            .browser_request_at(&identity, std::time::Instant::now())
            .unwrap();
        checked.activate(&identity.root_key).unwrap()
    };
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = Mutex::new(release_rx);
    // Activation schedules reconciliation on Tokio. Set the hook before the
    // first await so this runtime pauses before its mandatory local H.
    runtime.set_pre_h_hook_for_tests(Arc::new(move || {
        entered_tx.send(()).unwrap();
        release_rx.lock().unwrap().recv().unwrap();
    }));
    let prefix = format!("/api/checkouts/{}", identity.root_key);
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while entered_rx.try_recv().is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon-local H did not reach pre-publication boundary");
    let (code, headers, files) = selected_json(
        browser.clone(),
        "GET",
        &format!("{prefix}/files"),
        Value::Null,
    )
    .await;
    assert_eq!(code, axum::http::StatusCode::OK, "{files}");
    assert_eq!(headers["X-Trellis-Catching-Up"], "true");
    assert_eq!(files["revision"], prior);
    assert!(files.to_string().contains("core.js"));
    let (mutation_code, _, mutation_body) = selected_json(
        browser.clone(),
        "POST",
        &format!("{prefix}/dependencies/refresh"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        mutation_code,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "{mutation_body}"
    );
    assert_eq!(mutation_body["error"]["code"], "index_not_ready");
    let (status_code, status_headers, status) = selected_json(
        browser.clone(),
        "GET",
        &format!("{prefix}/status"),
        Value::Null,
    )
    .await;
    assert_eq!(status_code, axum::http::StatusCode::OK, "{status}");
    assert_eq!(status["revision"], prior);
    assert_eq!(status_headers["X-Trellis-Catching-Up"], "true");
    assert_eq!(
        published.request_by_id(&queued.id).unwrap().unwrap().state,
        "queued",
        "pre-H selected reads cannot claim accepted FIFO work"
    );
    release_tx.send(()).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (code, _, status) = selected_json(
                browser.clone(),
                "GET",
                &format!("{prefix}/status"),
                Value::Null,
            )
            .await;
            if code == axum::http::StatusCode::OK && status["catchingUp"] == false {
                assert!(
                    status["revision"]["indexRevision"].as_u64().unwrap()
                        >= prior["indexRevision"].as_u64().unwrap() + 2,
                    "mandatory H must publish before queued work"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("local H and accepted FIFO head did not finish");
    let finished = published.request_by_id(&queued.id).unwrap().unwrap();
    assert_eq!(
        (finished.id.as_str(), finished.seq, finished.state.as_str()),
        (queued.id.as_str(), queued.seq, "done")
    );
}
