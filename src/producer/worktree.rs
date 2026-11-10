//! The controlled worktree (D1): written into private run state from one
//! retained native revision only, so a mixed basis cannot occur.
//!
//! One map keyed by relative path: every document of the retained revision,
//! then every present entry of the retained config component, then every
//! present entry of the dependency component. The first insert of a path
//! wins. The row then removes its omissions and adds its generated files.

use super::profiles::{BinLink, Generated, Omission, Rules};
use crate::store::NativeReader;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

/// The retained revision as the worktree needs it.
pub trait RetainedSource {
    /// Every document's path and exact retained bytes, one at a time.
    fn each_document(&self, visit: &mut dyn FnMut(&str, Vec<u8>) -> Result<()>) -> Result<()>;
    /// The retained config and dependency components.
    fn components(&self) -> Result<(Vec<Value>, Vec<Value>)>;
}

impl RetainedSource for NativeReader<'_> {
    fn each_document(&self, visit: &mut dyn FnMut(&str, Vec<u8>) -> Result<()>) -> Result<()> {
        let mut inventory = self.inventory()?;
        while let Some(document) = inventory.next_document()? {
            let bytes = self.source_bytes(&document)?;
            visit(&document.key.path, bytes)?;
        }
        Ok(())
    }

    fn components(&self) -> Result<(Vec<Value>, Vec<Value>)> {
        let (_toolchain, config, dependencies) = self.retained_components()?;
        Ok((config, dependencies))
    }
}

/// One present root input with its retained bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootInput {
    pub path: String,
    pub bytes: Vec<u8>,
}

impl RootInput {
    pub fn sha256(&self) -> String {
        hex::encode(Sha256::digest(&self.bytes))
    }
}

/// The present entries of a retained component. Absent entries and entries
/// without a path (`nativeAdmission`) are skipped.
pub fn present_entries(entries: &[Value]) -> Result<Vec<RootInput>> {
    let mut out = Vec::new();
    for entry in entries {
        if entry["type"] != "present" {
            continue;
        }
        let (Some(path), Some(bytes)) = (entry["path"].as_str(), entry["bytes"].as_str()) else {
            continue;
        };
        out.push(RootInput {
            path: path.to_owned(),
            bytes: hex::decode(bytes).context("retained root input bytes")?,
        });
    }
    Ok(out)
}

/// The retained root inputs of one revision.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RetainedInputs {
    pub config: Vec<RootInput>,
    pub dependencies: Vec<RootInput>,
}

impl RetainedInputs {
    pub fn read(source: &dyn RetainedSource) -> Result<Self> {
        let (config, dependencies) = source.components()?;
        Ok(Self {
            config: present_entries(&config)?,
            dependencies: present_entries(&dependencies)?,
        })
    }
}

/// A listed file: its path and content hash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Listed {
    pub path: String,
    pub sha256: String,
}

/// What the plan shows of the worktree's non-source files.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Layout {
    pub config: Vec<Listed>,
    pub dependency_manifests: Vec<Listed>,
    pub omitted: Vec<(String, String)>,
    pub generated: Vec<Listed>,
}

fn omission<'a>(rules: &'a Rules, path: &str) -> Option<&'a Omission> {
    rules.omit.iter().find(|omission| omission.matches(path))
}

/// Sources, config, then dependency inputs claim a path only once.
fn supplied(rules: &Rules, path: &str, already_present: bool) -> bool {
    omission(rules, path).is_none() && !already_present
}

/// Whether a default-only generated file gives way to a root input at its worktree path.
fn replaced(file: &Generated, present: impl Fn(&str) -> bool) -> bool {
    file.default_only && file.path.strip_prefix("worktree/").is_some_and(present)
}

/// The listing for `rules`. Omitted paths leave the config and dependency
/// lists and appear under `omitted` once, in sorted order.
pub fn layout(inputs: &RetainedInputs, rules: &Rules) -> Layout {
    let mut present = std::collections::BTreeSet::new();
    let mut list = |files: &[RootInput]| -> Vec<Listed> {
        files
            .iter()
            .filter(|file| {
                supplied(rules, &file.path, present.contains(&file.path))
                    && present.insert(file.path.clone())
            })
            .map(|file| Listed {
                path: file.path.clone(),
                sha256: file.sha256(),
            })
            .collect()
    };
    let mut omitted: BTreeMap<String, String> = BTreeMap::new();
    for file in inputs.config.iter().chain(&inputs.dependencies) {
        if let Some(omission) = omission(rules, &file.path) {
            omitted.insert(file.path.clone(), omission.reason.to_owned());
        }
    }
    let config = list(&inputs.config);
    let dependency_manifests = list(&inputs.dependencies);
    Layout {
        config,
        dependency_manifests,
        omitted: omitted.into_iter().collect(),
        generated: rules
            .generated
            .iter()
            .filter(|file| !replaced(file, |path| present.contains(path)))
            .map(|file| Listed {
                path: file.path.clone(),
                sha256: hex::encode(Sha256::digest(&file.bytes)),
            })
            .collect(),
    }
}

/// A controlled profile's effective non-source context. Native identity and freshness
/// keep their full captured hashes; this digest is only for P-to-N pending eligibility.
/// Entries borrow retained metadata, including encoded bytes, without decoding file bodies.
pub(crate) fn context_hash(
    rules: &Rules,
    config: &[Value],
    dependencies: &[Value],
    is_source: &dyn Fn(&str) -> Result<bool>,
    cancel: &AtomicBool,
) -> Result<String> {
    let mut files = BTreeMap::new();
    let mut metadata = Vec::new();
    for entry in config.iter().chain(dependencies) {
        check_cancel(cancel)?;
        let Some(path) = entry["path"].as_str() else {
            metadata.push(entry);
            continue;
        };
        if omission(rules, path).is_some() {
            continue;
        }
        match entry["type"].as_str() {
            Some("absent") => {}
            Some("present") => {
                if supplied(rules, path, files.contains_key(path)) && !is_source(path)? {
                    files.insert(path, entry);
                }
            }
            _ => metadata.push(entry),
        }
    }
    let mut hash = Sha256::new();
    hash.update(b"controlled-worktree-context:v1\0files[");
    for entry in files.values() {
        check_cancel(cancel)?;
        crate::native_ids::canonical_into(entry, &mut |bytes| hash.update(bytes));
        hash.update(b",");
    }
    hash.update(b"]metadata[");
    for entry in metadata {
        check_cancel(cancel)?;
        crate::native_ids::canonical_into(entry, &mut |bytes| hash.update(bytes));
        hash.update(b",");
    }
    hash.update(b"]generated[");
    for file in &rules.generated {
        check_cancel(cancel)?;
        let present = if file.default_only {
            match file.path.strip_prefix("worktree/") {
                Some(path) if omission(rules, path).is_none() => {
                    files.contains_key(path) || is_source(path)?
                }
                _ => false,
            }
        } else {
            false
        };
        if !replaced(file, |_| present) {
            crate::native_ids::canonical_string_into(&file.path, &mut |bytes| hash.update(bytes));
            hash.update(Sha256::digest(&file.bytes));
            hash.update(b",");
        }
    }
    hash.update(b"]");
    check_cancel(cancel)?;
    Ok(hex::encode(hash.finalize()))
}

fn private_dirs(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create {}", path.display()))
}

/// Written once each, never replacing an existing file.
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        private_dirs(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write {}", path.display()))
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::SeqCst), "cancelled");
    Ok(())
}

/// Write `<run_dir>/worktree/` from `source` and the row's `rules`, then the
/// row's generated files. Cancellation is checked between files. Returns the
/// number of files written under `worktree/`.
pub fn materialize(
    source: &dyn RetainedSource,
    rules: &Rules,
    run_dir: &Path,
    cancel: &AtomicBool,
) -> Result<usize> {
    let worktree = run_dir.join("worktree");
    private_dirs(&worktree)?;
    let inputs = RetainedInputs::read(source)?;
    // A config input over its cap was captured unread (#581): the producer would get the
    // placeholder as that file, so the run is refused rather than fail on it or run without it.
    if let Some(file) = inputs
        .config
        .iter()
        .chain(&inputs.dependencies)
        .find(|file| {
            file.bytes == crate::skipped_config::PLACEHOLDER
                && omission(rules, &file.path).is_none()
        })
    {
        anyhow::bail!(
            "capture_unavailable: config input {} is over the config input cap and was not read",
            file.path
        );
    }
    let mut written = std::collections::BTreeSet::new();
    let mut put = |path: &str, bytes: &[u8]| -> Result<()> {
        check_cancel(cancel)?;
        if supplied(rules, path, written.contains(path)) {
            written.insert(path.to_owned());
            write_new(&worktree.join(path), bytes)?;
        }
        Ok(())
    };
    source.each_document(&mut |path, bytes| put(path, &bytes))?;
    for file in inputs.config.iter().chain(&inputs.dependencies) {
        put(&file.path, &file.bytes)?;
    }
    let count = written.len();
    for file in &rules.generated {
        check_cancel(cancel)?;
        if !replaced(file, |path| written.contains(path)) {
            write_new(&run_dir.join(&file.path), &file.bytes)?;
        }
    }
    Ok(count)
}

/// Create `<run_dir>/bin/` holding exactly `links`.
pub fn write_bin_links(run_dir: &Path, links: &[BinLink]) -> Result<PathBuf> {
    let bin = run_dir.join("bin");
    private_dirs(&bin)?;
    for link in links {
        std::os::unix::fs::symlink(&link.target, bin.join(&link.name))
            .with_context(|| format!("link {}", link.name))?;
    }
    Ok(bin)
}
