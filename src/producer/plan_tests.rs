use super::detect::{Declared, Environment, Exe, Resolution, Verification};
use super::plan::{self, Basis, LimitsRequest, Parts, PlanRequest};
use super::profiles::{self, Target};
use super::worktree::{RetainedInputs, RootInput};
use super::{ProducerName, TrustProfile};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn basis(revision: &str) -> Basis {
    Basis {
        source_set_id: "source-set:v1:root".into(),
        root_id: "root".into(),
        source_revision: revision.into(),
        source_manifest_hash: "1".repeat(64),
        toolchain_hash: "2".repeat(64),
        config_hash: "3".repeat(64),
        dependency_hash: "4".repeat(64),
        documents: 3,
        requested_documents: 2,
        inputs: RetainedInputs {
            config: vec![RootInput {
                path: "pyproject.toml".into(),
                bytes: b"[project]".to_vec(),
            }],
            dependencies: vec![],
        },
    }
}

fn build(revision: &str, trust: TrustProfile) -> Value {
    let row = profiles::row(Target::Real(ProducerName::ScipPython), trust).unwrap();
    let pin = row.target.pin().unwrap();
    let node = Exe {
        path: PathBuf::from("/n/node"),
        sha256: "5".repeat(64),
        dir: PathBuf::from("/n"),
    };
    let resolution = Resolution {
        launcher: PathBuf::from("/n/main.js"),
        verification: Verification {
            method: "packageManifestVersion",
            observed: pin.version.clone(),
        },
        node: Some(node.clone()),
        java: None,
        java_home: None,
        rustup: None,
        python3: Some(Exe {
            path: PathBuf::from("/p/python3"),
            sha256: "6".repeat(64),
            dir: PathBuf::from("/p"),
        }),
    };
    let declared = Declared::new(&Environment {
        path: "/n".into(),
        home: "/h".into(),
        ..Environment::default()
    })
    .unwrap();
    plan::build(&Parts {
        workspace: "/w",
        row,
        pin: &pin,
        resolution: &resolution,
        declared: &declared,
        stub: None,
        basis: &basis(revision),
        limits: LimitsRequest {
            timeout_secs: 1800,
            memory_mib: 8192,
        },
        run_dir: "/state/runs/{runId}",
    })
}

/// The same value with every object's keys inserted in reverse order.
fn reversed(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .rev()
                .map(|(k, v)| (k.clone(), reversed(v)))
                .collect(),
        ),
        Value::Array(items) => items.iter().map(reversed).collect(),
        other => other.clone(),
    }
}

/// A producer run admits exactly what batch import admits, so a run past the old cap imports.
#[test]
fn run_artifact_bound_is_the_batch_artifact_limit() {
    assert_eq!(
        super::exec::Limits::default().artifact_bytes,
        crate::semantic_batch::limits::BatchLimits::PRODUCTION.artifact_bytes
    );
}

#[test]
fn plan_hash_is_canonical_and_stable() {
    let plan = build("revision:v2:a", TrustProfile::Restricted);
    let hash = plan::plan_sha256(&plan);
    assert_eq!(hash.len(), 64);
    assert_eq!(
        hash,
        plan::plan_sha256(&build("revision:v2:a", TrustProfile::Restricted))
    );
    assert_eq!(hash, plan::plan_sha256(&reversed(&plan)));
    assert_eq!(
        hash,
        hex::encode(Sha256::digest(crate::native_ids::canonical(&plan)))
    );

    let hashed = plan::with_hash(plan.clone());
    assert_eq!(hashed["planSha256"], json!(hash));
    let mut without = hashed.clone();
    without.as_object_mut().unwrap().remove("planSha256");
    assert_eq!(without, plan);

    assert_ne!(
        hash,
        plan::plan_sha256(&build("revision:v2:b", TrustProfile::Restricted))
    );
    assert_ne!(
        hash,
        plan::plan_sha256(&build("revision:v2:a", TrustProfile::Trusted))
    );
}

#[test]
fn plan_shows_the_generation_table_and_omissions() {
    let plan = build("revision:v2:a", TrustProfile::Restricted);
    assert_eq!(plan["generation"]["runtime"][0]["name"], "node");
    assert_eq!(plan["generation"]["trustProfile"], "restricted");
    assert_eq!(plan["inputs"]["omitted"][0]["path"], "pyproject.toml");
    assert!(plan["inputs"]["config"].as_array().unwrap().is_empty());
    assert_eq!(plan["command"]["cwd"], "/state/runs/{runId}/worktree");
    assert_eq!(plan["command"]["env"]["PATH"], "/state/runs/{runId}/bin");
    let trusted = build("revision:v2:a", TrustProfile::Trusted);
    assert_eq!(trusted["command"]["env"]["PATH"], "/p:/n:/usr/bin:/bin");
    let names: Vec<_> = trusted["runtimeExecutables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["name"].as_str().unwrap(),
                e["generation"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(names, [("node", "runtime"), ("python3", "notClaimed")]);
}

fn request(patch: impl FnOnce(&mut Value)) -> Result<PlanRequest, anyhow::Error> {
    let mut value = json!({
        "version": 1,
        "producer": "scip-java",
        "trustProfile": "restricted",
        "limits": {"timeoutSecs": 1800, "memoryMiB": 8192},
        "executable": null,
        "environment": {"PATH": "/usr/bin", "HOME": "/h"},
    });
    patch(&mut value);
    PlanRequest::parse(&value)
}

#[test]
fn plan_request_is_closed_and_bounded() {
    assert!(request(|_| {}).is_ok());
    for patch in [
        (|v: &mut Value| v["extra"] = json!(1)) as fn(&mut Value),
        |v| v["version"] = json!(2),
        |v| v["limits"]["timeoutSecs"] = json!(0),
        |v| v["limits"]["timeoutSecs"] = json!(14401),
        |v| v["limits"]["memoryMiB"] = json!(63),
        |v| v["executable"] = json!("relative/path"),
        |v| v["trustProfile"] = json!("root"),
        |v| v["environment"]["HOME"] = json!("rel"),
        |v| v["environment"]["EXTRA"] = json!("x"),
        |v| v["environment"]["PATH"] = json!("/a:".repeat(20_000)),
    ] {
        let error = request(patch).unwrap_err().to_string();
        assert!(error.starts_with("invalid_request"), "{error}");
    }
}
