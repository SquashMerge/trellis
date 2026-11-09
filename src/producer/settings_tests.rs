//! #717: rust-analyzer reads `cargo.features` and the workspace members from the retained
//! Cargo manifests, an override replaces them, and malformed or contradictory manifests are
//! the typed refusal. The plan lists each setting and hashes it.
use super::detect::{Declared, Environment, Exe, Resolution, Verification};
use super::plan::{self, Basis, LimitsRequest, Parts};
use super::profiles::{self, Rules, Target};
use super::settings::{self, MAX_FEATURES, Overrides, RUST_ANALYZER_CONFIG, TsProject};
use super::tsconfig::CONFIG_UNRESOLVED;
use super::worktree::{RetainedInputs, RootInput};
use super::{ProducerName, TrustProfile};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// This repository's shape: a single package whose example and stub binary require
/// `test-support`, and a platform feature that must stay off.
const SINGLE: &str = r#"
[package]
name = "trellis"
version = "0.1.0"

[features]
default = []
test-support = []
metal = []

[[example]]
name = "fixture"
required-features = ["test-support"]

[[bin]]
name = "trellis-producer-stub"
required-features = ["test-support"]
"#;

fn inputs(files: &[(&str, &str)]) -> RetainedInputs {
    RetainedInputs {
        config: files
            .iter()
            .map(|(path, text)| RootInput {
                path: (*path).into(),
                bytes: text.as_bytes().to_vec(),
            })
            .collect(),
        dependencies: vec![],
    }
}

/// The plan and rules for `producer` over `files`, with the settings `prepare` derives.
fn planned(
    producer: ProducerName,
    files: &[(&str, &str)],
    overrides: &Overrides,
) -> anyhow::Result<(Value, Rules)> {
    let row = profiles::row(Target::Real(producer), TrustProfile::Trusted).unwrap();
    let inputs = inputs(files);
    let mut rules = profiles::rules(&row);
    let workspace = tempfile::tempdir().unwrap();
    let settings =
        plan::settings_for(row.target, &mut rules, &inputs, workspace.path(), overrides)?;
    let pin = row.target.pin().unwrap();
    let exe = |path: &str| Exe {
        path: PathBuf::from(path),
        sha256: "0".repeat(64),
        dir: Path::new(path).parent().unwrap().to_path_buf(),
    };
    let resolution = Resolution {
        launcher: PathBuf::from("/bin/launcher"),
        verification: Verification {
            method: "executableSha256",
            observed: "0".repeat(64),
        },
        node: Some(exe("/n/node")),
        java: None,
        java_home: None,
        rustup: Some(exe("/bin/rustup")),
        python3: None,
    };
    let declared = Declared::new(&Environment {
        path: "/usr/bin".into(),
        home: "/h".into(),
        ..Environment::default()
    })
    .unwrap();
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
        inputs,
    };
    let plan = plan::with_hash(plan::build(&Parts {
        workspace: "/w",
        row,
        pin: &pin,
        resolution: &resolution,
        declared: &declared,
        stub: None,
        basis: &basis,
        rules: &rules,
        settings: &settings,
        limits: LimitsRequest {
            timeout_secs: 60,
            memory_mib: 1024,
        },
        run_dir: "/state/runs/{runId}",
    }));
    Ok((plan, rules))
}

fn rust(files: &[(&str, &str)]) -> (Value, Rules) {
    planned(ProducerName::RustAnalyzer, files, &Overrides::default()).unwrap()
}

fn cargo_override(features: &[&str]) -> Overrides {
    Overrides {
        cargo_features: Some(features.iter().map(|f| (*f).to_owned()).collect()),
        typescript_project: None,
    }
}

fn setting<'a>(plan: &'a Value, name: &str) -> &'a Value {
    plan["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|setting| setting["name"] == name)
        .unwrap_or_else(|| panic!("no {name}: {}", plan["settings"]))
}

fn config_path(plan: &Value) -> Option<&Value> {
    let argv = plan["command"]["argv"].as_array().unwrap();
    let at = argv.iter().position(|arg| arg == "--config-path")?;
    argv.get(at + 1)
}

#[track_caller]
fn refusal(files: &[(&str, &str)]) -> String {
    let error = planned(ProducerName::RustAnalyzer, files, &Overrides::default())
        .unwrap_err()
        .to_string();
    assert!(error.starts_with(CONFIG_UNRESOLVED), "{error}");
    error
}

#[test]
fn required_features_are_enabled_and_listed_with_their_source() {
    let (plan, rules) = rust(&[("Cargo.toml", SINGLE)]);
    let features = setting(&plan, "cargo.features");
    assert_eq!(features["value"], json!(["test-support"]));
    assert_eq!(features["source"], "project");
    assert_eq!(features["from"][0]["path"], "Cargo.toml");
    let detail = features["detail"].as_str().unwrap();
    assert!(
        detail.contains("required-features of [[bin]] trellis-producer-stub in Cargo.toml"),
        "{detail}"
    );
    // Never all: the platform feature stays off.
    assert!(!features["value"].to_string().contains("metal"));
    let config = rules
        .generated
        .iter()
        .find(|file| file.path == RUST_ANALYZER_CONFIG)
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&config.bytes).unwrap(),
        json!({"cargo": {"features": ["test-support"]}})
    );
    assert_eq!(
        config_path(&plan),
        Some(&json!(format!(
            "/state/runs/{{runId}}/{RUST_ANALYZER_CONFIG}"
        )))
    );
    let generated = plan["inputs"]["generatedConfig"].as_array().unwrap();
    assert_eq!(generated[0]["path"], RUST_ANALYZER_CONFIG, "{generated:?}");
    assert_eq!(
        setting(&plan, "cargo.workspaceMembers")["value"],
        json!(["."])
    );
}

#[test]
fn derived_features_change_the_plan_hash() {
    let without = SINGLE.replace("required-features = [\"test-support\"]", "");
    let (plain, plain_rules) = rust(&[("Cargo.toml", &without)]);
    let (gated, _) = rust(&[("Cargo.toml", SINGLE)]);
    assert_eq!(setting(&plain, "cargo.features")["value"], json!([]));
    assert_eq!(setting(&plain, "cargo.features")["source"], "default");
    assert!(config_path(&plain).is_none());
    assert!(plain_rules.generated.is_empty());
    // The derived setting itself, not only the manifest's bytes, separates the two plans.
    assert_eq!(
        setting(&gated, "cargo.features")["value"],
        json!(["test-support"])
    );
    assert_ne!(plain["command"]["argv"], gated["command"]["argv"]);
    assert_ne!(
        plain["inputs"]["generatedConfig"],
        gated["inputs"]["generatedConfig"]
    );
    assert_ne!(plain["planSha256"], gated["planSha256"]);
}

#[test]
fn workspace_members_are_qualified_and_non_members_are_not_read() {
    let member = |name: &str, feature: &str| {
        format!(
            "[package]\nname = \"{name}\"\n[features]\n{feature} = []\n[[test]]\nname = \"t\"\nrequired-features = [\"{feature}\"]\n"
        )
    };
    let (a, b) = (member("a", "fa"), member("b", "fb"));
    let (plan, _) = rust(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/skip\"]\n",
        ),
        ("crates/a/Cargo.toml", &a),
        ("crates/b/Cargo.toml", &b),
        // Excluded, and outside the members: neither is read, so neither refuses.
        ("crates/skip/Cargo.toml", "not toml ["),
        ("tests/fixture/Cargo.toml", "not toml ["),
    ]);
    assert_eq!(
        setting(&plan, "cargo.features")["value"],
        json!(["a/fa", "b/fb"])
    );
    let members = setting(&plan, "cargo.workspaceMembers");
    assert_eq!(members["value"], json!(["crates/a", "crates/b"]));
    assert_eq!(members["from"][0]["path"], "Cargo.toml");
}

#[test]
fn the_override_replaces_the_derived_features() {
    // The override is not merged with test-support, and an undeclared required feature is
    // not checked once the derived value is replaced.
    let undeclared = SINGLE.replace("test-support = []", "");
    let (plan, rules) = planned(
        ProducerName::RustAnalyzer,
        &[("Cargo.toml", &undeclared)],
        &cargo_override(&["metal"]),
    )
    .unwrap();
    let features = setting(&plan, "cargo.features");
    assert_eq!(features["value"], json!(["metal"]));
    assert_eq!(features["source"], "override");
    assert_eq!(
        serde_json::from_slice::<Value>(&rules.generated[0].bytes).unwrap(),
        json!({"cargo": {"features": ["metal"]}})
    );
    // An empty override enables nothing.
    let (none, rules) = planned(
        ProducerName::RustAnalyzer,
        &[("Cargo.toml", SINGLE)],
        &cargo_override(&[]),
    )
    .unwrap();
    assert_eq!(setting(&none, "cargo.features")["value"], json!([]));
    assert!(config_path(&none).is_none() && rules.generated.is_empty());
    let (derived, _) = rust(&[("Cargo.toml", SINGLE)]);
    assert_ne!(none["planSha256"], derived["planSha256"]);
}

#[test]
fn no_root_manifest_is_default_features() {
    let (plan, rules) = rust(&[("package.json", "{}")]);
    assert_eq!(setting(&plan, "cargo.features")["source"], "default");
    assert!(rules.generated.is_empty());
    assert_eq!(plan["settings"].as_array().unwrap().len(), 1);
}

#[test]
fn malformed_or_contradictory_manifests_are_typed_refusals() {
    let ws = "[workspace]\nmembers = [\"a\"]\n";
    let placeholder = std::str::from_utf8(crate::skipped_config::PLACEHOLDER).unwrap();
    for (files, expected) in [
        (
            vec![("Cargo.toml", "[package\nname=")],
            "Cargo.toml is not valid TOML",
        ),
        (
            vec![("Cargo.toml", ws), ("a/Cargo.toml", "= broken")],
            "a/Cargo.toml is not valid TOML",
        ),
        (
            vec![("Cargo.toml", ws), ("a/Cargo.toml", "[lib]\n")],
            "a workspace member without [package]",
        ),
        (
            vec![("Cargo.toml", "workspace = 1\n")],
            "workspace is not a table",
        ),
        (
            vec![("Cargo.toml", "[workspace]\nmembers = \"a\"\n")],
            "members is not an array of strings",
        ),
        (
            vec![("Cargo.toml", "[workspace]\nmembers = [\"a[\"]\n")],
            "is not a valid glob",
        ),
        (
            vec![(
                "Cargo.toml",
                "[package]\nname = \"p\"\n[[bin]]\nname = \"b\"\nrequired-features = \"x\"\n",
            )],
            "required-features is not an array of strings",
        ),
        (
            vec![("Cargo.toml", "bin = 3\n[package]\nname = \"p\"\n")],
            "bin is not an array of tables",
        ),
        (
            vec![(
                "Cargo.toml",
                "[package]\nname = \"p\"\n[[bin]]\nname = \"b\"\nrequired-features = [\"missing\"]\n",
            )],
            "requires feature \"missing\", which [features] does not declare",
        ),
        (
            vec![(
                "Cargo.toml",
                "[package]\nname = \"p\"\n[[bin]]\nname = \"b\"\nrequired-features = [\"a,b\"]\n",
            )],
            "which is not a Cargo feature name",
        ),
        (
            vec![("Cargo.toml", placeholder)],
            "over the config input cap",
        ),
    ] {
        let error = refusal(&files);
        assert!(error.contains(expected), "{expected}: {error}");
    }
    // An optional dependency's implicit feature is declared.
    let optional = "[package]\nname = \"p\"\n[dependencies]\nserde = { version = \"1\", optional = true }\n[[bin]]\nname = \"b\"\nrequired-features = [\"serde\"]\n";
    let (plan, _) = rust(&[("Cargo.toml", optional)]);
    assert_eq!(setting(&plan, "cargo.features")["value"], json!(["serde"]));
}

#[test]
fn hostile_manifests_are_bounded() {
    // Deep nesting is a typed refusal, not a stack overflow.
    let deep = format!(
        "[package]\nname = \"p\"\nx = {}{}\n",
        "[".repeat(100_000),
        "]".repeat(100_000)
    );
    refusal(&[("Cargo.toml", &deep)]);
    // More required features than a plan enables.
    let mut many = String::from("[package]\nname = \"p\"\n[features]\n");
    for n in 0..=MAX_FEATURES {
        many.push_str(&format!("f{n} = []\n"));
    }
    let names: Vec<String> = (0..=MAX_FEATURES).map(|n| format!("\"f{n}\"")).collect();
    many.push_str(&format!(
        "[[bin]]\nname = \"b\"\nrequired-features = [{}]\n",
        names.join(",")
    ));
    let error = refusal(&[("Cargo.toml", &many)]);
    assert!(
        error.contains(&format!("more than {MAX_FEATURES} features")),
        "{error}"
    );
    let binary = RetainedInputs {
        config: vec![RootInput {
            path: "Cargo.toml".into(),
            bytes: vec![0xff, 0xfe, b'['],
        }],
        dependencies: vec![],
    };
    let error = settings::rust(&mut Rules::default(), &binary, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("is not UTF-8"), "{error}");
}

#[test]
fn overrides_are_validated_per_producer() {
    let features = |list: Vec<String>| Overrides {
        cargo_features: Some(list),
        typescript_project: None,
    };
    assert!(
        features(vec!["test-support".into(), "dep?/f".into()])
            .validate("rust-analyzer")
            .is_ok()
    );
    for (overrides, producer) in [
        (features(vec!["x".into()]), "scip-typescript"),
        (features(vec!["a b".into()]), "rust-analyzer"),
        (features(vec!["".into()]), "rust-analyzer"),
        (features(vec!["x".repeat(129)]), "rust-analyzer"),
        (
            features((0..=MAX_FEATURES).map(|n| format!("f{n}")).collect()),
            "rust-analyzer",
        ),
        (
            Overrides {
                cargo_features: None,
                typescript_project: Some(TsProject::Generated),
            },
            "rust-analyzer",
        ),
    ] {
        let error = overrides.validate(producer).unwrap_err().to_string();
        assert!(error.starts_with("invalid_request"), "{error}");
    }
}

/// The scip-typescript project setting is listed and changes the plan hash with the config
/// it chooses; the override wins.
#[test]
fn typescript_project_setting_is_listed_and_hashed() {
    let ts = |files: &[(&str, &str)], project: Option<TsProject>| {
        planned(
            ProducerName::ScipTypescript,
            files,
            &Overrides {
                cargo_features: None,
                typescript_project: project,
            },
        )
        .unwrap()
        .0
    };
    let none = ts(&[], None);
    let js = ts(&[("jsconfig.json", "{}")], None);
    let project = setting(&js, "typescript.project");
    assert_eq!(project["value"], "jsconfig.json");
    assert_eq!(project["source"], "project");
    assert_eq!(project["from"][0]["path"], "jsconfig.json");
    assert_eq!(setting(&none, "typescript.project")["source"], "default");
    assert_ne!(none["planSha256"], js["planSha256"]);
    let generated = ts(&[("jsconfig.json", "{}")], Some(TsProject::Generated));
    let project = setting(&generated, "typescript.project");
    assert_eq!(project["value"], "generated");
    assert_eq!(project["source"], "override");
    assert_ne!(generated["planSha256"], js["planSha256"]);
}
