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
//!
//! The root config is chosen as the project's settings say (#717): the `--ts-project`
//! override, else `tsconfig.json`, else `jsconfig.json` (through a generated `tsconfig.json`
//! that extends it, since scip-typescript reads only `tsconfig.json`), else the generated
//! tsconfig. A root config Trellis cannot parse is the same typed refusal.

use super::profiles::{Generated, Omission, Rules};
use super::settings::{Origin, Setting, Source, TsProject};
use super::worktree::{RetainedInputs, RootInput};
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
/// The most files (configs and `package.json` manifests) one resolution reads.
pub const MAX_CHAIN_FILES: usize = 16;
/// The most paths one resolution inspects while looking files up.
pub const MAX_PROBES: usize = 256;
const TSCONFIG: &str = "tsconfig.json";
const JSCONFIG: &str = "jsconfig.json";
/// The project config scip-typescript reads when the checkout's root config is a
/// `jsconfig.json`: TypeScript gives a `jsconfig.json` `allowJs` by default, and a config
/// reached through `extends` keeps none of its file-name defaults, so this sets it.
pub const JSCONFIG_PROJECT: &[u8] =
    br#"{"extends":"./jsconfig.json","compilerOptions":{"allowJs":true}}"#;
const NODE_MODULES: &str = "node_modules";
/// The export conditions TypeScript 5.9 matches when it resolves a config's `extends`
/// (`nodeNextJsonConfigResolver`: CommonJS mode, so `require`, `types` and `node`, plus
/// `default`, which always matches). Their order here does not matter: the manifest's does.
const CONDITIONS: [&str; 4] = ["require", "types", "node", "default"];
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

/// A package's `exports`, with every object's properties in source order: conditions are
/// matched in the order the manifest lists them, as Node and TypeScript match them.
/// (`serde_json::Value` sorts object keys, which would put `default` before `types`.)
#[derive(Debug)]
enum Exports {
    Target(String),
    List(Vec<Exports>),
    Object(Vec<(String, Exports)>),
    Other,
}

impl<'de> serde::Deserialize<'de> for Exports {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Exports;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a package.json exports value")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Exports, E> {
                Ok(Exports::Target(value.to_owned()))
            }

            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Exports, E> {
                Ok(Exports::Other)
            }

            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Exports, E> {
                Ok(Exports::Other)
            }

            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Exports, E> {
                Ok(Exports::Other)
            }

            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Exports, E> {
                Ok(Exports::Other)
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Exports, E> {
                Ok(Exports::Other)
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Exports, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Exports::List(items))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Exports, A::Error> {
                let mut entries = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Exports>()? {
                    entries.push((key, value));
                }
                Ok(Exports::Object(entries))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// The two `package.json` fields a config lookup reads.
#[derive(Debug, serde::Deserialize)]
struct Manifest {
    exports: Option<Exports>,
    tsconfig: Option<Value>,
}

/// What is at a path, without following anything but the checkout's own directories.
enum Kind {
    Absent,
    File,
    Dir,
    Other,
}

/// The configs one root `extends` reaches, read from a checkout.
struct Chain<'a> {
    workspace: &'a Path,
    /// The checkout's `node_modules`, canonical: every file read must really be under it.
    node_modules: Option<PathBuf>,
    /// Supplied files by checkout-relative path, with the bytes the worktree gets.
    files: BTreeMap<String, Vec<u8>>,
    /// Paths still being read, so a cycle is refused instead of followed.
    reading: Vec<String>,
    /// Files read from `node_modules` (configs and manifests), against `MAX_CHAIN_FILES`.
    reads: usize,
    /// Paths inspected, against `MAX_PROBES`.
    probes: usize,
}

impl<'a> Chain<'a> {
    fn new(workspace: &'a Path, root: &str) -> Self {
        Self {
            workspace,
            node_modules: fs::canonicalize(workspace.join(NODE_MODULES)).ok(),
            files: BTreeMap::new(),
            reading: vec![root.to_owned()],
            reads: 0,
            probes: 0,
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.workspace.join(relative)
    }

    /// What `relative` is. Only absence is `Absent`; any other error is a refusal.
    fn kind(&mut self, relative: &str) -> Result<Kind, Failure> {
        self.probes += 1;
        if self.probes > MAX_PROBES {
            return Err(Failure::Invalid(format!(
                "the extends lookup inspects more than {MAX_PROBES} paths"
            )));
        }
        match fs::metadata(self.path(relative)) {
            Ok(meta) if meta.is_file() => Ok(Kind::File),
            Ok(meta) if meta.is_dir() => Ok(Kind::Dir),
            Ok(_) => Ok(Kind::Other),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(Kind::Absent)
            }
            Err(error) => Err(Failure::Invalid(format!("{relative}: {error}"))),
        }
    }

    /// At most `MAX_FILE_BYTES` of a regular file whose real path is under the checkout's
    /// `node_modules`. The type is checked before the file is opened, so a FIFO or device is
    /// never opened.
    fn read(&mut self, relative: &str) -> Result<Vec<u8>, Failure> {
        self.reads += 1;
        if self.reads > MAX_CHAIN_FILES {
            return Err(Failure::Invalid(format!(
                "the extends chain reads more than {MAX_CHAIN_FILES} files"
            )));
        }
        let path = self.path(relative);
        let real = fs::canonicalize(&path)
            .map_err(|error| Failure::Invalid(format!("{relative}: {error}")))?;
        if !self
            .node_modules
            .as_ref()
            .is_some_and(|node_modules| real.starts_with(node_modules))
        {
            return Err(Failure::Invalid(format!(
                "{relative} resolves outside the checkout's node_modules"
            )));
        }
        let meta = fs::metadata(&real)
            .map_err(|error| Failure::Invalid(format!("{relative}: {error}")))?;
        if !meta.is_file() {
            return Err(Failure::Invalid(format!(
                "{relative} is not a regular file"
            )));
        }
        let too_large =
            || Failure::Invalid(format!("{relative} is larger than {MAX_FILE_BYTES} bytes"));
        if meta.len() > MAX_FILE_BYTES {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        fs::File::open(&real)
            .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
            .map_err(|error| Failure::Invalid(format!("{relative}: {error}")))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(too_large());
        }
        Ok(bytes)
    }

    /// The config file a package specifier names. The `node_modules` of `from_dir` and of each
    /// ancestor up to the checkout root are tried in turn, as TypeScript does: a package
    /// directory with no config passes the lookup outward, unless its `exports` decided it.
    fn resolve_package(&mut self, specifier: &str, from_dir: &str) -> Result<String, Failure> {
        let (name, subpath) = split_package(specifier)
            .ok_or_else(|| Failure::Invalid("is not a valid package name".into()))?;
        let mut dir = Some(from_dir);
        let mut last = format!("package {name} is not in the checkout's node_modules");
        while let Some(at) = dir {
            dir = (!at.is_empty()).then(|| parent(at));
            let package = join(at, &format!("{NODE_MODULES}/{name}"))
                .ok_or_else(|| Failure::Invalid("leaves the checkout".into()))?;
            if !matches!(self.kind(&package)?, Kind::Dir) {
                continue;
            }
            match self.resolve_in(&package, subpath.as_deref(), specifier)? {
                Lookup::Found(path) => return Ok(path),
                Lookup::Decided(reason) => return Err(Failure::Missing(reason)),
                Lookup::Next(reason) => last = reason,
            }
        }
        Err(Failure::Missing(last))
    }

    /// The config `specifier` names inside one package directory.
    fn resolve_in(
        &mut self,
        package: &str,
        subpath: Option<&str>,
        specifier: &str,
    ) -> Result<Lookup, Failure> {
        let manifest_path = format!("{package}/package.json");
        let manifest: Option<Manifest> = match self.kind(&manifest_path)? {
            Kind::Absent => None,
            Kind::File => {
                let bytes = self.read(&manifest_path)?;
                Some(serde_json::from_slice(&bytes).map_err(|_| {
                    Failure::Invalid(format!("{manifest_path} is not a valid package.json"))
                })?)
            }
            Kind::Dir | Kind::Other => {
                return Err(Failure::Invalid(format!(
                    "{manifest_path} is not a regular file"
                )));
            }
        };
        let key = subpath.map_or_else(|| ".".to_owned(), |sub| format!("./{sub}"));
        // `(candidate, whether a directory there is passed over)`.
        let candidates: Vec<(String, bool)> = match manifest
            .as_ref()
            .and_then(|manifest| manifest.exports.as_ref())
        {
            Some(exports) => export_targets(exports, &key)?
                .into_iter()
                .map(|target| (target, false))
                .collect(),
            None => match subpath {
                Some(sub) => vec![
                    (sub.to_owned(), true),
                    (format!("{sub}.json"), false),
                    (format!("{sub}/tsconfig.json"), false),
                ],
                None => manifest
                    .as_ref()
                    .and_then(|manifest| manifest.tsconfig.as_ref())
                    .and_then(Value::as_str)
                    .map(|field| (field.to_owned(), false))
                    .into_iter()
                    .chain([("tsconfig.json".to_owned(), false)])
                    .collect(),
            },
        };
        for (candidate, directory_passes) in candidates {
            let path = join(package, &candidate)
                .filter(|path| path.starts_with(&format!("{package}/")))
                .ok_or_else(|| Failure::Invalid(format!("{candidate} leaves {package}")))?;
            match self.kind(&path)? {
                Kind::File => return Ok(Lookup::Found(path)),
                Kind::Absent => {}
                Kind::Dir if directory_passes => {}
                Kind::Dir | Kind::Other => {
                    return Err(Failure::Invalid(format!("{path} is not a regular file")));
                }
            }
        }
        let reason = format!("{package} has no config file for {specifier}");
        Ok(
            if manifest.is_some_and(|manifest| manifest.exports.is_some()) {
                Lookup::Decided(reason)
            } else {
                Lookup::Next(reason)
            },
        )
    }

    /// The file a relative `extends` names, from the config at `from`: the path itself, else
    /// with `.json` added, as TypeScript tries them. It must stay under `node_modules`.
    fn resolve_path(&mut self, specifier: &str, from: &str) -> Result<String, Failure> {
        if specifier.starts_with('/') {
            return Err(Failure::Invalid("is an absolute path".into()));
        }
        let path = join(parent(from), specifier)
            .filter(|path| path.starts_with(&format!("{NODE_MODULES}/")))
            .ok_or_else(|| Failure::Invalid("leaves the checkout's node_modules".into()))?;
        match self.kind(&path)? {
            Kind::File => return Ok(path),
            Kind::Absent | Kind::Dir if !path.ends_with(".json") => {}
            Kind::Absent => return Err(Failure::Missing(format!("{path} is missing"))),
            Kind::Dir | Kind::Other => {
                return Err(Failure::Invalid(format!("{path} is not a regular file")));
            }
        }
        let path = format!("{path}.json");
        match self.kind(&path)? {
            Kind::File => Ok(path),
            Kind::Absent => Err(Failure::Missing(format!("{path} is missing"))),
            Kind::Dir | Kind::Other => {
                Err(Failure::Invalid(format!("{path} is not a regular file")))
            }
        }
    }

    /// Read the config at `path` and everything its `extends` reaches.
    fn supply(&mut self, path: &str) -> Result<(), Failure> {
        if self.reading.iter().any(|reading| reading == path) {
            return Err(Failure::Invalid(format!("{path} extends itself")));
        }
        if self.files.contains_key(path) {
            return Ok(());
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

/// One package directory's answer.
enum Lookup {
    Found(String),
    /// Its `exports` names no config: TypeScript stops here.
    Decided(String),
    /// No config, and nothing stops the lookup going outward.
    Next(String),
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

/// The targets `exports` gives `key` (`.` or `./sub`), in the order TypeScript tries them:
/// a string; each entry of an array; each matching condition of a conditions object, in
/// the manifest's own order. TypeScript tries the next target when one does not resolve.
fn export_targets(exports: &Exports, key: &str) -> Result<Vec<String>, Failure> {
    let entry = match exports {
        Exports::Object(entries) if entries.iter().any(|(name, _)| name.starts_with('.')) => {
            match entries.iter().find(|(name, _)| name == key) {
                Some((_, entry)) => entry,
                None if entries.iter().any(|(name, _)| name.contains('*')) => {
                    return Err(Failure::Invalid(
                        "package.json exports uses subpath patterns, which are not supported"
                            .into(),
                    ));
                }
                None => return Ok(Vec::new()),
            }
        }
        _ if key == "." => exports,
        _ => return Ok(Vec::new()),
    };
    let mut targets = Vec::new();
    conditional(entry, 0, &mut targets)?;
    Ok(targets)
}

fn conditional(value: &Exports, depth: usize, targets: &mut Vec<String>) -> Result<(), Failure> {
    if depth > MAX_EXPORTS_DEPTH {
        return Err(Failure::Invalid(format!(
            "package.json exports nests deeper than {MAX_EXPORTS_DEPTH} levels"
        )));
    }
    match value {
        Exports::Target(target) => targets.push(target.clone()),
        Exports::List(items) => {
            for item in items {
                conditional(item, depth + 1, targets)?;
            }
        }
        Exports::Object(entries) => {
            for (condition, value) in entries {
                if condition.starts_with("types@") {
                    return Err(Failure::Invalid(format!(
                        "package.json exports uses the versioned condition {condition}, which is not supported"
                    )));
                }
                if CONDITIONS.contains(&condition.as_str()) {
                    conditional(value, depth + 1, targets)?;
                }
            }
        }
        Exports::Other => {}
    }
    Ok(())
}

/// Whether the config `bytes` (already accepted by `resolve_root`) extends the root file `path`
/// by a relative path, with or without its `.json`.
fn extends_root(bytes: &[u8], path: &str) -> bool {
    let Some(config) = parse_jsonc(bytes) else {
        return false;
    };
    extends_of(&config).is_ok_and(|extends| {
        extends
            .iter()
            .filter(|specifier| is_path(specifier))
            .any(|specifier| {
                join("", specifier)
                    .is_some_and(|target| target == path || format!("{target}.json") == path)
            })
    })
}

/// The typed refusal. `extends` is the JSON text of the root's `extends` entry.
fn unresolved(root: &str, extends: &str, reason: &str) -> anyhow::Error {
    let extends: String = extends.chars().take(200).collect();
    anyhow::anyhow!(
        "{CONFIG_UNRESOLVED}: {root} extends {extends}, which cannot be supplied to the run's worktree: {reason}"
    )
}

/// Adjust scip-typescript's `rules` for the root config `root` (`tsconfig.json` or
/// `jsconfig.json`) whose `extends` names a package. A config Trellis cannot parse is the
/// typed refusal (#717). Returns why the generated tsconfig replaces it, when its package is
/// not in the checkout.
fn resolve_root(
    rules: &mut Rules,
    root: &RootInput,
    root_path: &'static str,
    workspace: &Path,
) -> Result<Option<String>> {
    if root.bytes == crate::skipped_config::PLACEHOLDER {
        anyhow::bail!(
            "{CONFIG_UNRESOLVED}: {root_path} is over the config input cap and was not read"
        );
    }
    let Some(mut config) = parse_jsonc(&root.bytes).filter(Value::is_object) else {
        anyhow::bail!("{CONFIG_UNRESOLVED}: {root_path} is not a JSON object");
    };
    let extends = match extends_of(&config) {
        Ok(extends) => extends,
        Err(Failure::Invalid(reason) | Failure::Missing(reason)) => {
            return Err(unresolved(
                root_path,
                &config["extends"].to_string(),
                &reason,
            ));
        }
    };
    if extends.iter().all(|specifier| is_path(specifier)) {
        return Ok(None);
    }
    let mut chain = Chain::new(workspace, root_path);
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
                let reason = format!(
                    "extends \"{specifier}\", which the checkout's node_modules does not hold ({reason}); the generated tsconfig.json replaces it"
                );
                rules.omit.push(Omission {
                    path: root_path,
                    reason: reason.clone(),
                });
                return Ok(Some(reason));
            }
            Err(Failure::Invalid(reason)) => {
                return Err(unresolved(
                    root_path,
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
        path: root_path,
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
        .retain(|file| file.path != format!("worktree/{root_path}"));
    rules.generated.push(Generated {
        path: format!("worktree/{root_path}"),
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
    Ok(None)
}

/// Choose scip-typescript's project config and adjust its `rules` (#717): the override, else
/// the root `tsconfig.json`, else the root `jsconfig.json`, else the generated tsconfig
/// (#652). A root config's package `extends` is supplied from the checkout's `node_modules`
/// (#799). Returns the `typescript.project` setting.
pub fn apply(
    rules: &mut Rules,
    inputs: &RetainedInputs,
    workspace: &Path,
    overridden: Option<TsProject>,
) -> Result<Setting> {
    let find = |path: &str| inputs.config.iter().find(|file| file.path == path);
    let (project, source) = match overridden {
        Some(project) => (project, Source::Override),
        None if find(TSCONFIG).is_some() => (TsProject::Tsconfig, Source::Project),
        None if find(JSCONFIG).is_some() => (TsProject::Jsconfig, Source::Project),
        None => (TsProject::Generated, Source::Default),
    };
    let given = if source == Source::Override {
        format!("given with --ts-project {}; ", project.as_str())
    } else {
        String::new()
    };
    let missing = |path: &str| {
        anyhow::anyhow!(
            "{CONFIG_UNRESOLVED}: --ts-project {path}, but the checkout has no root {path}"
        )
    };
    let setting = |value: TsProject, from: Option<&RootInput>, detail: String| Setting {
        name: "typescript.project",
        value: Value::from(value.as_str()),
        source,
        from: from.map(Origin::of).into_iter().collect(),
        detail: format!("{given}{detail}"),
    };
    match project {
        TsProject::Generated => {
            // The generated tsconfig extends nothing, so a root config it replaces is left out
            // of the worktree: it is never read, and one over the config input cap cannot
            // refuse the run (#822 review).
            for path in [TSCONFIG, JSCONFIG] {
                if find(path).is_some() {
                    rules.omit.push(Omission {
                        path,
                        reason: format!(
                            "--ts-project generated: the generated tsconfig.json is used instead of the root {path}"
                        ),
                    });
                }
            }
            Ok(setting(
                project,
                None,
                if source == Source::Default {
                    "no root tsconfig.json or jsconfig.json: the generated tsconfig.json".into()
                } else {
                    "the generated tsconfig.json".into()
                },
            ))
        }
        TsProject::Tsconfig => {
            let root = find(TSCONFIG).ok_or_else(|| missing(TSCONFIG))?;
            let resolved = resolve_root(rules, root, TSCONFIG, workspace)?;
            // An explicit choice leaves out the root jsconfig.json it does not use, unless the
            // tsconfig.json extends it (#822 review).
            if source == Source::Override
                && find(JSCONFIG).is_some()
                && !extends_root(&root.bytes, JSCONFIG)
            {
                rules.omit.push(Omission {
                    path: JSCONFIG,
                    reason: "--ts-project tsconfig.json: the root jsconfig.json is not used".into(),
                });
            }
            Ok(match resolved {
                Some(reason) => setting(
                    TsProject::Generated,
                    Some(root),
                    format!("root tsconfig.json {reason}"),
                ),
                None => setting(project, Some(root), "the root tsconfig.json".into()),
            })
        }
        TsProject::Jsconfig => {
            let root = find(JSCONFIG).ok_or_else(|| missing(JSCONFIG))?;
            if find(TSCONFIG).is_some() {
                rules.omit.push(Omission {
                    path: TSCONFIG,
                    reason: "--ts-project jsconfig.json: a generated tsconfig.json extending the root jsconfig.json replaces it".into(),
                });
            }
            if let Some(reason) = resolve_root(rules, root, JSCONFIG, workspace)? {
                return Ok(setting(
                    TsProject::Generated,
                    Some(root),
                    format!("root jsconfig.json {reason}"),
                ));
            }
            rules
                .generated
                .retain(|file| file.path != format!("worktree/{TSCONFIG}"));
            rules.generated.push(Generated {
                path: format!("worktree/{TSCONFIG}"),
                bytes: JSCONFIG_PROJECT.to_vec(),
                default_only: false,
            });
            Ok(setting(
                project,
                Some(root),
                "the root jsconfig.json, through a generated tsconfig.json that extends it with allowJs, since scip-typescript reads only tsconfig.json".into(),
            ))
        }
    }
}
