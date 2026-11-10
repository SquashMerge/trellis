//! #717: rust-analyzer reads `cargo.features` and the workspace members from the retained
//! Cargo manifests, scip-python its project name and version from `pyproject.toml`, and
//! scip-java its build tool from the root build files. An override replaces each, and
//! malformed or contradictory project files are the typed refusal. The plan lists each
//! setting and hashes it.
use super::detect::{Declared, Environment, Exe, Resolution, Verification};
use super::plan::{self, Basis, LimitsRequest, Parts};
use super::profiles::{self, Rules, Target};
use super::settings::{
    self, JavaBuildTool, MAX_FEATURES, Overrides, RUST_ANALYZER_CONFIG, TsProject,
    valid_python_name, valid_python_version,
};
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

/// The trusted plan and rules for `producer` over `files`, with the settings `prepare` derives.
fn planned(
    producer: ProducerName,
    files: &[(&str, &str)],
    overrides: &Overrides,
) -> anyhow::Result<(Value, Rules)> {
    planned_as(producer, TrustProfile::Trusted, files, overrides)
}

/// The plan and rules for `producer` under `trust` over `files`.
fn planned_as(
    producer: ProducerName,
    trust: TrustProfile,
    files: &[(&str, &str)],
    overrides: &Overrides,
) -> anyhow::Result<(Value, Rules)> {
    let row = profiles::row(Target::Real(producer), trust).unwrap();
    let inputs = inputs(files);
    let mut rules = profiles::rules(&row);
    let workspace = tempfile::tempdir().unwrap();
    let settings = plan::settings_for(row, &mut rules, &inputs, workspace.path(), overrides)?;
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
        ..Overrides::default()
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
        ..Overrides::default()
    };
    assert!(
        features(vec!["test-support".into(), "dep?/f".into()])
            .validate("rust-analyzer", TrustProfile::Trusted)
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
                typescript_project: Some(TsProject::Generated),
                ..Overrides::default()
            },
            "rust-analyzer",
        ),
    ] {
        let error = overrides
            .validate(producer, TrustProfile::Trusted)
            .unwrap_err()
            .to_string();
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
                typescript_project: project,
                ..Overrides::default()
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

/// #822 review: Cargo accepts Unicode XID feature names (Cargo 1.99.1 checks
/// `--features app/日本語`), so the plan derives one and passes it through to the config.
#[test]
fn unicode_feature_names_are_cargo_feature_names() {
    let unicode = "[package]\nname=\"app\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[features]\n\"日本語\"=[]\n[[bin]]\nname=\"app\"\nrequired-features=[\"日本語\"]\n";
    let (plan, rules) = rust(&[("Cargo.toml", unicode)]);
    assert_eq!(setting(&plan, "cargo.features")["value"], json!(["日本語"]));
    assert_eq!(
        serde_json::from_slice::<Value>(&rules.generated[0].bytes).unwrap(),
        json!({"cargo": {"features": ["日本語"]}})
    );
    assert!(config_path(&plan).is_some());
    // The override takes the same names.
    assert!(
        cargo_override(&["日本語", "dep/ñ", "_x", "1a", "a-b+c.d"])
            .validate("rust-analyzer", TrustProfile::Trusted)
            .is_ok()
    );
    // Names Cargo refuses stay refused, derived and overridden.
    for name in [
        "-x", "+x", ".x", "a b", "a,b", "a\u{7}", "🦀", "a/b/c", "dep:x", "a=b",
    ] {
        assert!(!settings::valid_feature(name), "{name:?}");
        let error = cargo_override(&[name])
            .validate("rust-analyzer", TrustProfile::Trusted)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("invalid_request"), "{name:?}: {error}");
    }
    let rejected = "[package]\nname=\"p\"\n[features]\n\"🦀\"=[]\n[[bin]]\nname=\"b\"\nrequired-features=[\"🦀\"]\n";
    let error = refusal(&[("Cargo.toml", rejected)]);
    assert!(
        error.contains("which is not a Cargo feature name"),
        "{error}"
    );
}

/// #822 review: an optional dependency has an implicit feature only while no `[features]`
/// value names it with `dep:`, as in Cargo 1.99.1 ("does not have feature `dep`").
#[test]
fn a_dep_colon_reference_suppresses_the_implicit_feature() {
    let manifest = |features: &str| {
        format!(
            "[package]\nname=\"app\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[dependencies]\ndep={{path=\"../actual\",package=\"actual\",optional=true}}\n[features]\n{features}\n[[bin]]\nname=\"app\"\nrequired-features=[\"dep\"]\n"
        )
    };
    let implicit = manifest("other=[\"dep/x\"]");
    let (plan, _) = rust(&[("Cargo.toml", &implicit)]);
    assert_eq!(setting(&plan, "cargo.features")["value"], json!(["dep"]));
    let suppressed = manifest("explicit=[\"dep:dep\"]");
    let error = refusal(&[("Cargo.toml", &suppressed)]);
    assert!(
        error.contains("requires feature \"dep\", which [features] does not declare"),
        "{error}"
    );
}

// scip-python (#717 part 2).

const PYPROJECT: &str = "[project]\nname = \"fixture-py\"\nversion = \"1.2.0rc1\"\n";

fn python(trust: TrustProfile, files: &[(&str, &str)], overrides: &Overrides) -> Value {
    planned_as(ProducerName::ScipPython, trust, files, overrides)
        .unwrap()
        .0
}

/// The value the plan's command passes for `flag`.
fn arg<'a>(plan: &'a Value, flag: &str) -> Option<&'a str> {
    let argv = plan["command"]["argv"].as_array().unwrap();
    let at = argv.iter().position(|arg| arg == flag)?;
    argv.get(at + 1).and_then(Value::as_str)
}

#[track_caller]
fn python_refusal(files: &[(&str, &str)]) -> String {
    let error = planned_as(
        ProducerName::ScipPython,
        TrustProfile::Trusted,
        files,
        &Overrides::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with(CONFIG_UNRESOLVED), "{error}");
    error
}

#[test]
fn python_project_name_and_version_are_read_from_pyproject() {
    let plan = python(
        TrustProfile::Trusted,
        &[("pyproject.toml", PYPROJECT)],
        &Overrides::default(),
    );
    let name = setting(&plan, "python.projectName");
    assert_eq!(name["value"], "fixture-py");
    assert_eq!(name["source"], "project");
    assert_eq!(name["from"][0]["path"], "pyproject.toml");
    assert_eq!(name["detail"], "[project] name of pyproject.toml");
    assert_eq!(setting(&plan, "python.projectVersion")["value"], "1.2.0rc1");
    assert_eq!(arg(&plan, "--project-name"), Some("fixture-py"));
    assert_eq!(arg(&plan, "--project-version"), Some("1.2.0rc1"));

    // Negative control: with no pyproject.toml the defaults are passed, as before #717.
    let none = python(TrustProfile::Trusted, &[], &Overrides::default());
    assert_eq!(setting(&none, "python.projectName")["source"], "default");
    assert_eq!(arg(&none, "--project-name"), Some("trellis-workspace"));
    assert_eq!(arg(&none, "--project-version"), Some("0"));
    assert_ne!(none["planSha256"], plan["planSha256"]);

    // The derived value is in the hash: another version is another plan.
    let other = python(
        TrustProfile::Trusted,
        &[("pyproject.toml", &PYPROJECT.replace("1.2.0rc1", "1.2.0"))],
        &Overrides::default(),
    );
    assert_eq!(arg(&other, "--project-version"), Some("1.2.0"));
    assert_ne!(other["planSha256"], plan["planSha256"]);
}

/// scip-python 0.6.6 takes a key `[project]` lacks from `[tool.poetry]`, and only then.
#[test]
fn python_poetry_fills_only_the_keys_project_lacks() {
    let mixed = "[project]\nname = \"pep621\"\ndynamic = [\"version\"]\n[tool.poetry]\nname = \"poetry-name\"\nversion = \"2.0\"\n";
    let plan = python(
        TrustProfile::Trusted,
        &[("pyproject.toml", mixed)],
        &Overrides::default(),
    );
    assert_eq!(arg(&plan, "--project-name"), Some("pep621"));
    assert_eq!(arg(&plan, "--project-version"), Some("2.0"));
    assert_eq!(
        setting(&plan, "python.projectVersion")["detail"],
        "[tool.poetry] version of pyproject.toml"
    );
    let poetry = "[tool.poetry]\nname = \"only-poetry\"\nversion = \"3\"\n";
    let plan = python(
        TrustProfile::Trusted,
        &[("pyproject.toml", poetry)],
        &Overrides::default(),
    );
    assert_eq!(arg(&plan, "--project-name"), Some("only-poetry"));
    // A dynamic version with no poetry version is the default.
    let dynamic = "[project]\nname = \"p\"\ndynamic = [\"version\"]\n";
    let plan = python(
        TrustProfile::Trusted,
        &[("pyproject.toml", dynamic)],
        &Overrides::default(),
    );
    let version = setting(&plan, "python.projectVersion");
    assert_eq!(version["source"], "default");
    assert_eq!(version["value"], "0");
    assert_eq!(
        version["detail"],
        "pyproject.toml sets no [project] or [tool.poetry] version"
    );
}

/// The restricted profile omits pyproject.toml, so the plan neither reads it nor refuses a
/// malformed one.
#[test]
fn restricted_python_does_not_read_the_omitted_pyproject() {
    for text in [PYPROJECT, "not [toml"] {
        let plan = python(
            TrustProfile::Restricted,
            &[("pyproject.toml", text)],
            &Overrides::default(),
        );
        let name = setting(&plan, "python.projectName");
        assert_eq!(name["source"], "default");
        assert_eq!(
            name["detail"],
            "the restricted profile omits pyproject.toml and reads neither it nor setup.cfg"
        );
        assert_eq!(arg(&plan, "--project-name"), Some("trellis-workspace"));
    }
}

#[test]
fn python_overrides_replace_the_derived_values() {
    let name_only = Overrides {
        python_project_name: Some("given".into()),
        ..Overrides::default()
    };
    let derived = python(
        TrustProfile::Trusted,
        &[("pyproject.toml", PYPROJECT)],
        &Overrides::default(),
    );
    let plan = python(
        TrustProfile::Trusted,
        &[("pyproject.toml", PYPROJECT)],
        &name_only,
    );
    assert_eq!(setting(&plan, "python.projectName")["source"], "override");
    assert_eq!(arg(&plan, "--project-name"), Some("given"));
    // The other key is still derived.
    assert_eq!(arg(&plan, "--project-version"), Some("1.2.0rc1"));
    assert_ne!(plan["planSha256"], derived["planSha256"]);
    // An override applies in the restricted profile too.
    let restricted = python(TrustProfile::Restricted, &[], &name_only);
    assert_eq!(arg(&restricted, "--project-name"), Some("given"));
    // Both overridden: pyproject.toml is not read, so a malformed one does not refuse.
    let both = Overrides {
        python_project_name: Some("given".into()),
        python_project_version: Some("4.0".into()),
        ..Overrides::default()
    };
    let plan = python(
        TrustProfile::Trusted,
        &[("pyproject.toml", "not [toml")],
        &both,
    );
    assert_eq!(arg(&plan, "--project-version"), Some("4.0"));
    // An override of one key leaves the other key's malformed value unread.
    let plan = python(
        TrustProfile::Trusted,
        &[("pyproject.toml", "[project]\nname = 1\nversion = \"1\"\n")],
        &name_only,
    );
    assert_eq!(arg(&plan, "--project-version"), Some("1"));
}

#[test]
fn malformed_pyproject_is_a_typed_refusal() {
    for (text, reason) in [
        ("not [toml", "is not valid TOML"),
        ("project = 1\n", "project is not a table"),
        ("[project]\nname = 1\n", "[project] name is not a string"),
        (
            "[project]\nname = \"a b\"\n",
            "is not a Python project name",
        ),
        ("[project]\nname = \"\"\n", "is not a Python project name"),
        (
            "[project]\nname = \"p\"\nversion = \"1.0 beta\"\n",
            "is not a PEP 440 version",
        ),
        ("[project]\nname = \"p\"\ntool = 1\n", ""),
        (
            "[project]\nname = \"p\"\n[tool]\npoetry = []\n",
            "poetry is not a table",
        ),
        ("tool = \"x\"\n", "tool is not a table"),
    ] {
        if reason.is_empty() {
            // `[project] tool` is not `[tool]`: nothing to refuse.
            python(
                TrustProfile::Trusted,
                &[("pyproject.toml", text)],
                &Overrides::default(),
            );
            continue;
        }
        let error = python_refusal(&[("pyproject.toml", text)]);
        assert!(error.contains(reason), "{text:?}: {error}");
    }
    // Negative control: a malformed [tool.poetry] is not read when [project] sets both keys.
    let plan = python(
        TrustProfile::Trusted,
        &[(
            "pyproject.toml",
            &format!("{PYPROJECT}[tool]\npoetry = 1\n"),
        )],
        &Overrides::default(),
    );
    assert_eq!(arg(&plan, "--project-name"), Some("fixture-py"));
    for bytes in [
        vec![0xff, 0xfe, b'['],
        crate::skipped_config::PLACEHOLDER.to_vec(),
    ] {
        let inputs = RetainedInputs {
            config: vec![RootInput {
                path: "pyproject.toml".into(),
                bytes,
            }],
            dependencies: vec![],
        };
        let error = settings::python(
            &mut Rules::default(),
            &inputs,
            TrustProfile::Trusted,
            &Overrides::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.starts_with(CONFIG_UNRESOLVED), "{error}");
    }
    // Deep nesting is bounded by the TOML parser, a refusal and not a stack overflow.
    let deep = format!(
        "[project]\nname = \"p\"\nx = {}{}\n",
        "[".repeat(100_000),
        "]".repeat(100_000)
    );
    python_refusal(&[("pyproject.toml", &deep)]);
}

/// The validators follow PEP 508 names and `packaging`'s PEP 440 `VERSION_PATTERN`; the
/// cases were checked against pip 26.2.1's vendored `packaging` (evidence log).
#[test]
fn python_names_and_versions_follow_the_packaging_rules() {
    for name in ["a", "A1", "my.pkg_name-2", "0x"] {
        assert!(valid_python_name(name), "{name}");
    }
    for name in ["", "-a", "a-", ".a", "a b", "a/b", "é", &"a".repeat(257)] {
        assert!(!valid_python_name(name), "{name}");
    }
    for version in [
        "0",
        "1.0",
        "v1.0",
        "V1.0",
        "1!2.0",
        "1.0a1",
        "1.0.alpha.1",
        "1.0-preview-2",
        "1.0rc",
        "1.0c1",
        "1.0a.",
        "1.0-1",
        "1.0.post1",
        "1.0r",
        "1.0-rev.3",
        "1.0.dev",
        "1.0-dev-",
        "1.0dev2",
        "1.0a1.post2.dev3",
        "1.0+local.1-x_y",
        "1.0RC1",
        "2026.11.09",
    ] {
        assert!(valid_python_version(version), "{version}");
    }
    for version in [
        "",
        "v",
        "1.",
        ".1",
        "1..0",
        "1.0 ",
        " 1.0",
        "1.0-",
        "1.0+",
        "1.0+a..b",
        "1.0+-a",
        "1.0gamma",
        "1!",
        "!1",
        "1.0.post1.post2",
        "1.0é",
        "a1",
    ] {
        assert!(!valid_python_version(version), "{version}");
    }
    assert!(!valid_python_version(&format!("1.{}", "0".repeat(256))));
}

// scip-java (#717 part 2).

fn java(trust: TrustProfile, files: &[(&str, &str)], tool: Option<JavaBuildTool>) -> Value {
    java_result(trust, files, tool).unwrap()
}

fn java_result(
    trust: TrustProfile,
    files: &[(&str, &str)],
    tool: Option<JavaBuildTool>,
) -> anyhow::Result<Value> {
    let overrides = Overrides {
        java_build_tool: tool,
        ..Overrides::default()
    };
    Ok(planned_as(ProducerName::ScipJava, trust, files, &overrides)?.0)
}

#[test]
fn java_build_tool_is_detected_from_the_root_build_files() {
    for (file, tool) in [
        ("pom.xml", "Maven"),
        ("build.gradle", "Gradle"),
        ("build.gradle.kts", "Gradle"),
        ("settings.gradle", "Gradle"),
    ] {
        let plan = java(TrustProfile::Trusted, &[(file, "x")], None);
        let setting = setting(&plan, "java.buildTool");
        assert_eq!(setting["value"], tool, "{file}");
        assert_eq!(setting["source"], "project");
        assert_eq!(setting["from"][0]["path"], file);
        assert_eq!(arg(&plan, "--build-tool"), Some(tool));
    }
    let gradle = java(
        TrustProfile::Trusted,
        &[("build.gradle", "x"), ("settings.gradle", "y")],
        None,
    );
    assert_eq!(
        setting(&gradle, "java.buildTool")["from"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let maven = java(TrustProfile::Trusted, &[("pom.xml", "x")], None);
    assert_ne!(gradle["planSha256"], maven["planSha256"]);
}

/// Negative controls, as the pinned scip-java detects: `settings.gradle.kts` or
/// `gradle.properties` alone selects no build tool, and nothing is passed.
#[test]
fn java_files_scip_java_does_not_detect_select_nothing() {
    for files in [
        &[][..],
        &[("settings.gradle.kts", "x")][..],
        &[("gradle.properties", "x")][..],
        &[("settings.gradle.kts", "x"), ("gradle.properties", "y")][..],
    ] {
        let plan = java(TrustProfile::Trusted, files, None);
        let setting = setting(&plan, "java.buildTool");
        assert_eq!(setting["value"], Value::Null, "{files:?}");
        assert_eq!(setting["source"], "default");
        assert_eq!(arg(&plan, "--build-tool"), None);
    }
}

#[test]
fn java_maven_and_gradle_together_refuse_unless_overridden() {
    let both = [("pom.xml", "x"), ("build.gradle.kts", "y")];
    let error = java_result(TrustProfile::Trusted, &both, None)
        .unwrap_err()
        .to_string();
    assert!(error.starts_with(CONFIG_UNRESOLVED), "{error}");
    assert!(
        error.contains("pom.xml, build.gradle.kts select Maven and Gradle"),
        "{error}"
    );
    let plan = java(TrustProfile::Trusted, &both, Some(JavaBuildTool::Gradle));
    let setting = setting(&plan, "java.buildTool");
    assert_eq!(setting["value"], "Gradle");
    assert_eq!(setting["source"], "override");
    assert_eq!(arg(&plan, "--build-tool"), Some("Gradle"));
    let maven = java(TrustProfile::Trusted, &both, Some(JavaBuildTool::Maven));
    assert_ne!(maven["planSha256"], plan["planSha256"]);
    // An override naming a tool the checkout has no file for is refused.
    let error = java_result(
        TrustProfile::Trusted,
        &[("pom.xml", "x")],
        Some(JavaBuildTool::Gradle),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("--java-build-tool gradle, but the checkout has no root build.gradle"),
        "{error}"
    );
}

/// The restricted profile omits the build files, so nothing is detected or passed, and
/// Maven and Gradle together do not refuse.
#[test]
fn restricted_java_uses_the_written_lsif_java_json() {
    let plan = java(
        TrustProfile::Restricted,
        &[("pom.xml", "x"), ("build.gradle", "y")],
        None,
    );
    let setting = setting(&plan, "java.buildTool");
    assert_eq!(setting["value"], "SCIP");
    assert_eq!(setting["source"], "default");
    assert_eq!(arg(&plan, "--build-tool"), None);
}

#[test]
fn python_and_java_overrides_are_validated() {
    let ok = |overrides: Overrides, producer: &str, trust: TrustProfile| {
        overrides.validate(producer, trust).unwrap();
    };
    ok(
        Overrides {
            python_project_name: Some("p".into()),
            python_project_version: Some("1.0".into()),
            ..Overrides::default()
        },
        "scip-python",
        TrustProfile::Restricted,
    );
    ok(
        Overrides {
            java_build_tool: Some(JavaBuildTool::Maven),
            ..Overrides::default()
        },
        "scip-java",
        TrustProfile::Trusted,
    );
    for (overrides, producer, trust) in [
        (
            Overrides {
                python_project_name: Some("p".into()),
                ..Overrides::default()
            },
            "scip-java",
            TrustProfile::Trusted,
        ),
        (
            Overrides {
                python_project_name: Some("a b".into()),
                ..Overrides::default()
            },
            "scip-python",
            TrustProfile::Trusted,
        ),
        (
            Overrides {
                python_project_version: Some("one".into()),
                ..Overrides::default()
            },
            "scip-python",
            TrustProfile::Trusted,
        ),
        (
            Overrides {
                java_build_tool: Some(JavaBuildTool::Gradle),
                ..Overrides::default()
            },
            "scip-java",
            TrustProfile::Restricted,
        ),
        (
            Overrides {
                java_build_tool: Some(JavaBuildTool::Gradle),
                ..Overrides::default()
            },
            "scip-python",
            TrustProfile::Trusted,
        ),
    ] {
        let error = overrides.validate(producer, trust).unwrap_err().to_string();
        assert!(error.starts_with("invalid_request"), "{error}");
    }
    let parsed: Overrides = serde_json::from_value(json!({"javaBuildTool": "gradle"})).unwrap();
    assert_eq!(parsed.java_build_tool, Some(JavaBuildTool::Gradle));
    assert!(serde_json::from_value::<Overrides>(json!({"javaBuildTool": "Gradle"})).is_err());
}

// The rest of #717: TypeScript project references, setup.cfg and lsif-java.json.

fn typescript(files: &[(&str, &str)], project: Option<TsProject>) -> anyhow::Result<Value> {
    Ok(planned(
        ProducerName::ScipTypescript,
        files,
        &Overrides {
            typescript_project: project,
            ..Overrides::default()
        },
    )?
    .0)
}

#[track_caller]
fn typescript_refusal(files: &[(&str, &str)]) -> String {
    let error = typescript(files, None).unwrap_err().to_string();
    assert!(error.starts_with(CONFIG_UNRESOLVED), "{error}");
    error
}

const REFERENCING_ROOT: &str = r#"{
  // JSONC, as TypeScript reads it.
  "files": [],
  "references": [{"path": "./packages/a"}, {"path": "packages/b/tsconfig.json"}]
}"#;

#[test]
fn typescript_project_references_are_followed_listed_and_hashed() {
    let files = [
        ("tsconfig.json", REFERENCING_ROOT),
        (
            "packages/a/tsconfig.json",
            r#"{"extends": "../../tsconfig.base.json", "references": [{"path": "../b"}, {"path": "../c"}]}"#,
        ),
        ("packages/b/tsconfig.json", "{}"),
        (
            "packages/c/tsconfig.json",
            r#"{"references": [{"path": "../.."}]}"#,
        ),
    ];
    let plan = typescript(&files, None).unwrap();
    let references = setting(&plan, "typescript.projectReferences");
    assert_eq!(
        references["value"],
        json!([
            "packages/a/tsconfig.json",
            "packages/b/tsconfig.json",
            "packages/c/tsconfig.json"
        ])
    );
    assert_eq!(references["source"], "project");
    let from: Vec<&str> = references["from"]
        .as_array()
        .unwrap()
        .iter()
        .map(|origin| origin["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        from,
        [
            "tsconfig.json",
            "packages/a/tsconfig.json",
            "packages/b/tsconfig.json",
            "packages/c/tsconfig.json"
        ]
    );
    // A referenced config is in the worktree and the plan: editing it changes the hash.
    let mut edited = files;
    edited[2].1 = r#"{"compilerOptions": {"strict": true}}"#;
    let changed = typescript(&edited, None).unwrap();
    assert_ne!(plan["planSha256"], changed["planSha256"]);
    assert!(
        plan["inputs"]["config"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "packages/b/tsconfig.json"),
        "{}",
        plan["inputs"]
    );
}

#[test]
fn typescript_without_references_or_a_root_tsconfig_follows_none() {
    for (files, detail) in [
        (
            &[("tsconfig.json", "{}"), ("packages/a/tsconfig.json", "{}")][..],
            "the root tsconfig.json has no references",
        ),
        (&[][..], "the generated tsconfig.json has no references"),
        (
            &[("jsconfig.json", r#"{"references": [{"path": "./x"}]}"#)][..],
            "references are not inherited through extends",
        ),
    ] {
        let plan = typescript(files, None).unwrap();
        let references = setting(&plan, "typescript.projectReferences");
        assert_eq!(references["value"], json!([]), "{files:?}");
        assert_eq!(references["source"], "default");
        assert!(
            references["detail"].as_str().unwrap().contains(detail),
            "{references}"
        );
    }
}

/// `--ts-project generated` replaces the root config, and with it every reference: a broken
/// reference no longer refuses.
#[test]
fn the_ts_project_override_replaces_the_references() {
    let files = [(
        "tsconfig.json",
        r#"{"references": [{"path": "./missing"}]}"#,
    )];
    typescript_refusal(&files);
    let plan = typescript(&files, Some(TsProject::Generated)).unwrap();
    let references = setting(&plan, "typescript.projectReferences");
    assert_eq!(references["value"], json!([]));
    assert_eq!(references["source"], "default");
}

#[test]
fn unfollowable_project_references_are_typed_refusals() {
    let root = |references: &str| format!(r#"{{"references": {references}}}"#);
    for (files, expected) in [
        (
            vec![(
                "tsconfig.json".to_owned(),
                root(r#"[{"path": "./packages/a"}]"#),
            )],
            "capture retained no packages/a/tsconfig.json",
        ),
        (
            vec![(
                "tsconfig.json".to_owned(),
                root(r#"[{"path": "../outside"}]"#),
            )],
            "outside the checkout",
        ),
        (
            vec![("tsconfig.json".to_owned(), root(r#"[{"path": "/abs/a"}]"#))],
            "outside the checkout",
        ),
        (
            vec![
                (
                    "tsconfig.json".to_owned(),
                    root(r#"[{"path": "./tsconfig.app.json"}]"#),
                ),
                ("tsconfig.app.json".to_owned(), "{}".to_owned()),
            ],
            "a config not named tsconfig.json",
        ),
        (
            vec![("tsconfig.json".to_owned(), root(r#""./a""#))],
            "references is not an array",
        ),
        (
            vec![("tsconfig.json".to_owned(), root(r#"[{"path": 3}]"#))],
            "references is not an array",
        ),
        (
            vec![
                ("tsconfig.json".to_owned(), root(r#"[{"path": "a"}]"#)),
                ("a/tsconfig.json".to_owned(), "[1]".to_owned()),
            ],
            "a/tsconfig.json is not a JSON object",
        ),
        (
            vec![
                ("tsconfig.json".to_owned(), root(r#"[{"path": "a"}]"#)),
                ("a/tsconfig.json".to_owned(), "{".to_owned()),
            ],
            "a/tsconfig.json is not a JSON object",
        ),
        (
            vec![
                ("tsconfig.json".to_owned(), root(r#"[{"path": "a"}]"#)),
                (
                    "a/tsconfig.json".to_owned(),
                    r#"{"extends": "@tsconfig/node20/tsconfig.json"}"#.to_owned(),
                ),
            ],
            "extends names a package",
        ),
        (
            vec![
                ("tsconfig.json".to_owned(), root(r#"[{"path": "a"}]"#)),
                (
                    "a/tsconfig.json".to_owned(),
                    String::from_utf8(crate::skipped_config::PLACEHOLDER.to_vec()).unwrap(),
                ),
            ],
            "over the config input cap",
        ),
    ] {
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let error = typescript_refusal(&files);
        assert!(error.contains(expected), "{expected}: {error}");
    }
}

#[test]
fn project_references_are_bounded() {
    let count = super::tsconfig::MAX_REFERENCES + 1;
    let references: Vec<Value> = (0..count)
        .map(|i| json!({"path": format!("p{i}")}))
        .collect();
    let root = json!({ "references": references }).to_string();
    let mut files = vec![("tsconfig.json".to_owned(), root)];
    files.extend((0..count).map(|i| (format!("p{i}/tsconfig.json"), "{}".to_owned())));
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    assert!(typescript_refusal(&files).contains("more than 64 configs"));
    // At the bound, it is followed.
    let at_bound = &files[..files.len() - 1];
    let mut kept: Vec<(&str, String)> = at_bound
        .iter()
        .map(|(p, t)| (*p, (*t).to_owned()))
        .collect();
    let references: Vec<Value> = (0..count - 1)
        .map(|i| json!({"path": format!("p{i}")}))
        .collect();
    kept[0].1 = json!({ "references": references }).to_string();
    let kept: Vec<(&str, &str)> = kept.iter().map(|(p, t)| (*p, t.as_str())).collect();
    let plan = typescript(&kept, None).unwrap();
    assert_eq!(
        setting(&plan, "typescript.projectReferences")["value"]
            .as_array()
            .unwrap()
            .len(),
        count - 1
    );
}

const SETUP_CFG: &str = "# comment\n[metadata]\nname = fixture-cfg\nversion: 2.0\nclassifiers =\n    Programming Language :: Python\n\n[options]\npackages = find:\n";

#[test]
fn python_project_name_and_version_fall_back_to_setup_cfg() {
    let plan = python(
        TrustProfile::Trusted,
        &[("setup.cfg", SETUP_CFG)],
        &Overrides::default(),
    );
    for (name, value, flag) in [
        ("python.projectName", "fixture-cfg", "--project-name"),
        ("python.projectVersion", "2.0", "--project-version"),
    ] {
        let setting = setting(&plan, name);
        assert_eq!(setting["value"], value);
        assert_eq!(setting["source"], "project");
        assert_eq!(setting["from"][0]["path"], "setup.cfg");
        assert!(setting["detail"].as_str().unwrap().contains("of setup.cfg"));
        assert_eq!(arg(&plan, flag), Some(value));
    }
    // pyproject.toml wins key by key; setup.cfg fills the key it lacks.
    let both = python(
        TrustProfile::Trusted,
        &[
            ("pyproject.toml", "[project]\nname = \"from-pyproject\"\n"),
            ("setup.cfg", SETUP_CFG),
        ],
        &Overrides::default(),
    );
    assert_eq!(
        setting(&both, "python.projectName")["value"],
        "from-pyproject"
    );
    assert_eq!(
        setting(&both, "python.projectName")["from"][0]["path"],
        "pyproject.toml"
    );
    assert_eq!(setting(&both, "python.projectVersion")["value"], "2.0");
    assert_eq!(
        setting(&both, "python.projectVersion")["from"][0]["path"],
        "setup.cfg"
    );
    let none = python(TrustProfile::Trusted, &[], &Overrides::default());
    assert_ne!(plan["planSha256"], none["planSha256"]);
}

#[test]
fn setup_cfg_directives_and_the_restricted_profile_derive_nothing() {
    let directive = python(
        TrustProfile::Trusted,
        &[(
            "setup.cfg",
            "[metadata]\nname = cfg\nversion = attr: cfg.__version__\n",
        )],
        &Overrides::default(),
    );
    let version = setting(&directive, "python.projectVersion");
    assert_eq!(version["value"], "0");
    assert_eq!(version["source"], "default");
    assert_eq!(setting(&directive, "python.projectName")["value"], "cfg");
    // The restricted profile reads neither file: a malformed setup.cfg does not refuse.
    let restricted = python(
        TrustProfile::Restricted,
        &[("setup.cfg", "not ini")],
        &Overrides::default(),
    );
    let name = setting(&restricted, "python.projectName");
    assert_eq!(name["source"], "default");
    assert!(name["detail"].as_str().unwrap().contains("restricted"));
}

#[test]
fn python_overrides_replace_setup_cfg() {
    let plan = python(
        TrustProfile::Trusted,
        &[("setup.cfg", "not ini")],
        &Overrides {
            python_project_name: Some("given".into()),
            python_project_version: Some("9".into()),
            ..Overrides::default()
        },
    );
    assert_eq!(setting(&plan, "python.projectName")["source"], "override");
    assert_eq!(arg(&plan, "--project-name"), Some("given"));
}

#[test]
fn malformed_setup_cfg_is_a_typed_refusal() {
    for (text, expected) in [
        (
            "[metadata]\nname fixture\n",
            "line 2 is not a section, key or comment",
        ),
        (
            "name = x\n[metadata]\n",
            "line 1 is a key before any section",
        ),
        (
            "[metadata]\nname = a\n[metadata]\n",
            "[metadata] appears twice",
        ),
        (
            "[metadata]\nname = a\nName = b\n",
            "[metadata] name appears twice",
        ),
        ("[metadata]\n = a\n", "empty key"),
        (
            "[metadata]\nname = not a name\n",
            "is not a Python project name",
        ),
        (
            "[metadata]\nname = a\nversion = one\n",
            "is not a PEP 440 version",
        ),
        (
            "[metadata]\nname =\n  two\n  lines\n",
            "is not a Python project name",
        ),
    ] {
        let error = python_refusal(&[("setup.cfg", text)]);
        assert!(error.contains(expected), "{text:?}: {error}");
    }
    let error = planned_as(
        ProducerName::ScipPython,
        TrustProfile::Trusted,
        &[],
        &Overrides::default(),
    );
    assert!(error.is_ok());
    let row = profiles::row(
        Target::Real(ProducerName::ScipPython),
        TrustProfile::Trusted,
    )
    .unwrap();
    let mut rules = profiles::rules(&row);
    let not_utf8 = RetainedInputs {
        config: vec![RootInput {
            path: "setup.cfg".into(),
            bytes: vec![b'[', 0xff, b']'],
        }],
        dependencies: vec![],
    };
    let error = settings::python(
        &mut rules,
        &not_utf8,
        TrustProfile::Trusted,
        &Overrides::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("setup.cfg is not UTF-8"), "{error}");
}

#[test]
fn lsif_java_json_selects_scip_java_s_own_build_tool() {
    let plan = java(TrustProfile::Trusted, &[("lsif-java.json", "{}")], None);
    let setting = setting(&plan, "java.buildTool");
    assert_eq!(setting["value"], "SCIP");
    assert_eq!(setting["source"], "project");
    assert_eq!(setting["from"][0]["path"], "lsif-java.json");
    assert_eq!(arg(&plan, "--build-tool"), Some("SCIP"));
    // With a Maven or Gradle file it is contradictory, as scip-java says; an override picks.
    for (file, tool) in [("pom.xml", "Maven"), ("build.gradle", "Gradle")] {
        let both = [("lsif-java.json", "{}"), (file, "x")];
        let error = java_result(TrustProfile::Trusted, &both, None)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with(CONFIG_UNRESOLVED), "{error}");
        assert!(
            error.contains(&format!("select SCIP and {tool}")),
            "{error}"
        );
        let scip = java(TrustProfile::Trusted, &both, Some(JavaBuildTool::Scip));
        assert_eq!(arg(&scip, "--build-tool"), Some("SCIP"));
    }
    let error = java_result(
        TrustProfile::Trusted,
        &[("pom.xml", "x")],
        Some(JavaBuildTool::Scip),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("has no root lsif-java.json"), "{error}");
}

/// The restricted profile writes its own `lsif-java.json` and leaves the checkout's out, with
/// the nested module build files, and scip-python leaves `pyrightconfig.json` out.
#[test]
fn restricted_profiles_omit_the_new_inputs() {
    let omitted = |plan: &Value| -> Vec<String> {
        plan["inputs"]["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap().to_owned())
            .collect()
    };
    let restricted_java = java(
        TrustProfile::Restricted,
        &[
            ("lsif-java.json", "{}"),
            ("app/pom.xml", "<project/>"),
            ("lib/build.gradle.kts", ""),
        ],
        None,
    );
    assert_eq!(
        omitted(&restricted_java),
        ["app/pom.xml", "lib/build.gradle.kts", "lsif-java.json"]
    );
    let restricted_python = python(
        TrustProfile::Restricted,
        &[("pyrightconfig.json", "{}"), ("setup.cfg", SETUP_CFG)],
        &Overrides::default(),
    );
    assert_eq!(omitted(&restricted_python), ["pyrightconfig.json"]);
    let trusted = python(
        TrustProfile::Trusted,
        &[("pyrightconfig.json", "{}")],
        &Overrides::default(),
    );
    assert!(omitted(&trusted).is_empty());
}
