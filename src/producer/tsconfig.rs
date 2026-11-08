//! A root `tsconfig.json` whose `extends` names an npm package (#799). The run's worktree
//! is written from the retained revision only and holds no `node_modules`, so TypeScript
//! cannot resolve the package there (TS6053). In this order:
//!
//! 1. The package's config, and every config its `extends` chain reaches, is read from the
//!    checkout's own `node_modules`, read-only, bounded and executing nothing. Each file is
//!    generated at its own path under `worktree/node_modules/`, and package specifiers are
//!    rewritten to relative paths, so every relative option keeps its meaning.
//! 2. When the checkout has no such package or file, the generated tsconfig (#652) replaces
//!    the repo's, and the plan records the replacement under `omitted`.
//! 3. Any other failure of an `extends` is the typed `producer_config_unresolved` refusal,
//!    naming the `extends`.

use super::profiles::{Generated, Omission, Rules};
use super::worktree::RetainedInputs;
use crate::references::tsconfig::parse_jsonc;
use anyhow::Result;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub const CONFIG_UNRESOLVED: &str = "producer_config_unresolved";
/// The largest config or `package.json` read from `node_modules`.
pub const MAX_FILE_BYTES: u64 = 256 * 1024;
/// The most files one `extends` chain may supply.
pub const MAX_CHAIN_FILES: usize = 16;
const ROOT: &str = "tsconfig.json";
const NODE_MODULES: &str = "node_modules";
/// Export conditions a config lookup may match, as TypeScript's resolver accepts them.
const CONDITIONS: [&str; 5] = ["types", "node", "require", "import", "default"];
const MAX_EXPORTS_DEPTH: usize = 8;

/// Why one `extends` could not be supplied.
#[derive(Debug)]
enum Failure {
    /// The package or its file is not in the checkout: the generated tsconfig is used.
    Missing(String),
    /// Anything else: the typed refusal.
    Invalid(String),
}

/// The `extends` entries of a config: absent, one string or an array of strings.
fn extends_of(config: &Value) -> Result<Vec<String>, Failure> {
    let invalid = || Failure::Invalid("extends is not a string or an array of strings".into());
    match config.get("extends") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(one)) => Ok(vec![one.clone()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or_else(invalid))
            .collect(),
        Some(_) => Err(invalid()),
    }
}

/// A path TypeScript resolves as a file rather than as a package.
fn is_path(specifier: &str) -> bool {
    specifier.starts_with('/')
        || matches!(specifier, "." | "..")
        || specifier.starts_with("./")
        || specifier.starts_with("../")
}

/// `dir` joined with `relative`, with `.` and `..` resolved, or `None` when it climbs out of
/// the checkout.
fn join(dir: &str, relative: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|part| !part.is_empty()).collect();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// `to` relative to the directory `from`, in the `./` or `../` form `extends` reads as a path.
fn relative(from: &str, to: &str) -> String {
    let from: Vec<&str> = from.split('/').filter(|part| !part.is_empty()).collect();
    let to: Vec<&str> = to.split('/').collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let up = from.len() - common;
    let rest = to[common..].join("/");
    if up == 0 {
        format!("./{rest}")
    } else {
        format!("{}{rest}", "../".repeat(up))
    }
}

/// A package specifier's name (`pkg` or `@scope/pkg`) and its subpath, if any.
fn split_package(specifier: &str) -> Option<(String, Option<String>)> {
    let segments: Vec<&str> = specifier.split('/').collect();
    let take = if specifier.starts_with('@') { 2 } else { 1 };
    if segments.len() < take {
        return None;
    }
    let valid = |segment: &str| {
        !segment.is_empty()
            && !matches!(segment, "." | "..")
            && !segment.contains(['\\', '\0', ':'])
    };
    let (name, rest) = segments.split_at(take);
    if !name.iter().all(|segment| valid(segment)) || (take == 2 && name[0] == "@") {
        return None;
    }
    let subpath = (!rest.is_empty()).then(|| rest.join("/"));
    Some((name.join("/"), subpath))
}

/// The configs one root `extends` reaches, read from a checkout.
struct Chain<'a> {
    workspace: &'a Path,
    /// Supplied files by checkout-relative path, with the bytes the worktree gets.
    files: BTreeMap<String, Vec<u8>>,
    /// Paths still being read, so a cycle is refused instead of followed.
    reading: Vec<String>,
}

impl Chain<'_> {
    fn path(&self, relative: &str) -> PathBuf {
        self.workspace.join(relative)
    }

    fn is_file(&self, relative: &str) -> bool {
        fs::metadata(self.path(relative)).is_ok_and(|meta| meta.is_file())
    }

    fn is_dir(&self, relative: &str) -> bool {
        fs::metadata(self.path(relative)).is_ok_and(|meta| meta.is_dir())
    }

    /// At most `MAX_FILE_BYTES` of a regular file, checked before it is opened so a FIFO or
    /// device is never opened.
    fn read(&self, relative: &str) -> Result<Vec<u8>, Failure> {
        let path = self.path(relative);
        let meta = match fs::metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Failure::Missing(format!("{relative} is missing")));
            }
            Err(error) => return Err(Failure::Invalid(format!("{relative}: {error}"))),
        };
        if !meta.is_file() {
            return Err(Failure::Missing(format!("{relative} is not a file")));
        }
        let too_large =
            || Failure::Invalid(format!("{relative} is larger than {MAX_FILE_BYTES} bytes"));
        if meta.len() > MAX_FILE_BYTES {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        fs::File::open(&path)
            .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
            .map_err(|error| Failure::Invalid(format!("{relative}: {error}")))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(too_large());
        }
        Ok(bytes)
    }

    /// The config file a package specifier names, looked up in the `node_modules` of
    /// `from_dir` and of each of its ancestors, as Node resolution does.
    fn resolve_package(&self, specifier: &str, from_dir: &str) -> Result<String, Failure> {
        let (name, subpath) = split_package(specifier)
            .ok_or_else(|| Failure::Invalid("is not a valid package name".into()))?;
        let mut dir = Some(from_dir);
        let package = loop {
            let Some(at) = dir else {
                return Err(Failure::Missing(format!(
                    "package {name} is not in the checkout's node_modules"
                )));
            };
            let candidate = join(at, &format!("{NODE_MODULES}/{name}"))
                .ok_or_else(|| Failure::Invalid("leaves the checkout".into()))?;
            if self.is_dir(&candidate) {
                break candidate;
            }
            dir = (!at.is_empty()).then(|| parent(at));
        };
        let manifest = format!("{package}/package.json");
        let manifest: Option<Value> = if self.is_file(&manifest) {
            let bytes = self.read(&manifest)?;
            Some(
                serde_json::from_slice(&bytes)
                    .map_err(|_| Failure::Invalid(format!("{manifest} is not valid JSON")))?,
            )
        } else {
            None
        };
        let key = subpath
            .as_deref()
            .map_or_else(|| ".".to_owned(), |sub| format!("./{sub}"));
        let exported = manifest
            .as_ref()
            .and_then(|manifest| manifest.get("exports"))
            .map(|exports| export_target(exports, &key));
        let candidates: Vec<String> = match exported {
            Some(Some(target)) => vec![target],
            Some(None) => {
                return Err(Failure::Missing(format!(
                    "{package}/package.json exports no config for {key}"
                )));
            }
            None => match &subpath {
                Some(sub) => vec![
                    sub.clone(),
                    format!("{sub}.json"),
                    format!("{sub}/tsconfig.json"),
                ],
                None => manifest
                    .as_ref()
                    .and_then(|manifest| manifest.get("tsconfig"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .into_iter()
                    .chain(["tsconfig.json".to_owned()])
                    .collect(),
            },
        };
        for candidate in candidates {
            let path = join(&package, &candidate)
                .filter(|path| path.starts_with(&format!("{package}/")))
                .ok_or_else(|| Failure::Invalid(format!("{candidate} leaves {package}")))?;
            if self.is_file(&path) {
                return Ok(path);
            }
        }
        Err(Failure::Missing(format!(
            "{package} has no config file for {specifier}"
        )))
    }

    /// The file a relative `extends` names, from the config at `from`: the path itself, else
    /// with `.json` added, as TypeScript tries them. It must stay under `node_modules`.
    fn resolve_path(&self, specifier: &str, from: &str) -> Result<String, Failure> {
        if specifier.starts_with('/') {
            return Err(Failure::Invalid("is an absolute path".into()));
        }
        let path = join(parent(from), specifier)
            .filter(|path| path.starts_with(&format!("{NODE_MODULES}/")))
            .ok_or_else(|| Failure::Invalid("leaves the checkout's node_modules".into()))?;
        if self.is_file(&path) || path.ends_with(".json") {
            return Ok(path);
        }
        Ok(format!("{path}.json"))
    }

    /// Read the config at `path` and everything its `extends` reaches.
    fn supply(&mut self, path: &str) -> Result<(), Failure> {
        if self.reading.iter().any(|reading| reading == path) {
            return Err(Failure::Invalid(format!("{path} extends itself")));
        }
        if self.files.contains_key(path) {
            return Ok(());
        }
        if self.files.len() + self.reading.len() >= MAX_CHAIN_FILES {
            return Err(Failure::Invalid(format!(
                "the extends chain reaches more than {MAX_CHAIN_FILES} files"
            )));
        }
        let bytes = self.read(path)?;
        let mut config = parse_jsonc(&bytes)
            .filter(Value::is_object)
            .ok_or_else(|| Failure::Invalid(format!("{path} is not a JSON object")))?;
        let extends = extends_of(&config).map_err(|failure| within(failure, path))?;
        self.reading.push(path.to_owned());
        let mut rewritten = Vec::with_capacity(extends.len());
        let mut changed = false;
        for specifier in &extends {
            let target = if is_path(specifier) {
                self.resolve_path(specifier, path)
            } else {
                self.resolve_package(specifier, parent(path))
            }
            .map_err(|failure| within(failure, &format!("{path} extends \"{specifier}\"")))?;
            self.supply(&target)?;
            if is_path(specifier) {
                rewritten.push(specifier.clone());
            } else {
                rewritten.push(relative(parent(path), &target));
                changed = true;
            }
        }
        self.reading.pop();
        let bytes = if changed {
            set_extends(&mut config, rewritten);
            serde_json::to_vec(&config).map_err(|error| Failure::Invalid(error.to_string()))?
        } else {
            bytes
        };
        self.files.insert(path.to_owned(), bytes);
        Ok(())
    }
}

fn within(failure: Failure, context: &str) -> Failure {
    match failure {
        Failure::Missing(reason) => Failure::Missing(format!("{context}: {reason}")),
        Failure::Invalid(reason) => Failure::Invalid(format!("{context}: {reason}")),
    }
}

/// Replace `extends`, keeping a single string a string.
fn set_extends(config: &mut Value, specifiers: Vec<String>) {
    let value = match (&config["extends"], specifiers.as_slice()) {
        (Value::String(_), [one]) => Value::String(one.clone()),
        _ => Value::from(specifiers),
    };
    config["extends"] = value;
}

/// The target `exports` gives `key` (`.` or `./sub`): a string, the first matching condition
/// of a conditions object, or the first entry of an array that has one.
fn export_target(exports: &Value, key: &str) -> Option<String> {
    let entry = match exports {
        Value::Object(map) if map.keys().any(|key| key.starts_with('.')) => map.get(key)?,
        _ if key == "." => exports,
        _ => return None,
    };
    conditional(entry, 0)
}

fn conditional(value: &Value, depth: usize) -> Option<String> {
    if depth > MAX_EXPORTS_DEPTH {
        return None;
    }
    match value {
        Value::String(target) => Some(target.clone()),
        Value::Array(items) => items.iter().find_map(|item| conditional(item, depth + 1)),
        Value::Object(map) => map
            .iter()
            .filter(|(condition, _)| CONDITIONS.contains(&condition.as_str()))
            .find_map(|(_, value)| conditional(value, depth + 1)),
        _ => None,
    }
}

/// The typed refusal. `extends` is the JSON text of the root's `extends` entry.
fn unresolved(extends: &str, reason: &str) -> anyhow::Error {
    let extends: String = extends.chars().take(200).collect();
    anyhow::anyhow!(
        "{CONFIG_UNRESOLVED}: {ROOT} extends {extends}, which cannot be supplied to the run's worktree: {reason}"
    )
}

/// Adjust scip-typescript's `rules` for a root `tsconfig.json` whose `extends` names a
/// package. Any other root config is left to the run as it is.
pub fn apply(rules: &mut Rules, inputs: &RetainedInputs, workspace: &Path) -> Result<()> {
    let Some(root) = inputs.config.iter().find(|file| file.path == ROOT) else {
        return Ok(());
    };
    // A config this cannot read is left to TypeScript, which reports it.
    let Some(mut config) = parse_jsonc(&root.bytes).filter(Value::is_object) else {
        return Ok(());
    };
    let extends = match extends_of(&config) {
        Ok(extends) => extends,
        Err(Failure::Invalid(reason) | Failure::Missing(reason)) => {
            return Err(unresolved(&config["extends"].to_string(), &reason));
        }
    };
    if extends.iter().all(|specifier| is_path(specifier)) {
        return Ok(());
    }
    let mut chain = Chain {
        workspace,
        files: BTreeMap::new(),
        reading: vec![ROOT.to_owned()],
    };
    let mut rewritten = Vec::with_capacity(extends.len());
    for specifier in &extends {
        if is_path(specifier) {
            rewritten.push(specifier.clone());
            continue;
        }
        let supplied = chain
            .resolve_package(specifier, "")
            .and_then(|target| chain.supply(&target).map(|()| target));
        match supplied {
            Ok(target) => rewritten.push(relative("", &target)),
            Err(Failure::Missing(reason)) => {
                rules.omit.push(Omission {
                    path: ROOT,
                    reason: format!(
                        "extends \"{specifier}\", which the checkout's node_modules does not hold ({reason}); the generated tsconfig.json replaces it"
                    ),
                });
                return Ok(());
            }
            Err(Failure::Invalid(reason)) => {
                return Err(unresolved(
                    &Value::from(specifier.as_str()).to_string(),
                    &reason,
                ));
            }
        }
    }
    set_extends(&mut config, rewritten);
    let packages: Vec<&str> = extends
        .iter()
        .filter(|specifier| !is_path(specifier))
        .map(String::as_str)
        .collect();
    rules.omit.push(Omission {
        path: ROOT,
        reason: format!(
            "extends {}, supplied from the checkout's node_modules; a copy with extends rewritten to the supplied files is generated",
            packages
                .iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    });
    // The rewritten root takes the place of the default-only generated one.
    rules
        .generated
        .retain(|file| file.path != format!("worktree/{ROOT}"));
    rules.generated.push(Generated {
        path: format!("worktree/{ROOT}"),
        bytes: serde_json::to_vec(&config)?,
        default_only: false,
    });
    rules
        .generated
        .extend(chain.files.into_iter().map(|(path, bytes)| Generated {
            path: format!("worktree/{path}"),
            bytes,
            default_only: false,
        }));
    Ok(())
}
