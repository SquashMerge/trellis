use std::{fs, process::Command, time::Duration};
use trellis::{
    build_info,
    daemon::registry::CheckoutRegistry,
    store::topology::{TopologyRoots, WorkspaceIdentity},
};

#[test]
fn executable_version_reports_compiled_identity_without_starting_a_daemon() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_trellis"))
        .arg("--version")
        .env("HOME", home.path())
        // Runtime environment must not rewrite the identity of a built binary.
        .env("TRELLIS_BUILD_COMMIT", "not-the-compiled-commit")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("trellis {}\n", build_info::version())
    );
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    let build = build_info::current();
    assert_eq!(build.version, env!("CARGO_PKG_VERSION"));
    assert!(build.target.starts_with(std::env::consts::ARCH));
    let target_os = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        std::env::consts::OS
    };
    assert!(build.target.contains(target_os));
    assert_eq!(build.features.contains(&"metal"), cfg!(feature = "metal"));
    assert_eq!(
        build.features.contains(&"test-support"),
        cfg!(feature = "test-support")
    );
    assert!(build.features.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(
        build.embedding_capabilities.contains(&"metal"),
        cfg!(feature = "metal")
    );
    assert!(build.embedding_capabilities.contains(&"cpu"));
    assert_eq!(
        build.commit,
        option_env!("TRELLIS_BUILD_COMMIT").unwrap_or("dev")
    );
    assert!(build.toolchain.starts_with("rustc "));
}

#[tokio::test]
async fn daemon_cli_status_adds_binary_identity_without_changing_snapshot_status() {
    let base = tempfile::tempdir().unwrap();
    let checkout = base.path().join("work");
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("sample.js"), "function sample() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&checkout), &checkout)
        .unwrap()
        .attach_marker()
        .unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(roots);
    registry.attach_launch(1, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    let status = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok(status) = runtime.cli_status() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("startup failed: {:?}", runtime.reconciliation_error()));
    assert_eq!(
        status["build"],
        serde_json::to_value(build_info::current()).unwrap()
    );
    assert!(!status["revision"].is_null());
    assert_eq!(
        status["searchIndex"],
        serde_json::json!({"backend":null,"unavailableReason":null})
    );
    assert_eq!(
        status["workspaceRoot"],
        fs::canonicalize(checkout)
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );

    let (response, _) = runtime.evidence_response().unwrap();
    let snapshot_status = response.status().unwrap();
    assert!(
        serde_json::to_value(response.finish(snapshot_status).unwrap())
            .unwrap()
            .get("build")
            .is_none()
    );
    drop(response);
    registry.disconnect(1);
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while !registry.release(&identity.root_key).unwrap() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("checkout did not release after startup and watcher quiescence");
}
