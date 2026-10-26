//! Package-level evidence only. This does not inspect installed libraries or bind members.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct ManifestInput {
    pub source_set_id: String,
    pub native_revision_id: String,
    pub dependency_hash: String,
    pub path: String,
    pub content_hash: Option<String>,
    pub bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalIdentity {
    pub kind: ExternalKind,
    pub source_set_id: String,
    pub language: String,
    pub package: String,
    pub standard_library: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ExternalKind {
    #[serde(rename = "externalPackage")]
    ExternalPackage,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalPackage {
    pub identity: ExternalIdentity,
    pub ecosystem: &'static str,
    pub evidence: Vec<ExternalEvidence>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ExternalEvidence {
    Manifest {
        source_set_id: String,
        native_revision_id: String,
        dependency_hash: String,
        path: String,
        content_hash: String,
        import_root: String,
        section: String,
        key: String,
        version_constraint: Option<String>,
    },
    StandardLibraryCatalog {
        catalog: &'static str,
        entry: String,
    },
}

/// Canonical selector spellings, not proof that a package or library is available.
pub fn valid_package(language: &str, package: &str, standard_library: bool) -> bool {
    if standard_library {
        return match language {
            "rust" => matches!(package, "std" | "core" | "alloc"),
            "java" => matches!(package, "java" | "javax"),
            "python" => identifier(package),
            "javascript" => npm_part(package) && !package.starts_with('@'),
            _ => false,
        };
    }
    match language {
        "rust" => {
            package
                .as_bytes()
                .first()
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
                && package
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        }
        "java" => {
            let mut parts = package.split(':');
            let group = parts.next().unwrap_or_default();
            let artifact = parts.next().unwrap_or_default();
            parts.next().is_none() && coordinate(group) && coordinate(artifact)
        }
        "python" => {
            !package.is_empty()
                && package
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && package
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && package
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                && !package.contains("--")
        }
        "javascript" => match package.strip_prefix('@') {
            Some(scoped) => scoped
                .split_once('/')
                .is_some_and(|(scope, name)| npm_part(scope) && npm_part(name)),
            None => npm_part(package),
        },
        _ => false,
    }
}
fn identifier(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
fn coordinate(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}
fn npm_part(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(['.', '_'])
        && value.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        })
}

#[derive(Default)]
pub struct RustPackages {
    bindings: BTreeMap<(String, String), (String, Option<ExternalPackage>)>,
    roots: BTreeMap<String, (bool, bool)>,
}
impl RustPackages {
    pub fn new(
        manifests: &[ManifestInput],
        work: &mut usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut result = Self::default();
        let mut seen = std::collections::BTreeSet::new();
        for manifest in manifests {
            super::owner_step(work, check)?;
            ensure!(
                seen.insert((&manifest.source_set_id, &manifest.path)),
                "store_unavailable: duplicate architecture manifest"
            );
            if manifest.path != "Cargo.toml" {
                continue;
            }
            ensure!(
                manifest.bytes.is_some() == manifest.content_hash.is_some(),
                "store_unavailable: architecture manifest content"
            );
            let Some(bytes) = &manifest.bytes else {
                continue;
            };
            // Parsing cost and all subsequent fan-out share the existing request work budget.
            *work = work.saturating_add(bytes.len());
            ensure!(
                *work <= super::OWNER_WORK,
                "bounded_work: reference manifest parsing"
            );
            check()?;
            let Ok(text) = std::str::from_utf8(bytes) else {
                continue;
            };
            let parsed = toml::from_str::<toml::Value>(text);
            check()?;
            let Ok(value) = parsed else { continue };
            let Some(table) = value.as_table() else {
                continue;
            };
            let Some(roots) = conventional_roots(table) else {
                continue;
            };
            result.roots.insert(manifest.source_set_id.clone(), roots);
            for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(dependencies) = table.get(section).and_then(toml::Value::as_table) {
                    result.section(manifest, section, dependencies, work, check)?;
                }
            }
            if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
                for (target, value) in targets {
                    super::owner_step(work, check)?;
                    let Some(target_table) = value.as_table() else {
                        continue;
                    };
                    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                        if let Some(dependencies) =
                            target_table.get(section).and_then(toml::Value::as_table)
                        {
                            result.section(
                                manifest,
                                &format!("target.{target}.{section}"),
                                dependencies,
                                work,
                                check,
                            )?;
                        }
                    }
                }
            }
        }
        Ok(result)
    }
    fn section(
        &mut self,
        manifest: &ManifestInput,
        section: &str,
        dependencies: &toml::map::Map<String, toml::Value>,
        work: &mut usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        for (key, value) in dependencies {
            super::owner_step(work, check)?;
            let root = key.replace('-', "_");
            if !identifier(&root) {
                continue;
            }
            let (package, version, proved) = match value {
                toml::Value::String(version) => (key.as_str(), Some(version.as_str()), true),
                toml::Value::Table(declaration) => {
                    let package = declaration
                        .get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(key);
                    let version = declaration.get("version").and_then(toml::Value::as_str);
                    let proved = !declaration.contains_key("path")
                        && !declaration.contains_key("workspace")
                        && declaration.get("package").is_none_or(toml::Value::is_str)
                        && declaration.get("version").is_none_or(toml::Value::is_str)
                        && declaration.get("git").is_none_or(toml::Value::is_str)
                        && (version.is_some()
                            || declaration
                                .get("git")
                                .and_then(toml::Value::as_str)
                                .is_some());
                    (package, version, proved)
                }
                _ => (key.as_str(), None, false),
            };
            let descriptor =
                (proved && valid_package("rust", package, false)).then(|| ExternalPackage {
                    identity: ExternalIdentity {
                        kind: ExternalKind::ExternalPackage,
                        source_set_id: manifest.source_set_id.clone(),
                        language: "rust".into(),
                        package: package.into(),
                        standard_library: false,
                    },
                    ecosystem: "cargo",
                    evidence: vec![ExternalEvidence::Manifest {
                        source_set_id: manifest.source_set_id.clone(),
                        native_revision_id: manifest.native_revision_id.clone(),
                        dependency_hash: manifest.dependency_hash.clone(),
                        path: manifest.path.clone(),
                        content_hash: manifest
                            .content_hash
                            .clone()
                            .expect("present manifest hash"),
                        import_root: root.clone(),
                        section: section.into(),
                        key: key.clone(),
                        version_constraint: version.map(str::to_owned),
                    }],
                });
            match self.bindings.entry((manifest.source_set_id.clone(), root)) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert((key.clone(), descriptor));
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let (original_key, binding) = entry.get_mut();
                    match (binding.as_mut(), descriptor) {
                        (Some(existing), Some(additional))
                            if original_key == key && existing.identity == additional.identity =>
                        {
                            existing.evidence.extend(additional.evidence);
                            existing.evidence.sort();
                            existing.evidence.dedup();
                        }
                        _ => *binding = None,
                    }
                }
            }
        }
        Ok(())
    }
    /// Check captured Cargo root settings only after the resolver proves the module chain.
    pub fn root_allowed(&self, source_set_id: &str, root_path: &str) -> bool {
        self.roots
            .get(source_set_id)
            .is_some_and(|(lib, bin)| match root_path {
                "src/lib.rs" => *lib,
                "src/main.rs" => *bin,
                _ => false,
            })
    }
    /// The Rust module resolver, not this manifest parser, proves root ownership.
    /// Call only after applying the language's local binding precedence.
    pub fn resolve(
        &self,
        source_set_id: &str,
        import_root: &str,
        root_owned: bool,
    ) -> Option<ExternalPackage> {
        if root_owned
            && let Some(binding) = self
                .bindings
                .get(&(source_set_id.into(), import_root.into()))
        {
            return binding.1.clone();
        }
        if !matches!(import_root, "std" | "core" | "alloc") {
            return None;
        }
        Some(ExternalPackage {
            identity: ExternalIdentity {
                kind: ExternalKind::ExternalPackage,
                source_set_id: source_set_id.into(),
                language: "rust".into(),
                package: import_root.into(),
                standard_library: true,
            },
            ecosystem: "rust",
            evidence: vec![ExternalEvidence::StandardLibraryCatalog {
                catalog: "rust-standard-library:v1",
                entry: import_root.into(),
            }],
        })
    }
}
fn conventional_roots(table: &toml::map::Map<String, toml::Value>) -> Option<(bool, bool)> {
    let package = table.get("package")?.as_table()?;
    let name = package.get("name")?.as_str()?;
    if !valid_package("rust", name, false) {
        return None;
    }
    let lib = match table.get("lib") {
        Some(lib) => lib.as_table().is_some_and(|lib| {
            lib.get("path")
                .is_none_or(|path| path.as_str() == Some("src/lib.rs"))
        }),
        None => package.get("autolib").and_then(toml::Value::as_bool) != Some(false),
    };
    let bin = match table.get("bin") {
        Some(bins) => bins.as_array().is_some_and(|bins| {
            bins.iter().any(|bin| {
                bin.as_table().is_some_and(|bin| match bin.get("path") {
                    Some(path) => path.as_str() == Some("src/main.rs"),
                    None => bin.get("name").and_then(toml::Value::as_str) == Some(name),
                })
            })
        }),
        None => package.get("autobins").and_then(toml::Value::as_bool) != Some(false),
    };
    Some((lib, bin))
}
