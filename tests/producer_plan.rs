//! `trellis producer plan` and `producer status`: every inspectable field, a
//! stable hash, the refusal of restricted rust-analyzer, and no execution.
#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{fs, path::Path, process::Output};

#[path = "support/producer.rs"]
mod support;
use support::{Checkout, fake_npm_install, pinned_version, start_checkout, stub_env, tripwire_dir};

const PRODUCERS: [&str; 4] = [
    "scip-java",
    "scip-typescript",
    "scip-python",
    "rust-analyzer",
];

fn sources(root: &Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.js"), "export const a = 1;\n").unwrap();
    fs::write(root.join("package.json"), "{\"name\":\"fixture\"}\n").unwrap();
    fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"fixture\"\n",
    )
    .unwrap();
    fs::write(root.join("build.gradle"), "// build\n").unwrap();
}

fn with_path(checkout: &Checkout, dirs: &[&Path], args: &[&str]) -> Output {
    let path = dirs
        .iter()
        .map(|dir| dir.display().to_string())
        .chain(["/usr/bin".to_owned(), "/bin".to_owned()])
        .collect::<Vec<_>>()
        .join(":");
    checkout
        .cli()
        .env("PATH", path)
        .args(args)
        .arg("--workspace")
        .arg(&checkout.root)
        .output()
        .unwrap()
}

fn json_out(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn stderr(output: &Output) -> String {
    assert!(!output.status.success());
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn plan(checkout: &Checkout, dirs: &[&Path], producer: &str, trust: &str) -> Output {
    with_path(
        checkout,
        dirs,
        &["producer", "plan", "--producer", producer, "--trust", trust],
    )
}

#[test]
fn plan_lists_every_inspectable_field() {
    let tmp = tempfile::tempdir().unwrap();
    let ts = fake_npm_install(
        &tmp.path().join("ts"),
        "scip-typescript",
        &pinned_version("scip-typescript"),
    );
    let py = fake_npm_install(
        &tmp.path().join("py"),
        "scip-python",
        &pinned_version("scip-python"),
    );
    let checkout = start_checkout(sources, &[]);

    let plan_ts = json_out(&plan(&checkout, &[&ts], "scip-typescript", "restricted"));
    assert_eq!(plan_ts["producer"]["name"], "scip-typescript");
    assert_eq!(plan_ts["producer"]["language"], "javascript");
    assert_eq!(
        plan_ts["producer"]["version"],
        pinned_version("scip-typescript")
    );
    assert_eq!(plan_ts["producer"]["positionEncoding"], "utf16");
    assert_eq!(
        plan_ts["producer"]["verification"]["method"],
        "packageManifestVersion"
    );
    assert_eq!(plan_ts["producer"]["verification"]["result"], "verified");
    assert_eq!(plan_ts["trustProfile"], "restricted");
    assert_eq!(plan_ts["workspace"], checkout.root.to_str().unwrap());
    let hash = plan_ts["planSha256"].as_str().unwrap();
    assert!(hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()));
    let argv = plan_ts["command"]["argv"].as_array().unwrap();
    assert!(argv[0].as_str().unwrap().ends_with("/node"));
    assert!(argv.contains(&json!("--infer-tsconfig")));
    let cwd = plan_ts["command"]["cwd"].as_str().unwrap();
    assert!(cwd.ends_with("/runs/{runId}/worktree"), "{cwd}");
    assert!(
        plan_ts["command"]["env"]["PATH"]
            .as_str()
            .unwrap()
            .ends_with(":/usr/bin:/bin")
    );
    let runtime = plan_ts["runtimeExecutables"].as_array().unwrap();
    assert_eq!(runtime[0]["name"], "node");
    assert_eq!(runtime[0]["generation"], "runtime");
    assert_eq!(plan_ts["generation"]["runtime"][0]["name"], "node");
    assert!(
        plan_ts["generation"]["observed"]
            .as_array()
            .unwrap()
            .contains(&json!("runtime"))
    );
    let inputs = &plan_ts["inputs"];
    assert!(
        inputs["sourceRevision"]
            .as_str()
            .unwrap()
            .starts_with("revision:v2:")
    );
    assert_eq!(inputs["sourceManifestHash"].as_str().unwrap().len(), 64);
    assert_eq!(inputs["documents"]["requestedLanguage"], "javascript");
    assert_eq!(inputs["documents"]["requested"], 1);
    assert!(
        inputs["config"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == "package.json")
    );
    assert!(inputs["omitted"].as_array().unwrap().is_empty());
    for key in [
        "network",
        "packageManager",
        "javaBuildLogic",
        "annotationProcessors",
        "javascriptLifecycleScripts",
        "rustBuildScripts",
        "rustProcMacros",
        "pythonEnvironmentInspection",
    ] {
        for field in ["decision", "enforcement", "detail"] {
            assert!(plan_ts["policy"][key][field].is_string(), "{key}.{field}");
        }
    }
    assert_eq!(
        plan_ts["policy"]["javascriptLifecycleScripts"]["decision"],
        "deny"
    );
    assert_eq!(plan_ts["limits"]["memoryEnforcement"], "sampled");
    assert_eq!(plan_ts["limits"]["timeoutSecs"], 1800);
    assert_eq!(plan_ts["output"]["artifact"], "out/index.scip");

    let restricted = json_out(&plan(&checkout, &[&py], "scip-python", "restricted"));
    let omitted = restricted["inputs"]["omitted"].as_array().unwrap();
    assert_eq!(omitted.len(), 1);
    assert_eq!(omitted[0]["path"], "pyproject.toml");
    assert_eq!(
        restricted["inputs"]["generatedConfig"][0]["path"],
        "producer-config/environment.json"
    );
    assert_eq!(
        restricted["policy"]["pythonEnvironmentInspection"]["decision"],
        "deny"
    );
    assert_eq!(
        restricted["policy"]["pythonEnvironmentInspection"]["enforcement"],
        "structural"
    );

    let trusted = json_out(&plan(&checkout, &[&py], "scip-python", "trusted"));
    assert_eq!(
        trusted["policy"]["pythonEnvironmentInspection"]["decision"],
        "allow"
    );
    assert!(trusted["inputs"]["omitted"].as_array().unwrap().is_empty());
}

#[test]
fn plan_and_status_execute_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (tripwire, marker) = tripwire_dir(tmp.path());
    let stub_marker = tmp.path().join("stub.marker");
    let envs = stub_env(
        tmp.path(),
        &json!({"marker": stub_marker, "emit": tmp.path().join("none.scip")}),
    );
    let checkout = start_checkout(sources, &envs);
    let cli = |args: &[&str]| {
        let mut command = checkout.cli();
        for (key, value) in &envs {
            command.env(key, value);
        }
        command
            .env("PATH", format!("{}:/usr/bin:/bin", tripwire.display()))
            .args(args)
            .arg("--workspace")
            .arg(&checkout.root)
            .output()
            .unwrap()
    };
    for producer in PRODUCERS.iter().chain(&["trellis-synthetic-scip"]) {
        for trust in ["restricted", "trusted"] {
            let output = cli(&["producer", "plan", "--producer", producer, "--trust", trust]);
            if *producer == "trellis-synthetic-scip" {
                assert_eq!(json_out(&output)["producer"]["name"], *producer);
            } else if output.status.success() {
                // A host that really has the producer installed plans it; the
                // tripwire scripts shadow only the launchers and runtimes.
                assert_eq!(json_out(&output)["trustProfile"], trust);
            } else {
                let error = String::from_utf8_lossy(&output.stderr);
                assert!(
                    error.contains("producer_unavailable") || error.contains("profile_unsupported"),
                    "{error}"
                );
            }
        }
    }
    let status = json_out(&cli(&["producer", "status"]));
    assert!(status["activeRun"].is_null());
    for producer in status["producers"].as_array().unwrap() {
        assert!(producer["lastRun"].is_null());
    }
    assert!(!marker.exists(), "a tripwire script ran");
    assert!(!stub_marker.exists(), "the stub ran");
    assert!(
        !support::cache_dir(&checkout.home)
            .join("producers")
            .exists(),
        "planning created state"
    );
}

#[test]
fn rust_analyzer_restricted_plan_is_profile_unsupported() {
    let checkout = start_checkout(sources, &[]);
    let output = plan(&checkout, &[], "rust-analyzer", "restricted");
    assert!(stderr(&output).contains("profile_unsupported"));
}

#[test]
fn plan_is_stable_until_basis_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let envs = stub_env(tmp.path(), &json!({"exit": 0}));
    let checkout = start_checkout(sources, &envs);
    let first = json_out(&plan(
        &checkout,
        &[],
        "trellis-synthetic-scip",
        "restricted",
    ));
    let second = json_out(&plan(
        &checkout,
        &[],
        "trellis-synthetic-scip",
        "restricted",
    ));
    assert_eq!(first, second);

    fs::write(checkout.root.join("src/a.js"), "export const a = 2;\n").unwrap();
    let index = checkout
        .cli()
        .args(["index", "--workspace"])
        .arg(&checkout.root)
        .output()
        .unwrap();
    assert!(index.status.success());
    let third = json_out(&plan(
        &checkout,
        &[],
        "trellis-synthetic-scip",
        "restricted",
    ));
    assert_ne!(third["planSha256"], first["planSha256"]);
    assert_ne!(
        third["inputs"]["sourceRevision"],
        first["inputs"]["sourceRevision"]
    );
}

#[test]
fn status_reports_detection_and_imported_only() {
    let tmp = tempfile::tempdir().unwrap();
    let ts = fake_npm_install(
        tmp.path(),
        "scip-typescript",
        &pinned_version("scip-typescript"),
    );
    let checkout = start_checkout(sources, &[]);
    let status = json_out(&with_path(&checkout, &[&ts], &["producer", "status"]));
    assert!(
        status["head"]["sourceRevision"]
            .as_str()
            .unwrap()
            .starts_with("revision:v2:")
    );
    assert!(status["activeRun"].is_null());
    let producers = status["producers"].as_array().unwrap();
    let by_name = |name: &str| producers.iter().find(|p| p["name"] == name).unwrap();
    for name in PRODUCERS {
        let producer = by_name(name);
        assert_eq!(producer["mode"], "importedOnly");
        assert!(producer["lastRun"].is_null());
        assert!(producer["pickup"].is_null());
    }
    assert_eq!(
        by_name("scip-typescript")["detection"]["result"],
        "available"
    );
    assert_eq!(
        by_name("scip-typescript")["trustProfiles"]["trusted"]["result"],
        "available"
    );
    assert_eq!(by_name("scip-java")["detection"]["result"], "unavailable");
    assert!(by_name("scip-java")["detection"]["reason"].is_string());
    let rust = by_name("rust-analyzer");
    assert_eq!(
        rust["trustProfiles"]["restricted"]["result"],
        "profile_unsupported"
    );
}

#[test]
fn trusted_java_and_rust_plans_declare_downloads_and_caches() {
    use std::path::PathBuf;
    use trellis::producer::{
        ProducerName, TrustProfile,
        detect::{Declared, Environment, Exe, Resolution, Verification},
        plan::{Basis, LimitsRequest, Parts, build},
        profiles::{Row, Target},
    };
    let environment = Environment {
        path: "/usr/bin".into(),
        home: "/home/u".into(),
        rustup_home: Some("/home/u/.rustup".into()),
        cargo_home: Some("/home/u/.cargo".into()),
        coursier_cache: Some("/home/u/coursier/v1".into()),
        coursier_archive_cache: Some("/home/u/coursier/arc".into()),
    };
    let declared = Declared::new(&environment).unwrap();
    let exe = |path: &str| Exe {
        path: PathBuf::from(path),
        sha256: "0".repeat(64),
        dir: PathBuf::from(path).parent().unwrap().to_path_buf(),
    };
    let basis = Basis {
        source_set_id: "set".into(),
        root_id: "root".into(),
        source_revision: "revision".into(),
        source_manifest_hash: "0".repeat(64),
        toolchain_hash: "0".repeat(64),
        config_hash: "0".repeat(64),
        dependency_hash: "0".repeat(64),
        documents: 1,
        requested_documents: 1,
        inputs: Default::default(),
    };
    let planned = |producer: ProducerName, resolution: Resolution| {
        let pin = trellis::scip_admission::pinned_producer_by_id(producer.as_str()).unwrap();
        build(&Parts {
            workspace: "/w",
            row: Row {
                target: Target::Real(producer),
                trust: TrustProfile::Trusted,
            },
            pin: &pin,
            resolution: &resolution,
            declared: &declared,
            stub: None,
            basis: &basis,
            limits: LimitsRequest {
                timeout_secs: 60,
                memory_mib: 1024,
            },
            run_dir: "/state/runs/{runId}",
        })
    };
    let resolution = |java: bool| Resolution {
        launcher: PathBuf::from("/bin/launcher"),
        verification: Verification {
            method: "executableSha256",
            observed: "0".repeat(64),
        },
        node: None,
        java: java.then(|| exe("/jdk/bin/java")),
        java_home: None,
        rustup: (!java).then(|| exe("/bin/rustup")),
        python3: None,
    };

    let java = planned(ProducerName::ScipJava, resolution(true));
    assert_eq!(java["policy"]["network"]["decision"], "allow");
    assert!(
        java["policy"]["network"]["detail"]
            .as_str()
            .unwrap()
            .contains("download")
    );
    let caches = java["inputs"]["declaredCaches"].as_array().unwrap();
    let named = |name: &str| caches.iter().find(|c| c["name"] == name).unwrap()["path"].clone();
    assert_eq!(named("COURSIER_CACHE"), "/home/u/coursier/v1");
    assert_eq!(named("COURSIER_ARCHIVE_CACHE"), "/home/u/coursier/arc");

    let rust = planned(ProducerName::RustAnalyzer, resolution(false));
    assert_eq!(rust["policy"]["network"]["decision"], "allow");
    assert!(
        rust["policy"]["network"]["detail"]
            .as_str()
            .unwrap()
            .contains("download")
    );
    let caches = rust["inputs"]["declaredCaches"].as_array().unwrap();
    let named = |name: &str| caches.iter().find(|c| c["name"] == name).unwrap()["path"].clone();
    assert_eq!(named("RUSTUP_HOME"), "/home/u/.rustup");
    assert_eq!(named("CARGO_HOME"), "/home/u/.cargo");
}
