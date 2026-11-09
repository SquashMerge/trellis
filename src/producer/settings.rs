//! Producer settings read from the project's own configuration (#717). A plan derives each
//! setting from the retained root inputs, read-only, bounded and executing nothing, and lists
//! it with its source; an explicit override on `producer plan` and `producer run` replaces the
//! derived value. Unparseable or contradictory project config is the typed
//! `producer_config_unresolved` refusal, never a silent fallback.
//!
//! rust-analyzer: `cargo.features` is the union of the `required-features` of every workspace
//! member's `[[bin]]`, `[[test]]`, `[[bench]]` and `[[example]]` targets, so the code they gate
//! is analyzed. Nothing else is turned on: never all features, so a platform feature such as
//! `metal` stays off. The workspace members are recorded with it.
//!
//! scip-python: `--project-name` and `--project-version` from the root `pyproject.toml` the
//! way scip-python 0.6.6 infers them itself (`[project]`, then `[tool.poetry]` for a key
//! `[project]` lacks), in the trusted profile, whose worktree holds that file.
//!
//! scip-java: the build tool scip-java 0.12.3 detects from the retained root build files, in
//! the trusted profile; Maven and Gradle together are contradictory.

use super::TrustProfile;
use super::profiles::{Generated, Rules};
use super::tsconfig::CONFIG_UNRESOLVED;
use super::worktree::{RetainedInputs, RootInput};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The rust-analyzer config a run gets when it enables features, under the run directory.
pub const RUST_ANALYZER_CONFIG: &str = "producer-config/rust-analyzer.json";
/// The most features one plan enables, derived or overridden.
pub const MAX_FEATURES: usize = 256;
const MAX_FEATURE_BYTES: usize = 128;
const ROOT_MANIFEST: &str = "Cargo.toml";
const TARGET_TABLES: [&str; 4] = ["bin", "test", "bench", "example"];
/// scip-python's `--project-name` when nothing else sets it.
pub const PYTHON_PROJECT_NAME: &str = "trellis-workspace";
/// scip-python's `--project-version` when nothing else sets it; without one scip-python runs
/// `git`, which is not on the child's `PATH`.
pub const PYTHON_PROJECT_VERSION: &str = "0";
const PYPROJECT: &str = "pyproject.toml";
const MAX_PYTHON_VALUE_BYTES: usize = 256;
/// The root files by which scip-java 0.12.3 detects Maven. It detects Gradle by
/// `build.gradle`, `build.gradle.kts` or `settings.gradle`; neither `settings.gradle.kts` nor
/// `gradle.properties` alone selects Gradle (probed against the pinned launcher).
const MAVEN_FILES: [&str; 1] = ["pom.xml"];
const GRADLE_FILES: [&str; 3] = ["build.gradle", "build.gradle.kts", "settings.gradle"];

/// A build tool scip-java runs in the trusted profile, by the name its `--build-tool` takes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JavaBuildTool {
    Maven,
    Gradle,
}

impl JavaBuildTool {
    /// scip-java's own name for the tool.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Maven => "Maven",
            Self::Gradle => "Gradle",
        }
    }

    fn files(self) -> &'static [&'static str] {
        match self {
            Self::Maven => &MAVEN_FILES,
            Self::Gradle => &GRADLE_FILES,
        }
    }
}

/// The scip-typescript project config a run uses.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TsProject {
    #[serde(rename = "tsconfig.json")]
    Tsconfig,
    #[serde(rename = "jsconfig.json")]
    Jsconfig,
    /// The Trellis-generated tsconfig (#652), whatever the checkout holds.
    #[serde(rename = "generated")]
    Generated,
}

impl TsProject {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tsconfig => "tsconfig.json",
            Self::Jsconfig => "jsconfig.json",
            Self::Generated => "generated",
        }
    }
}

/// Explicit settings from `producer plan` and `producer run`: each replaces the derived value.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Overrides {
    pub cargo_features: Option<Vec<String>>,
    pub typescript_project: Option<TsProject>,
    pub python_project_name: Option<String>,
    pub python_project_version: Option<String>,
    pub java_build_tool: Option<JavaBuildTool>,
}

impl Overrides {
    /// Each override is bounded and applies to one producer only; the build tool applies to
    /// the trusted profile only, since the restricted one omits the build files.
    pub fn validate(&self, producer: &str, trust: TrustProfile) -> Result<()> {
        if let Some(features) = &self.cargo_features {
            ensure!(
                producer == super::ProducerName::RustAnalyzer.as_str(),
                "invalid_request: cargoFeatures applies to rust-analyzer only"
            );
            ensure!(
                features.len() <= MAX_FEATURES && features.iter().all(|f| valid_feature(f)),
                "invalid_request: cargoFeatures"
            );
        }
        if self.typescript_project.is_some() {
            ensure!(
                producer == super::ProducerName::ScipTypescript.as_str(),
                "invalid_request: typescriptProject applies to scip-typescript only"
            );
        }
        let python = producer == super::ProducerName::ScipPython.as_str();
        if let Some(name) = &self.python_project_name {
            ensure!(
                python,
                "invalid_request: pythonProjectName applies to scip-python only"
            );
            ensure!(
                valid_python_name(name),
                "invalid_request: pythonProjectName"
            );
        }
        if let Some(version) = &self.python_project_version {
            ensure!(
                python,
                "invalid_request: pythonProjectVersion applies to scip-python only"
            );
            ensure!(
                valid_python_version(version),
                "invalid_request: pythonProjectVersion"
            );
        }
        if self.java_build_tool.is_some() {
            ensure!(
                producer == super::ProducerName::ScipJava.as_str(),
                "invalid_request: javaBuildTool applies to scip-java only"
            );
            ensure!(
                trust == TrustProfile::Trusted,
                "invalid_request: javaBuildTool applies to the trusted profile only"
            );
        }
        Ok(())
    }
}

/// Where a setting's value came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// Read from the project's own files, listed in `from`.
    Project,
    /// Given explicitly on the command.
    Override,
    /// Nothing in the project sets it.
    Default,
}

/// A project file a setting was read from, with its retained content hash.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Origin {
    pub path: String,
    pub sha256: String,
}

impl Origin {
    pub fn of(input: &RootInput) -> Self {
        Self {
            path: input.path.clone(),
            sha256: input.sha256(),
        }
    }
}

/// One effective setting, as the plan lists it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Setting {
    pub name: &'static str,
    pub value: Value,
    pub source: Source,
    pub from: Vec<Origin>,
    pub detail: String,
}

fn unresolved(reason: impl std::fmt::Display) -> anyhow::Error {
    let reason: String = reason.to_string().chars().take(400).collect();
    anyhow!("{CONFIG_UNRESOLVED}: {reason}")
}

/// A Cargo feature name, optionally `dep/feature` or `dep?/feature`, by Cargo's own rule
/// (`validate_feature_name`, Cargo 1.99): the first character is a Unicode XID start
/// character, `_` or an ASCII digit, and each other one is a Unicode XID continue character,
/// `-`, `+` or `.`. No such name holds a control character, whitespace, `,` or `/`, so the
/// `pkg/feature` form, a cargo `--features` list and the JSON config read it the same way.
/// The dependency part of `dep/feature` is checked with the same rule.
pub fn valid_feature(feature: &str) -> bool {
    let part = |part: &str| {
        let mut chars = part.chars();
        chars.next().is_some_and(|first| {
            unicode_ident::is_xid_start(first) || first == '_' || first.is_ascii_digit()
        }) && chars.all(|ch| unicode_ident::is_xid_continue(ch) || matches!(ch, '-' | '+' | '.'))
    };
    feature.len() <= MAX_FEATURE_BYTES
        && match feature.split_once('/') {
            None => part(feature),
            Some((dep, name)) => part(dep.strip_suffix('?').unwrap_or(dep)) && part(name),
        }
}

/// A Cargo manifest's directory (`""` for the root), when `path` names one.
fn manifest_dir(path: &str) -> Option<&str> {
    match path.strip_suffix(ROOT_MANIFEST)? {
        "" => Some(""),
        dir => dir.strip_suffix('/'),
    }
}

fn parse_toml(input: &RootInput) -> Result<toml::Table> {
    let path = &input.path;
    if input.bytes == crate::skipped_config::PLACEHOLDER {
        return Err(unresolved(format!(
            "{path} is over the config input cap and was not read"
        )));
    }
    let text = std::str::from_utf8(&input.bytes)
        .map_err(|_| unresolved(format!("{path} is not UTF-8")))?;
    toml::from_str(text).map_err(|_| unresolved(format!("{path} is not valid TOML")))
}

fn table<'a>(parent: &'a toml::Table, key: &str, path: &str) -> Result<Option<&'a toml::Table>> {
    match parent.get(key) {
        None => Ok(None),
        Some(toml::Value::Table(table)) => Ok(Some(table)),
        Some(_) => Err(unresolved(format!("{path}: {key} is not a table"))),
    }
}

fn strings(value: Option<&toml::Value>, what: &str, path: &str) -> Result<Vec<String>> {
    let invalid = || unresolved(format!("{path}: {what} is not an array of strings"));
    match value {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or_else(invalid))
            .collect(),
        Some(_) => Err(invalid()),
    }
}

/// `pattern` as a workspace-relative directory: `./` and a trailing `/` dropped; `None` when it
/// leaves the checkout, which holds no such member.
fn member_pattern(pattern: &str) -> Option<String> {
    let trimmed = pattern.trim_start_matches("./").trim_end_matches('/');
    let outside = trimmed.starts_with('/') || trimmed.split('/').any(|part| part == "..");
    (!outside).then(|| trimmed.to_owned())
}

/// The package manifests of the workspace, by directory: the root package, and each nested
/// manifest that `[workspace] members` matches and `exclude` does not.
fn members<'a>(
    root: &toml::Table,
    manifests: &BTreeMap<&'a str, &'a RootInput>,
) -> Result<(Vec<&'a str>, String)> {
    let mut members = Vec::new();
    if root.contains_key("package") {
        members.push("");
    }
    let Some(workspace) = table(root, "workspace", ROOT_MANIFEST)? else {
        return Ok((
            members,
            "the root package; the root Cargo.toml has no [workspace]".into(),
        ));
    };
    let patterns = strings(
        workspace.get("members"),
        "[workspace] members",
        ROOT_MANIFEST,
    )?;
    let exclude = strings(
        workspace.get("exclude"),
        "[workspace] exclude",
        ROOT_MANIFEST,
    )?;
    let mut set = globset::GlobSetBuilder::new();
    for pattern in patterns
        .iter()
        .filter_map(|pattern| member_pattern(pattern))
    {
        let glob = globset::GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()
            .map_err(|_| {
                unresolved(format!(
                    "Cargo.toml: [workspace] member {pattern:?} is not a valid glob"
                ))
            })?;
        set.add(glob);
    }
    let set = set
        .build()
        .map_err(|_| unresolved("Cargo.toml: [workspace] members"))?;
    let excluded: Vec<String> = exclude
        .iter()
        .filter_map(|path| member_pattern(path))
        .collect();
    for dir in manifests.keys().filter(|dir| !dir.is_empty()) {
        let out = excluded
            .iter()
            .any(|path| *dir == path.as_str() || dir.starts_with(&format!("{path}/")));
        if set.is_match(dir) && !out {
            members.push(*dir);
        }
    }
    let detail = format!(
        "[workspace] members {patterns:?}{} of the root Cargo.toml, matched against the retained manifests",
        if exclude.is_empty() {
            String::new()
        } else {
            format!(" without exclude {exclude:?}")
        }
    );
    Ok((members, detail))
}

/// Whether `package` declares `feature`: a `[features]` key, or the implicit feature of an
/// optional dependency. As in Cargo, a dependency has no implicit feature once any `[features]`
/// value names it as `dep:<name>`.
fn declares(package: &toml::Table, feature: &str) -> bool {
    let features = package.get("features").and_then(toml::Value::as_table);
    if features.is_some_and(|features| features.contains_key(feature)) {
        return true;
    }
    let explicit = format!("dep:{feature}");
    let suppressed = features.is_some_and(|features| {
        features.values().any(|values| {
            values
                .as_array()
                .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(&explicit)))
        })
    });
    if suppressed {
        return false;
    }
    let optional = |deps: Option<&toml::Value>| {
        deps.and_then(toml::Value::as_table)
            .and_then(|deps| deps.get(feature))
            .and_then(toml::Value::as_table)
            .and_then(|dep| dep.get("optional"))
            .and_then(toml::Value::as_bool)
            == Some(true)
    };
    let kinds = |table: &toml::Table| {
        optional(table.get("dependencies")) || optional(table.get("build-dependencies"))
    };
    kinds(package)
        || package
            .get("target")
            .and_then(toml::Value::as_table)
            .is_some_and(|targets| {
                targets
                    .values()
                    .filter_map(toml::Value::as_table)
                    .any(kinds)
            })
}

/// One member package: its name and the features its targets require, each with the target
/// that requires it.
fn required_features(
    input: &RootInput,
    manifest: &toml::Table,
) -> Result<(String, Vec<(String, String)>)> {
    let path = input.path.as_str();
    let package = table(manifest, "package", path)?
        .ok_or_else(|| unresolved(format!("{path} is a workspace member without [package]")))?;
    let name = package
        .get("name")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| unresolved(format!("{path}: [package] name is not a string")))?
        .to_owned();
    let mut required = Vec::new();
    for kind in TARGET_TABLES {
        let targets = match manifest.get(kind) {
            None => continue,
            Some(toml::Value::Array(targets)) => targets,
            Some(_) => {
                return Err(unresolved(format!(
                    "{path}: {kind} is not an array of tables"
                )));
            }
        };
        for target in targets {
            let target = target
                .as_table()
                .ok_or_else(|| unresolved(format!("{path}: {kind} is not an array of tables")))?;
            let label = match target.get("name").and_then(toml::Value::as_str) {
                Some(target) => format!("[[{kind}]] {target}"),
                None => format!("[[{kind}]]"),
            };
            for feature in strings(
                target.get("required-features"),
                &format!("{label} required-features"),
                path,
            )? {
                if !valid_feature(&feature) {
                    return Err(unresolved(format!(
                        "{path}: {label} requires {:?}, which is not a Cargo feature name",
                        feature.chars().take(64).collect::<String>()
                    )));
                }
                if !feature.contains('/') && !declares(manifest, &feature) {
                    return Err(unresolved(format!(
                        "{path}: {label} requires feature {feature:?}, which [features] does not declare"
                    )));
                }
                required.push((feature, format!("{label} in {path}")));
            }
        }
    }
    Ok((name, required))
}

/// rust-analyzer's settings: `cargo.features` and the workspace members. Enabled features are
/// written to the run's rust-analyzer config, which the command passes with `--config-path`.
pub fn rust(
    rules: &mut Rules,
    inputs: &RetainedInputs,
    overridden: Option<&[String]>,
) -> Result<Vec<Setting>> {
    let mut manifests: BTreeMap<&str, &RootInput> = BTreeMap::new();
    for input in &inputs.config {
        if let Some(dir) = manifest_dir(&input.path) {
            manifests.entry(dir).or_insert(input);
        }
    }
    let mut settings = Vec::new();
    let mut derived: Option<(Vec<String>, Vec<Origin>, String)> = None;
    if let Some(root) = manifests.get("").copied() {
        let root_table = parse_toml(root)?;
        let (dirs, members_detail) = members(&root_table, &manifests)?;
        let mut packages = Vec::new();
        for dir in &dirs {
            let input = manifests[dir];
            let manifest = if dir.is_empty() {
                root_table.clone()
            } else {
                parse_toml(input)?
            };
            packages.push((input, manifest));
        }
        if overridden.is_none() {
            let single = packages.len() == 1;
            let mut features = BTreeSet::new();
            let mut origins = BTreeMap::new();
            let mut reasons = Vec::new();
            for (input, manifest) in &packages {
                let (name, required) = required_features(input, manifest)?;
                for (feature, reason) in required {
                    let qualified = if single || feature.contains('/') {
                        feature
                    } else {
                        format!("{name}/{feature}")
                    };
                    origins.insert(input.path.clone(), Origin::of(input));
                    reasons.push(format!("{qualified}: required-features of {reason}"));
                    features.insert(qualified);
                    if features.len() > MAX_FEATURES {
                        return Err(unresolved(format!(
                            "the workspace requires more than {MAX_FEATURES} features"
                        )));
                    }
                }
            }
            reasons.dedup();
            let detail = if reasons.is_empty() {
                "no workspace target has required-features".to_owned()
            } else {
                reasons.join("; ")
            };
            derived = Some((
                features.into_iter().collect(),
                origins.into_values().collect(),
                detail,
            ));
        }
        settings.push(Setting {
            name: "cargo.workspaceMembers",
            value: json!(
                dirs.iter()
                    .map(|dir| if dir.is_empty() { "." } else { dir })
                    .collect::<Vec<_>>()
            ),
            source: Source::Project,
            from: vec![Origin::of(root)],
            detail: members_detail,
        });
    }
    let features = match (overridden, derived) {
        (Some(features), _) => {
            let features: BTreeSet<&String> = features.iter().collect();
            Setting {
                name: "cargo.features",
                value: json!(features),
                source: Source::Override,
                from: Vec::new(),
                detail: "given with --cargo-features; it replaces the derived features".into(),
            }
        }
        (None, Some((features, from, detail))) if !features.is_empty() => Setting {
            name: "cargo.features",
            value: json!(features),
            source: Source::Project,
            from,
            detail,
        },
        (None, derived) => Setting {
            name: "cargo.features",
            value: json!([]),
            source: Source::Default,
            from: Vec::new(),
            detail: derived.map_or_else(
                || "no root Cargo.toml; default features only".to_owned(),
                |(_, _, detail)| format!("{detail}; default features only"),
            ),
        },
    };
    let enabled = features.value.as_array().is_some_and(|f| !f.is_empty());
    if enabled {
        rules.generated.push(Generated {
            path: RUST_ANALYZER_CONFIG.into(),
            bytes: serde_json::to_vec(&json!({"cargo": {"features": features.value}}))?,
            default_only: false,
        });
    }
    settings.insert(0, features);
    Ok(settings)
}

/// A Python project name by the core metadata rule (PEP 508): ASCII letters, digits, `.`, `_`
/// and `-`, starting and ending with a letter or digit, at most 256 bytes.
pub fn valid_python_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let edge = |byte: Option<&u8>| byte.is_some_and(u8::is_ascii_alphanumeric);
    name.len() <= MAX_PYTHON_VALUE_BYTES
        && edge(bytes.first())
        && edge(bytes.last())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// One part of a PEP 440 version: every offset it can end at when it starts at `at`.
type VersionPart = fn(&[u8], usize) -> Vec<usize>;

fn digits(bytes: &[u8], at: usize) -> Option<usize> {
    let end = at
        + bytes[at..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
    (end > at).then_some(end)
}

fn separator(bytes: &[u8], at: usize) -> Vec<usize> {
    let mut ends = vec![at];
    if matches!(bytes.get(at), Some(b'-' | b'_' | b'.')) {
        ends.push(at + 1);
    }
    ends
}

/// `[-_.]? label [-_.]? digits?` for any of `labels`, or nothing.
fn labelled(bytes: &[u8], at: usize, labels: &[&str]) -> Vec<usize> {
    let mut ends = vec![at];
    for start in separator(bytes, at) {
        for label in labels {
            if bytes[start..].starts_with(label.as_bytes()) {
                for number in separator(bytes, start + label.len()) {
                    ends.push(number);
                    ends.extend(digits(bytes, number));
                }
            }
        }
    }
    ends
}

const VERSION_PARTS: [VersionPart; 7] = [
    // `v?`
    |bytes, at| {
        let mut ends = vec![at];
        if bytes.get(at) == Some(&b'v') {
            ends.push(at + 1);
        }
        ends
    },
    // `([0-9]+!)?`
    |bytes, at| {
        let mut ends = vec![at];
        if let Some(end) = digits(bytes, at).filter(|end| bytes.get(*end) == Some(&b'!')) {
            ends.push(end + 1);
        }
        ends
    },
    // `[0-9]+(\.[0-9]+)*`
    |bytes, at| {
        let mut ends = Vec::new();
        let mut next = digits(bytes, at);
        while let Some(end) = next {
            ends.push(end);
            next = (bytes.get(end) == Some(&b'.'))
                .then(|| digits(bytes, end + 1))
                .flatten();
        }
        ends
    },
    |bytes, at| {
        labelled(
            bytes,
            at,
            &["alpha", "a", "beta", "b", "preview", "pre", "c", "rc"],
        )
    },
    // `-[0-9]+` or `[-_.]? (post|rev|r) [-_.]? [0-9]*`
    |bytes, at| {
        let mut ends = labelled(bytes, at, &["post", "rev", "r"]);
        if bytes.get(at) == Some(&b'-') {
            ends.extend(digits(bytes, at + 1));
        }
        ends
    },
    |bytes, at| labelled(bytes, at, &["dev"]),
    // `(\+[a-z0-9]+([-_.][a-z0-9]+)*)?`
    |bytes, at| {
        let mut ends = vec![at];
        if bytes.get(at) != Some(&b'+') {
            return ends;
        }
        let segment = |at: usize| {
            let end = at
                + bytes[at..]
                    .iter()
                    .take_while(|b| b.is_ascii_alphanumeric())
                    .count();
            (end > at).then_some(end)
        };
        let mut next = segment(at + 1);
        while let Some(end) = next {
            ends.push(end);
            next = matches!(bytes.get(end), Some(b'-' | b'_' | b'.'))
                .then(|| segment(end + 1))
                .flatten();
        }
        ends
    },
];

/// A PEP 440 version, as `packaging`'s `VERSION_PATTERN` matches it (case-insensitively),
/// without the surrounding whitespace that pattern allows, at most 256 bytes.
pub fn valid_python_version(version: &str) -> bool {
    fn matches(bytes: &[u8], at: usize, parts: &[VersionPart]) -> bool {
        match parts.split_first() {
            None => at == bytes.len(),
            Some((part, rest)) => part(bytes, at)
                .into_iter()
                .any(|end| matches(bytes, end, rest)),
        }
    }
    version.len() <= MAX_PYTHON_VALUE_BYTES
        && version.is_ascii()
        && matches(version.to_ascii_lowercase().as_bytes(), 0, &VERSION_PARTS)
}

/// `key` of the root `pyproject.toml` as scip-python 0.6.6 infers it: `[project]`'s, else
/// `[tool.poetry]`'s when `[project]` has no such key, with the section it came from. Where
/// scip-python silently drops a value that is not a string, this refuses it.
fn pyproject_value(
    manifest: &toml::Table,
    key: &str,
    valid: fn(&str) -> bool,
    what: &str,
) -> Result<Option<(String, &'static str)>> {
    for (label, path) in [
        ("[project]", &["project"][..]),
        ("[tool.poetry]", &["tool", "poetry"][..]),
    ] {
        let mut section = Some(manifest);
        for part in path {
            section = match section {
                Some(parent) => table(parent, part, PYPROJECT)?,
                None => None,
            };
        }
        match section.and_then(|section| section.get(key)) {
            None => {}
            Some(toml::Value::String(value)) if valid(value) => {
                return Ok(Some((value.clone(), label)));
            }
            Some(toml::Value::String(value)) => {
                return Err(unresolved(format!(
                    "{PYPROJECT}: {label} {key} {:?} is not {what}",
                    value.chars().take(64).collect::<String>()
                )));
            }
            Some(_) => {
                return Err(unresolved(format!(
                    "{PYPROJECT}: {label} {key} is not a string"
                )));
            }
        }
    }
    Ok(None)
}

/// One scip-python setting read from `pyproject.toml`.
struct PythonKey<'a> {
    key: &'static str,
    flag: &'static str,
    name: &'static str,
    overridden: Option<&'a String>,
    default: &'static str,
    valid: fn(&str) -> bool,
    what: &'static str,
}

/// scip-python's settings: `python.projectName` and `python.projectVersion`, passed as
/// `--project-name` and `--project-version`. Only the trusted worktree holds the root
/// `pyproject.toml`; the restricted profile omits it, so there it is not read and the
/// defaults apply unless overridden.
pub fn python(
    rules: &mut Rules,
    inputs: &RetainedInputs,
    trust: TrustProfile,
    overrides: &Overrides,
) -> Result<Vec<Setting>> {
    let pyproject = inputs.config.iter().find(|input| input.path == PYPROJECT);
    let needed =
        overrides.python_project_name.is_none() || overrides.python_project_version.is_none();
    let manifest = match pyproject {
        Some(input) if needed && trust == TrustProfile::Trusted => {
            Some((input, parse_toml(input)?))
        }
        _ => None,
    };
    let keys = [
        PythonKey {
            key: "name",
            flag: "--project-name",
            name: "python.projectName",
            overridden: overrides.python_project_name.as_ref(),
            default: PYTHON_PROJECT_NAME,
            valid: valid_python_name,
            what: "a Python project name",
        },
        PythonKey {
            key: "version",
            flag: "--project-version",
            name: "python.projectVersion",
            overridden: overrides.python_project_version.as_ref(),
            default: PYTHON_PROJECT_VERSION,
            valid: valid_python_version,
            what: "a PEP 440 version",
        },
    ];
    let mut settings = Vec::new();
    for PythonKey {
        key,
        flag,
        name,
        overridden,
        default,
        valid,
        what,
    } in keys
    {
        let derived = match (overridden, &manifest) {
            (None, Some((input, manifest))) => pyproject_value(manifest, key, valid, what)?
                .map(|(value, section)| (value, section, *input)),
            _ => None,
        };
        let fallback = || match (trust, pyproject) {
            (TrustProfile::Restricted, Some(_)) => {
                "the restricted profile omits pyproject.toml, so it is not read".to_owned()
            }
            (_, None) => "no root pyproject.toml".to_owned(),
            (_, Some(_)) => format!("pyproject.toml sets no [project] or [tool.poetry] {key}"),
        };
        let setting = match (overridden, derived) {
            (Some(value), _) => Setting {
                name,
                value: json!(value),
                source: Source::Override,
                from: Vec::new(),
                detail: format!("given with --python-project-{key}; it replaces the derived {key}"),
            },
            (None, Some((value, section, input))) => Setting {
                name,
                value: json!(value),
                source: Source::Project,
                from: vec![Origin::of(input)],
                detail: format!("{section} {key} of pyproject.toml"),
            },
            (None, None) => Setting {
                name,
                value: json!(default),
                source: Source::Default,
                from: Vec::new(),
                detail: fallback(),
            },
        };
        let value = setting.value.as_str().unwrap_or(default).to_owned();
        rules.args.push((flag, value));
        settings.push(setting);
    }
    Ok(settings)
}

/// scip-java's `java.buildTool`. The trusted profile passes it as `--build-tool`; the
/// restricted profile omits the build files, so scip-java reads the Trellis-written
/// `lsif-java.json` and nothing is passed.
pub fn java(
    rules: &mut Rules,
    inputs: &RetainedInputs,
    trust: TrustProfile,
    overridden: Option<JavaBuildTool>,
) -> Result<Vec<Setting>> {
    let name = "java.buildTool";
    if trust == TrustProfile::Restricted {
        return Ok(vec![Setting {
            name,
            value: json!("SCIP"),
            source: Source::Default,
            from: Vec::new(),
            detail: "the restricted profile omits the build files, so scip-java reads the Trellis-written lsif-java.json".into(),
        }]);
    }
    let present = |tool: JavaBuildTool| -> Vec<&RootInput> {
        inputs
            .config
            .iter()
            .filter(|input| tool.files().contains(&input.path.as_str()))
            .collect()
    };
    let detected: Vec<(JavaBuildTool, Vec<&RootInput>)> =
        [JavaBuildTool::Maven, JavaBuildTool::Gradle]
            .into_iter()
            .map(|tool| (tool, present(tool)))
            .filter(|(_, files)| !files.is_empty())
            .collect();
    let paths = |files: &[&RootInput]| {
        files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let setting = match (overridden, detected.as_slice()) {
        (Some(tool), _) => {
            if present(tool).is_empty() {
                return Err(unresolved(format!(
                    "--java-build-tool {}, but the checkout has no root {}",
                    tool.as_str().to_ascii_lowercase(),
                    tool.files().join(" or ")
                )));
            }
            Setting {
                name,
                value: json!(tool.as_str()),
                source: Source::Override,
                from: Vec::new(),
                detail: "given with --java-build-tool; it replaces the detected build tool".into(),
            }
        }
        (None, []) => {
            return Ok(vec![Setting {
                name,
                value: Value::Null,
                source: Source::Default,
                from: Vec::new(),
                detail: "no root pom.xml, build.gradle, build.gradle.kts or settings.gradle, so scip-java detects no build tool".into(),
            }]);
        }
        (None, [(tool, files)]) => Setting {
            name,
            value: json!(tool.as_str()),
            source: Source::Project,
            from: files.iter().map(|file| Origin::of(file)).collect(),
            detail: format!("{} selects {}", paths(files), tool.as_str()),
        },
        (None, detected) => {
            let all: Vec<&RootInput> = detected
                .iter()
                .flat_map(|(_, files)| files.iter().copied())
                .collect();
            return Err(unresolved(format!(
                "{} select both Maven and Gradle, and scip-java will not choose; pass --java-build-tool maven or gradle",
                paths(&all)
            )));
        }
    };
    if let Some(tool) = setting.value.as_str() {
        rules.args.push(("--build-tool", tool.to_owned()));
    }
    Ok(vec![setting])
}
