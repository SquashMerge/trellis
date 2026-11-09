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
}

impl Overrides {
    /// Each override is bounded and applies to one producer only.
    pub fn validate(&self, producer: &str) -> Result<()> {
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

/// A Cargo feature name, optionally `dep/feature` or `dep?/feature`. Nothing a cargo
/// `--features` list or a JSON config could read differently.
pub fn valid_feature(feature: &str) -> bool {
    let part = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'.'))
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

fn parse_manifest(input: &RootInput) -> Result<toml::Table> {
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
/// optional dependency.
fn declares(package: &toml::Table, feature: &str) -> bool {
    if package
        .get("features")
        .and_then(toml::Value::as_table)
        .is_some_and(|features| features.contains_key(feature))
    {
        return true;
    }
    let optional = |deps: Option<&toml::Value>| {
        deps.and_then(toml::Value::as_table)
            .and_then(|deps| deps.get(feature))
            .and_then(toml::Value::as_table)
            .and_then(|dep| dep.get("optional"))
            .and_then(toml::Value::as_bool)
            == Some(true)
    };
    optional(package.get("dependencies"))
        || package
            .get("target")
            .and_then(toml::Value::as_table)
            .is_some_and(|targets| {
                targets
                    .values()
                    .any(|target| optional(target.get("dependencies")))
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
        let root_table = parse_manifest(root)?;
        let (dirs, members_detail) = members(&root_table, &manifests)?;
        let mut packages = Vec::new();
        for dir in &dirs {
            let input = manifests[dir];
            let manifest = if dir.is_empty() {
                root_table.clone()
            } else {
                parse_manifest(input)?
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
