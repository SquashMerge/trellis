//! App detection from build manifests (#784, #807, #854): which `Cargo.toml`, `package.json`,
//! `pyproject.toml`, `pom.xml` and Gradle build files name a runnable application, and how apps
//! authored in the rules file combine with them (#794). Pure: it reads manifest bytes and the
//! snapshot's document paths, and does no I/O.

mod java;

use crate::references::external::ManifestInput;
use crate::references::work::Work;
use anyhow::{Result, ensure};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AppKind {
    Cargo,
    Npm,
    Python,
    Maven,
    Gradle,
}

/// Where an app comes from: a build manifest, or an authored `[apps.<name>]` table (#794).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AppSource {
    Manifest,
    Override,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct App {
    pub name: String,
    pub source: AppSource,
    /// `None` for an authored app, which has no manifest.
    pub kind: Option<AppKind>,
    pub manifest: Option<String>,
    /// `.` or checkout-relative directories. A detected app has exactly one, its manifest's
    /// directory; an authored app has the roots it lists.
    pub roots: Vec<String>,
    pub entries: Vec<String>,
}

impl App {
    /// A detected app: its root is its manifest's directory.
    pub(crate) fn detected(
        name: String,
        kind: AppKind,
        manifest: &str,
        entries: Vec<String>,
    ) -> Self {
        Self {
            name,
            source: AppSource::Manifest,
            kind: Some(kind),
            manifest: Some(manifest.to_owned()),
            roots: vec![directory(manifest)],
            entries,
        }
    }

    /// An authored app (#794): no kind, manifest or entries.
    pub(crate) fn authored(name: String, roots: Vec<String>) -> Self {
        Self {
            name,
            source: AppSource::Override,
            kind: None,
            manifest: None,
            roots,
            entries: Vec::new(),
        }
    }
}

/// The text of an app's `trellis_references` `groupBy:"package"` group (#837): a detected
/// app's manifest path, or `[apps.<name>]` for an authored app, which has none. A manifest
/// path ends in its manifest file name and an authored group in `]`, and neither is the
/// `noApp` group, so no two kinds of group collide.
pub(crate) fn group_text(app: &App) -> String {
    match &app.manifest {
        Some(manifest) => manifest.clone(),
        None => format!("[apps.{}]", app.name),
    }
}

/// What one manifest detects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    App(App),
    NotApp,
    /// Malformed, over the config input cap, or otherwise unreadable.
    Unread,
}

/// Every listed manifest's outcome, ascending by manifest path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Candidates {
    pub manifests: Vec<(String, Outcome)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Detected {
    /// In manifest-path order. Apps that share a name are all kept.
    pub apps: Vec<App>,
    pub unread: usize,
}

/// The outcome of every build manifest in the snapshot. Config-free, so it parses manifests
/// a rules file would hide too; [`detect`] discards those.
pub(crate) fn candidates(
    manifests: &[ManifestInput],
    documents: &BTreeSet<&str>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Candidates> {
    let mut listed: Vec<&ManifestInput> = manifests
        .iter()
        .filter(|manifest| {
            let path = manifest.path.as_str();
            let java_settings = matches!(path, "settings.gradle" | "settings.gradle.kts");
            (java_settings
                || matches!(
                    file_name(path),
                    "Cargo.toml"
                        | "package.json"
                        | "pyproject.toml"
                        | "pom.xml"
                        | "build.gradle"
                        | "build.gradle.kts"
                ))
                && !path.split('/').any(|part| part == "node_modules")
        })
        .collect();
    listed.sort_by(|a, b| a.path.cmp(&b.path));
    // The root manifest is parsed once: it names the workspace members and is itself a
    // candidate.
    let mut root_parsed = None;
    let mut members = None;
    if let Some(root) = listed.iter().find(|manifest| manifest.path == "Cargo.toml") {
        let parsed = parse(root, work)?;
        if let Some(Parsed::Toml(table)) = &parsed {
            members = Members::from_workspace(table);
        }
        root_parsed = Some(parsed);
    }
    let mut java = maven_outcomes(&listed, work, check)?;
    java.extend(gradle_outcomes(&listed, work, check)?);
    let mut out = Vec::with_capacity(listed.len());
    for manifest in listed {
        check()?;
        let path = manifest.path.as_str();
        let outcome = if let Some(outcome) = java.remove(path) {
            outcome
        } else if file_name(path) == "package.json" {
            npm_outcome(manifest, parse(manifest, work)?)
        } else if file_name(path) == "pyproject.toml" {
            python_outcome(manifest, parse(manifest, work)?)
        } else if path == "Cargo.toml" {
            cargo_outcome(manifest, root_parsed.take().flatten(), documents)
        } else if members
            .as_ref()
            .is_some_and(|members| members.contains(&directory(path)))
        {
            cargo_outcome(manifest, parse(manifest, work)?, documents)
        } else {
            Outcome::NotApp
        };
        out.push((path.to_owned(), outcome));
    }
    Ok(Candidates { manifests: out })
}

/// The apps detected after dropping every manifest in `hidden`.
pub(crate) fn detect(candidates: &Candidates, hidden: &BTreeSet<String>) -> Detected {
    let mut detected = Detected::default();
    for (path, outcome) in &candidates.manifests {
        if hidden.contains(path) {
            continue;
        }
        match outcome {
            Outcome::App(app) => detected.apps.push(app.clone()),
            Outcome::NotApp => {}
            Outcome::Unread => detected.unread += 1,
        }
    }
    detected
}

/// The effective app inventory (#794): the `authored` apps, in name order, then every
/// `detected` app whose root is not equal to or under an authored root, in manifest-path order.
pub(crate) fn effective(authored: Vec<App>, detected: Vec<App>) -> Vec<App> {
    let shadowing: Vec<&str> = authored
        .iter()
        .flat_map(|app| app.roots.iter().map(String::as_str))
        .collect();
    let surviving: Vec<App> = detected
        .into_iter()
        .filter(|app| {
            !app.roots.iter().any(|root| {
                shadowing
                    .iter()
                    .any(|authored| depth(authored, root).is_some())
            })
        })
        .collect();
    authored.into_iter().chain(surviving).collect()
}

/// How specifically `root` contains `path`: `None` when it does not. `.` contains every path.
fn depth(root: &str, path: &str) -> Option<usize> {
    if root == "." {
        Some(0)
    } else if path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
    {
        Some(root.len() + 1)
    } else {
        None
    }
}

/// The index of the app with the most specific root containing `path`; on a tie, the first.
/// `.` contains every path.
pub(crate) fn containing(apps: &[App], path: &str) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (index, app) in apps.iter().enumerate() {
        for root in &app.roots {
            let Some(depth) = depth(root, path) else {
                continue;
            };
            if best.is_none_or(|(_, deepest)| depth > deepest) {
                best = Some((index, depth));
            }
        }
    }
    best.map(|(index, _)| index)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The directory of a manifest path: `.` at the root.
fn directory(path: &str) -> String {
    path.rsplit_once('/')
        .map_or(".", |(directory, _)| directory)
        .to_owned()
}

enum Parsed {
    Toml(toml::Table),
    Json(serde_json::Value),
}

/// `None` when the manifest is absent or unreadable. Parsing charges the manifest's bytes.
fn parse(manifest: &ManifestInput, work: &mut Work) -> Result<Option<Parsed>> {
    let Some(bytes) = &manifest.bytes else {
        return Ok(None);
    };
    ensure!(
        work.charge(bytes.len()),
        "bounded_work: app manifest parsing"
    );
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(None);
    };
    Ok(if file_name(&manifest.path).ends_with(".toml") {
        toml::from_str::<toml::Table>(text).ok().map(Parsed::Toml)
    } else {
        serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .map(Parsed::Json)
    })
}

fn npm_outcome(manifest: &ManifestInput, parsed: Option<Parsed>) -> Outcome {
    if manifest.bytes.is_none() {
        return Outcome::NotApp;
    }
    let Some(Parsed::Json(serde_json::Value::Object(object))) = parsed else {
        return Outcome::Unread;
    };
    let name = match object.get("name").and_then(|name| name.as_str()) {
        Some(name) if !name.trim().is_empty() => name.to_owned(),
        _ => directory(&manifest.path),
    };
    Outcome::App(App::detected(
        name,
        AppKind::Npm,
        &manifest.path,
        Vec::new(),
    ))
}

/// A `pyproject.toml` is an app when it declares a project: a `[project]` table with a
/// nonblank string `name`, or, without `[project]`, a Poetry `[tool.poetry]` table with one.
/// Its entries are the modules of its `[project.scripts]`, `[project.gui-scripts]` and
/// `[tool.poetry.scripts]` targets. Every `pyproject.toml` is judged on its own: a uv
/// `[tool.uv.workspace]` changes nothing, because a member, an excluded project and a
/// project outside the workspace are each an app exactly when they declare a project, and a
/// virtual workspace root declares none.
fn python_outcome(manifest: &ManifestInput, parsed: Option<Parsed>) -> Outcome {
    if manifest.bytes.is_none() {
        return Outcome::NotApp;
    }
    let Some(Parsed::Toml(table)) = parsed else {
        return Outcome::Unread;
    };
    let Ok(project) = subtable(&table, "project") else {
        return Outcome::Unread;
    };
    let Ok(tool) = subtable(&table, "tool") else {
        return Outcome::Unread;
    };
    let Ok(poetry) = tool.map_or(Ok(None), |tool| subtable(tool, "poetry")) else {
        return Outcome::Unread;
    };
    let declared = match (project, poetry) {
        (Some(project), _) => project,
        (None, Some(poetry)) if poetry.contains_key("name") => poetry,
        // A tool-only file, such as Poetry's `package-mode = false`.
        (None, _) => return Outcome::NotApp,
    };
    let name = match declared.get("name").and_then(|name| name.as_str()) {
        Some(name) if !name.trim().is_empty() => name.to_owned(),
        _ => return Outcome::Unread,
    };
    let mut entries = BTreeSet::new();
    // Standard `[project]` scripts are `module:attr` strings only; table targets are Poetry's.
    let tables = [
        (project, "scripts", false),
        (project, "gui-scripts", false),
        (poetry, "scripts", true),
    ];
    for (parent, key, is_poetry) in tables {
        let Ok(scripts) = parent.map_or(Ok(None), |parent| subtable(parent, key)) else {
            return Outcome::Unread;
        };
        for target in scripts.into_iter().flat_map(|scripts| scripts.values()) {
            let module = match (target, is_poetry) {
                (toml::Value::String(reference), false) => {
                    // `module:attr [extra]`: both sides of the colon are required.
                    match reference.split_once(':') {
                        Some((module, attr))
                            if !module.trim().is_empty() && !attr.trim().is_empty() =>
                        {
                            module.trim()
                        }
                        _ => return Outcome::Unread,
                    }
                }
                (toml::Value::String(reference), true) => {
                    reference.split(':').next().unwrap_or_default().trim()
                }
                // Poetry's `{ callable = "module:function" }`; a file script has no module.
                (toml::Value::Table(script), true) => match script.get("callable") {
                    Some(toml::Value::String(reference)) => {
                        reference.split(':').next().unwrap_or_default().trim()
                    }
                    Some(_) => return Outcome::Unread,
                    None => continue,
                },
                _ => return Outcome::Unread,
            };
            if module.is_empty() {
                return Outcome::Unread;
            }
            entries.insert(module.to_owned());
        }
    }
    Outcome::App(App::detected(
        name,
        AppKind::Python,
        &manifest.path,
        entries.into_iter().collect(),
    ))
}

/// The table at `key`: `Ok(None)` when absent, `Err` when it is not a table.
fn subtable<'a>(table: &'a toml::Table, key: &str) -> Result<Option<&'a toml::Table>, ()> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Table(value)) => Ok(Some(value)),
        Some(_) => Err(()),
    }
}

fn cargo_outcome(
    manifest: &ManifestInput,
    parsed: Option<Parsed>,
    documents: &BTreeSet<&str>,
) -> Outcome {
    if manifest.bytes.is_none() {
        return Outcome::NotApp;
    }
    let Some(Parsed::Toml(table)) = parsed else {
        return Outcome::Unread;
    };
    let Some(package) = table.get("package") else {
        return Outcome::NotApp;
    };
    let Some(package) = package.as_table() else {
        return Outcome::Unread;
    };
    let Some(name) = package.get("name").and_then(|name| name.as_str()) else {
        return Outcome::Unread;
    };
    let root = directory(&manifest.path);
    let within = |relative: &str| {
        if root == "." {
            relative.to_owned()
        } else {
            format!("{root}/{relative}")
        }
    };
    let mut entries = BTreeSet::new();
    let mut named_paths = BTreeSet::new();
    match table.get("bin") {
        None => {}
        Some(toml::Value::Array(bins)) => {
            for bin in bins {
                let Some(bin) = bin.as_table() else {
                    return Outcome::Unread;
                };
                let Some(bin_name) = bin.get("name").and_then(|name| name.as_str()) else {
                    return Outcome::Unread;
                };
                entries.insert(bin_name.to_owned());
                if let Some(path) = bin.get("path").and_then(|path| path.as_str()) {
                    named_paths.insert(normalize(&within(path)));
                }
            }
        }
        Some(_) => return Outcome::Unread,
    }
    if package.get("autobins").and_then(|value| value.as_bool()) != Some(false) {
        if documents.contains(within("src/main.rs").as_str()) {
            entries.insert(name.to_owned());
        }
        let prefix = within("src/bin/");
        for document in documents.range::<&str, _>(prefix.as_str()..) {
            let Some(rest) = document.strip_prefix(prefix.as_str()) else {
                break;
            };
            if let Some(stem) = rest.strip_suffix(".rs")
                && !stem.is_empty()
                && !stem.contains('/')
                && !named_paths.contains(*document)
            {
                entries.insert(stem.to_owned());
            }
        }
    }
    if entries.is_empty() {
        return Outcome::NotApp;
    }
    Outcome::App(App::detected(
        name.to_owned(),
        AppKind::Cargo,
        &manifest.path,
        entries.into_iter().collect(),
    ))
}

/// Resolve `.` and `..` components of a checkout-relative path.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

/// A manifest's text, charging its bytes: `Ok(None)` when it is not UTF-8.
fn text<'a>(manifest: &'a ManifestInput, work: &mut Work) -> Result<Option<&'a str>> {
    let bytes = manifest.bytes.as_deref().unwrap_or_default();
    ensure!(
        work.charge(bytes.len()),
        "bounded_work: app manifest parsing"
    );
    Ok(std::str::from_utf8(bytes).ok())
}

/// `relative` joined to the checkout-relative directory `dir` (`.` at the root), with `.` and
/// `..` resolved; `None` when it leaves the checkout or is absolute.
fn join_dir(dir: &str, relative: &str) -> Option<String> {
    if relative.starts_with('/') || relative.contains('\\') {
        return None;
    }
    let mut parts: Vec<&str> = if dir == "." {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    })
}

/// Whether some proper ancestor directory of `dir` is in `dirs`. `.` is every other
/// directory's ancestor.
fn has_ancestor<T>(dir: &str, dirs: &BTreeMap<String, T>) -> bool {
    if dir == "." {
        return false;
    }
    dirs.contains_key(".")
        || dir
            .match_indices('/')
            .any(|(at, _)| dirs.contains_key(&dir[..at]))
}

/// The outcome of every listed `pom.xml` (#854). A present POM with no present POM in an
/// ancestor directory is judged alone; below one, a POM is judged only when a judged POM lists
/// its directory in `<modules>`. A judged POM is an app (`maven`) when it is runnable: packaging
/// `war`, or `jar` (the default) with a main class declared through `maven-jar-plugin`'s
/// manifest `mainClass`, `spring-boot-maven-plugin` or `exec-maven-plugin`'s `mainClass`. Its
/// name is `artifactId`, with no fallback to a parent's; its entries are the literal main
/// classes. An aggregator (`pom`) or library jar is no app; every other POM is not judged.
fn maven_outcomes(
    listed: &[&ManifestInput],
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<BTreeMap<String, Outcome>> {
    let mut outcomes = BTreeMap::new();
    let mut present = BTreeMap::new();
    for manifest in listed.iter().filter(|m| file_name(&m.path) == "pom.xml") {
        if manifest.bytes.is_some() {
            present.insert(directory(&manifest.path), *manifest);
        }
        outcomes.insert(manifest.path.clone(), Outcome::NotApp);
    }
    let mut queue: VecDeque<String> = present
        .keys()
        .filter(|dir| !has_ancestor(dir, &present))
        .cloned()
        .collect();
    let mut judged: BTreeSet<String> = queue.iter().cloned().collect();
    while let Some(dir) = queue.pop_front() {
        check()?;
        let manifest = present[&dir];
        let Some(pom) = text(manifest, work)?.and_then(java::pom) else {
            outcomes.insert(manifest.path.clone(), Outcome::Unread);
            continue;
        };
        for module in &pom.modules {
            // A module names its directory, or a POM file in it.
            let module = module.strip_suffix("/pom.xml").unwrap_or(module);
            if let Some(module) = join_dir(&dir, module)
                && present.contains_key(&module)
                && judged.insert(module.clone())
            {
                queue.push_back(module);
            }
        }
        if pom.runnable {
            outcomes.insert(
                manifest.path.clone(),
                Outcome::App(App::detected(
                    pom.artifact_id,
                    AppKind::Maven,
                    &manifest.path,
                    pom.main_classes,
                )),
            );
        }
    }
    Ok(outcomes)
}

/// The outcome of every listed Gradle file (#854). The root `settings.gradle` and
/// `settings.gradle.kts` are never apps; an unreadable one is unread and includes nothing. A
/// build file (`build.gradle` or `build.gradle.kts`) is judged when it is the root project's,
/// when its directory is a project the root settings `include` (`:lib:core` is `lib/core`), or,
/// without root settings, when no ancestor directory holds a present build file. A judged build
/// file is an app (`gradle`) when it applies the `application` plugin or assigns `mainClass` or
/// `mainClassName`. Its name is Gradle's project name: the root project's literal
/// `rootProject.name`, else `.`; an included project's last path segment; a standalone nested
/// project's directory name. Its entries are the literal main classes.
fn gradle_outcomes(
    listed: &[&ManifestInput],
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<BTreeMap<String, Outcome>> {
    let mut outcomes = BTreeMap::new();
    let mut root_name = None;
    let mut includes = Vec::new();
    let mut settings_present = false;
    for manifest in listed
        .iter()
        .filter(|m| matches!(m.path.as_str(), "settings.gradle" | "settings.gradle.kts"))
    {
        check()?;
        if manifest.bytes.is_none() {
            outcomes.insert(manifest.path.clone(), Outcome::NotApp);
            continue;
        }
        settings_present = true;
        if manifest.bytes.as_deref() == Some(crate::skipped_config::PLACEHOLDER) {
            outcomes.insert(manifest.path.clone(), Outcome::Unread);
            continue;
        }
        match text(manifest, work)?.and_then(java::gradle_settings) {
            None => {
                outcomes.insert(manifest.path.clone(), Outcome::Unread);
            }
            Some(settings) => {
                root_name = root_name.or(settings.root_name);
                includes.extend(settings.includes);
                outcomes.insert(manifest.path.clone(), Outcome::NotApp);
            }
        }
    }
    let mut present: BTreeMap<String, Vec<&ManifestInput>> = BTreeMap::new();
    for manifest in listed
        .iter()
        .filter(|m| matches!(file_name(&m.path), "build.gradle" | "build.gradle.kts"))
    {
        outcomes.insert(manifest.path.clone(), Outcome::NotApp);
        if manifest.bytes.is_some() {
            present
                .entry(directory(&manifest.path))
                .or_default()
                .push(*manifest);
        }
    }
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    if present.contains_key(".") {
        names.insert(".".into(), root_name.unwrap_or_else(|| ".".into()));
    }
    for include in &includes {
        let segments: Vec<&str> = include.split(':').filter(|s| !s.is_empty()).collect();
        let (Some(last), Some(dir)) = (segments.last(), join_dir(".", &segments.join("/"))) else {
            continue;
        };
        if dir != "." && present.contains_key(&dir) {
            names.entry(dir).or_insert_with(|| (*last).to_owned());
        }
    }
    if !settings_present {
        for dir in present.keys() {
            if dir != "." && !has_ancestor(dir, &present) {
                names.insert(dir.clone(), file_name(dir).to_owned());
            }
        }
    }
    for (dir, name) in names {
        for manifest in &present[&dir] {
            check()?;
            let build = if manifest.bytes.as_deref() == Some(crate::skipped_config::PLACEHOLDER) {
                None
            } else {
                text(manifest, work)?.and_then(java::gradle_build)
            };
            let outcome = match build {
                None => Outcome::Unread,
                Some(build) if build.runnable => Outcome::App(App::detected(
                    name.clone(),
                    AppKind::Gradle,
                    &manifest.path,
                    build.main_classes,
                )),
                Some(_) => Outcome::NotApp,
            };
            outcomes.insert(manifest.path.clone(), outcome);
        }
    }
    Ok(outcomes)
}

/// The `[workspace]` members of the root manifest, minus its excludes.
struct Members {
    members: GlobSet,
    exclude: GlobSet,
}

impl Members {
    fn from_workspace(root: &toml::Table) -> Option<Self> {
        let workspace = root.get("workspace")?.as_table()?;
        let build = |key: &str| {
            let mut set = GlobSetBuilder::new();
            for pattern in workspace
                .get(key)
                .and_then(|value| value.as_array())
                .into_iter()
                .flatten()
                .filter_map(|value| value.as_str())
            {
                let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
                if let Ok(glob) = GlobBuilder::new(pattern).literal_separator(true).build() {
                    set.add(glob);
                }
            }
            set.build().ok()
        };
        Some(Self {
            members: build("members")?,
            exclude: build("exclude")?,
        })
    }

    fn contains(&self, directory: &str) -> bool {
        self.members.is_match(directory) && !self.exclude.is_match(directory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skipped_config::PLACEHOLDER;

    fn manifest(path: &str, bytes: Option<&[u8]>) -> ManifestInput {
        ManifestInput {
            source_set_id: String::new(),
            native_revision_id: String::new(),
            dependency_hash: String::new(),
            path: path.into(),
            content_hash: None,
            bytes: bytes.map(<[u8]>::to_vec),
        }
    }

    fn run(manifests: &[ManifestInput], documents: &[&str]) -> Candidates {
        let documents: BTreeSet<&str> = documents.iter().copied().collect();
        candidates(
            manifests,
            &documents,
            &mut Work::new(usize::MAX),
            &mut || Ok(()),
        )
        .unwrap()
    }

    fn outcome<'a>(found: &'a Candidates, path: &str) -> &'a Outcome {
        &found
            .manifests
            .iter()
            .find(|(candidate, _)| candidate == path)
            .unwrap_or_else(|| panic!("{path} is not listed"))
            .1
    }

    fn app(found: &Candidates, path: &str) -> App {
        match outcome(found, path) {
            Outcome::App(app) => app.clone(),
            other => panic!("{path}: {other:?}"),
        }
    }

    const PACKAGE: &[u8] = b"[package]\nname = \"daemon\"\n";

    #[test]
    fn cargo_root_with_main_and_explicit_bin_is_an_app() {
        let bytes =
            b"[package]\nname = \"daemon\"\n[[bin]]\nname = \"tool\"\npath = \"tools/tool.rs\"\n";
        let found = run(
            &[manifest("Cargo.toml", Some(bytes))],
            &["src/main.rs", "src/lib.rs"],
        );
        let app = app(&found, "Cargo.toml");
        assert_eq!(app.name, "daemon");
        assert_eq!(app.kind, Some(AppKind::Cargo));
        assert_eq!(app.roots, ["."]);
        assert_eq!(app.entries, ["daemon", "tool"]);
    }

    #[test]
    fn cargo_src_bin_files_are_entries_unless_a_bin_names_the_path() {
        let bytes = b"[package]\nname = \"daemon\"\n[[bin]]\nname = \"renamed\"\npath = \"./src/bin/b.rs\"\n";
        let found = run(
            &[manifest("Cargo.toml", Some(bytes))],
            &[
                "src/bin/a.rs",
                "src/bin/b.rs",
                "src/bin/nested/c.rs",
                "src/bin/d.txt",
            ],
        );
        assert_eq!(app(&found, "Cargo.toml").entries, ["a", "renamed"]);
    }

    #[test]
    fn autobins_false_drops_inferred_entries() {
        let bytes = b"[package]\nname = \"daemon\"\nautobins = false\n";
        let found = run(
            &[manifest("Cargo.toml", Some(bytes))],
            &["src/main.rs", "src/bin/a.rs"],
        );
        assert_eq!(outcome(&found, "Cargo.toml"), &Outcome::NotApp);
    }

    #[test]
    fn cargo_package_with_no_entries_is_not_an_app() {
        let found = run(&[manifest("Cargo.toml", Some(PACKAGE))], &["src/lib.rs"]);
        assert_eq!(outcome(&found, "Cargo.toml"), &Outcome::NotApp);
    }

    #[test]
    fn virtual_workspace_is_not_an_app_but_members_are_detected() {
        let root =
            b"[workspace]\nmembers = [\"crates/*\", \"tools/one\"]\nexclude = [\"crates/skip\"]\n";
        let found = run(
            &[
                manifest("Cargo.toml", Some(root)),
                manifest(
                    "crates/alpha/Cargo.toml",
                    Some(b"[package]\nname = \"alpha\"\n"),
                ),
                manifest(
                    "crates/skip/Cargo.toml",
                    Some(b"[package]\nname = \"skip\"\n"),
                ),
                manifest(
                    "crates/deep/inner/Cargo.toml",
                    Some(b"[package]\nname = \"deep\"\n"),
                ),
                manifest("tools/one/Cargo.toml", Some(b"[package]\nname = \"one\"\n")),
                manifest("crates/virtual/Cargo.toml", Some(b"[workspace]\n")),
                manifest("crates/broken/Cargo.toml", Some(b"[package\n")),
                manifest("crates/old/Cargo.toml", Some(PLACEHOLDER)),
                manifest("other/Cargo.toml", Some(b"this is not toml [")),
            ],
            &[
                "crates/alpha/src/main.rs",
                "crates/skip/src/main.rs",
                "crates/deep/inner/src/main.rs",
                "tools/one/src/main.rs",
            ],
        );
        assert_eq!(outcome(&found, "Cargo.toml"), &Outcome::NotApp);
        assert_eq!(
            app(&found, "crates/alpha/Cargo.toml").roots,
            ["crates/alpha"]
        );
        assert_eq!(app(&found, "tools/one/Cargo.toml").name, "one");
        // Excluded, too deep for `*`, and outside the members: never parsed for identity.
        for path in [
            "crates/skip/Cargo.toml",
            "crates/deep/inner/Cargo.toml",
            "other/Cargo.toml",
        ] {
            assert_eq!(outcome(&found, path), &Outcome::NotApp, "{path}");
        }
        assert_eq!(
            outcome(&found, "crates/virtual/Cargo.toml"),
            &Outcome::NotApp
        );
        assert_eq!(
            outcome(&found, "crates/broken/Cargo.toml"),
            &Outcome::Unread
        );
        assert_eq!(outcome(&found, "crates/old/Cargo.toml"), &Outcome::Unread);
        let paths: Vec<&str> = found.manifests.iter().map(|(p, _)| p.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn root_package_can_also_be_a_workspace_root() {
        let root = b"[package]\nname = \"daemon\"\n[workspace]\nmembers = [\"member\"]\n";
        let found = run(
            &[
                manifest("Cargo.toml", Some(root)),
                manifest("member/Cargo.toml", Some(b"[package]\nname = \"member\"\n")),
            ],
            &["src/main.rs", "member/src/main.rs"],
        );
        assert_eq!(app(&found, "Cargo.toml").name, "daemon");
        assert_eq!(app(&found, "member/Cargo.toml").name, "member");
    }

    #[test]
    fn nested_cargo_manifest_outside_a_workspace_is_never_parsed() {
        let found = run(
            &[
                manifest("Cargo.toml", Some(PACKAGE)),
                manifest("tools/x/Cargo.toml", Some(b"[package]\nname = \"x\"\n")),
                manifest("tools/y/Cargo.toml", Some(b"not toml [")),
            ],
            &["src/main.rs", "tools/x/src/main.rs"],
        );
        assert_eq!(outcome(&found, "tools/x/Cargo.toml"), &Outcome::NotApp);
        assert_eq!(outcome(&found, "tools/y/Cargo.toml"), &Outcome::NotApp);
    }

    #[test]
    fn unreadable_root_cargo_manifests_are_unread() {
        for bytes in [
            &b"[package\n"[..],
            PLACEHOLDER,
            b"[package]\nversion = \"1\"\n",
            b"[package]\nname = 3\n",
            b"[package]\nname = \"a\"\n[[bin]]\npath = \"x.rs\"\n",
            b"[package]\nname = \"a\"\n[[bin]]\nname = 4\n",
            &[0xff, 0xfe],
        ] {
            let found = run(&[manifest("Cargo.toml", Some(bytes))], &["src/main.rs"]);
            assert_eq!(outcome(&found, "Cargo.toml"), &Outcome::Unread, "{bytes:?}");
        }
    }

    #[test]
    fn unreadable_root_workspace_leaves_members_not_apps() {
        let found = run(
            &[
                manifest("Cargo.toml", Some(PLACEHOLDER)),
                manifest("member/Cargo.toml", Some(PACKAGE)),
            ],
            &["member/src/main.rs"],
        );
        assert_eq!(outcome(&found, "Cargo.toml"), &Outcome::Unread);
        assert_eq!(outcome(&found, "member/Cargo.toml"), &Outcome::NotApp);
    }

    #[test]
    fn absent_manifests_are_not_apps_never_unread() {
        let found = run(
            &[manifest("package.json", None), manifest("Cargo.toml", None)],
            &[],
        );
        assert_eq!(outcome(&found, "package.json"), &Outcome::NotApp);
        assert_eq!(outcome(&found, "Cargo.toml"), &Outcome::NotApp);
        assert_eq!(detect(&found, &BTreeSet::new()).unread, 0);
    }

    #[test]
    fn npm_root_and_nested_manifests_are_apps() {
        let found = run(
            &[
                manifest("package.json", Some(br#"{"name":"web"}"#)),
                manifest("tests/package.json", Some(br#"{"name":"  "}"#)),
                manifest("tools/package.json", Some(b"{}")),
                manifest("node_modules/dep/package.json", Some(b"{}")),
                manifest("a/node_modules/dep/package.json", Some(b"[")),
            ],
            &[],
        );
        let root = app(&found, "package.json");
        assert_eq!(
            (
                root.name.as_str(),
                root.roots[0].as_str(),
                root.kind.unwrap()
            ),
            ("web", ".", AppKind::Npm)
        );
        assert!(root.entries.is_empty());
        assert_eq!(app(&found, "tests/package.json").name, "tests");
        assert_eq!(app(&found, "tools/package.json").name, "tools");
        assert_eq!(found.manifests.len(), 3, "{found:?}");
    }

    #[test]
    fn unreadable_package_json_is_unread() {
        for bytes in [&b"[1]"[..], b"{", b"\"x\"", b"null", PLACEHOLDER] {
            let found = run(&[manifest("package.json", Some(bytes))], &[]);
            assert_eq!(
                outcome(&found, "package.json"),
                &Outcome::Unread,
                "{bytes:?}"
            );
        }
    }

    #[test]
    fn detect_drops_hidden_manifests_and_keeps_duplicate_names() {
        let found = run(
            &[
                manifest("package.json", Some(br#"{"name":"same"}"#)),
                manifest("a/package.json", Some(br#"{"name":"same"}"#)),
                manifest("fixtures/package.json", Some(b"{")),
                manifest("fixtures/ok/package.json", Some(b"{}")),
                manifest("other/package.json", Some(b"{")),
            ],
            &[],
        );
        let hidden: BTreeSet<String> = ["fixtures/package.json", "fixtures/ok/package.json"]
            .map(String::from)
            .into();
        let detected = detect(&found, &hidden);
        let manifests: Vec<&str> = detected
            .apps
            .iter()
            .map(|a| a.manifest.as_deref().unwrap())
            .collect();
        assert_eq!(manifests, ["a/package.json", "package.json"]);
        assert_eq!(detected.apps[0].name, detected.apps[1].name);
        assert_eq!(detected.unread, 1);
    }

    #[test]
    fn containing_picks_the_most_specific_root() {
        let make = |root: &str| App::authored(root.into(), vec![root.into()]);
        let apps = [make("."), make("web"), make("web/inner"), make("webx")];
        assert_eq!(containing(&apps, "README.md"), Some(0));
        assert_eq!(containing(&apps, "web/app.js"), Some(1));
        assert_eq!(containing(&apps, "web/inner/a.js"), Some(2));
        assert_eq!(containing(&apps, "web"), Some(1));
        assert_eq!(containing(&apps, "webx/a.js"), Some(3));
        assert_eq!(containing(&apps[1..], "README.md"), None);
        assert_eq!(containing(&[], "a"), None);
    }

    fn npm(name: &str, root: &str) -> App {
        let manifest = if root == "." {
            "package.json".to_owned()
        } else {
            format!("{root}/package.json")
        };
        App::detected(name.into(), AppKind::Npm, &manifest, Vec::new())
    }

    fn names(apps: &[App]) -> Vec<&str> {
        apps.iter().map(|app| app.name.as_str()).collect()
    }

    /// #794 D2: an authored root shadows detected apps at or under it; the rest survive, and
    /// authored apps come first.
    #[test]
    fn authored_roots_shadow_equal_and_nested_detected_roots_only() {
        let detected = vec![
            npm("root", "."),
            npm("client", "client"),
            npm("deep", "client/inner"),
            npm("clientele", "clientele"),
            npm("other", "other"),
        ];
        let shadowed = effective(
            vec![App::authored("web".into(), vec!["client".into()])],
            detected.clone(),
        );
        assert_eq!(names(&shadowed), ["web", "root", "clientele", "other"]);
        assert_eq!(shadowed[0].source, AppSource::Override);
        assert_eq!(shadowed[0].kind, None);
        assert_eq!(shadowed[0].manifest, None);
        // `.` shadows every detected app.
        let all = effective(
            vec![App::authored("all".into(), vec![".".into()])],
            detected.clone(),
        );
        assert_eq!(names(&all), ["all"]);
        // Without authored apps the inventory is detection, unchanged.
        assert_eq!(effective(Vec::new(), detected.clone()), detected);
    }

    /// #794 D3: authored roots may nest and an app may have several roots; the most specific
    /// root wins, across apps and detected roots alike.
    #[test]
    fn containing_reads_every_root_of_every_app() {
        let apps = effective(
            vec![
                App::authored("backend".into(), vec![".".into()]),
                App::authored("web".into(), vec!["client".into(), "shared/ui".into()]),
            ],
            vec![npm("root", "."), npm("plugin", "plugins/p")],
        );
        assert_eq!(names(&apps), ["backend", "web"]);
        assert_eq!(containing(&apps, "server/main.js"), Some(0));
        assert_eq!(containing(&apps, "client/app.js"), Some(1));
        assert_eq!(containing(&apps, "shared/ui/button.js"), Some(1));
        assert_eq!(containing(&apps, "shared/model.js"), Some(0));
        let mixed = effective(
            vec![App::authored("ui".into(), vec!["ui".into()])],
            vec![App::detected(
                "daemon".into(),
                AppKind::Cargo,
                "Cargo.toml",
                vec!["daemon".into()],
            )],
        );
        assert_eq!(names(&mixed), ["ui", "daemon"]);
        assert_eq!(containing(&mixed, "ui/index.js"), Some(0));
        assert_eq!(containing(&mixed, "src/main.rs"), Some(1));
    }

    #[test]
    fn app_source_serializes_as_manifest_or_override() {
        assert_eq!(
            serde_json::to_value([AppSource::Manifest, AppSource::Override]).unwrap(),
            serde_json::json!(["manifest", "override"])
        );
    }

    #[test]
    fn python_project_is_an_app_with_script_and_gui_script_modules() {
        let bytes = br#"
[project]
name = "mimir-agent"
[project.scripts]
mimir = "mimir.cli:main"
mimir-admin = "mimir.admin.cli:run [extra]"
also = "mimir.cli:other"
[project.gui-scripts]
mimir-gui = "mimir.gui:start"
"#;
        let found = run(&[manifest("pyproject.toml", Some(bytes))], &[]);
        let app = app(&found, "pyproject.toml");
        assert_eq!(
            (app.name.as_str(), app.kind.unwrap(), app.roots[0].as_str()),
            ("mimir-agent", AppKind::Python, ".")
        );
        assert_eq!(app.entries, ["mimir.admin.cli", "mimir.cli", "mimir.gui"]);
    }

    #[test]
    fn python_project_without_scripts_is_an_app_rooted_at_its_directory() {
        let found = run(
            &[manifest(
                "plugins/cultivate/pyproject.toml",
                Some(b"[project]\nname = \"mimir-cultivate\"\n"),
            )],
            &[],
        );
        let app = app(&found, "plugins/cultivate/pyproject.toml");
        assert_eq!(
            (app.name.as_str(), app.roots[0].as_str()),
            ("mimir-cultivate", "plugins/cultivate")
        );
        assert!(app.entries.is_empty());
    }

    #[test]
    fn pyproject_without_a_project_is_not_an_app() {
        for bytes in [
            &b""[..],
            b"[tool.ruff]\nline-length = 100\n",
            b"[build-system]\nrequires = [\"hatchling\"]\n",
            b"[tool.poetry]\npackage-mode = false\n",
        ] {
            let found = run(&[manifest("pyproject.toml", Some(bytes))], &[]);
            assert_eq!(
                outcome(&found, "pyproject.toml"),
                &Outcome::NotApp,
                "{bytes:?}"
            );
        }
        let found = run(&[manifest("pyproject.toml", None)], &[]);
        assert_eq!(outcome(&found, "pyproject.toml"), &Outcome::NotApp);
    }

    #[test]
    fn malformed_or_over_bound_pyproject_is_unread() {
        for bytes in [
            &b"[project\n"[..],
            PLACEHOLDER,
            &[0xff, 0xfe],
            b"project = 1\n",
            b"tool = 1\n",
            b"[project]\nversion = \"1\"\n",
            b"[project]\nname = 3\n",
            b"[project]\nname = \" \"\n",
            b"[project]\nname = \"a\"\nscripts = 1\n",
            b"[project]\nname = \"a\"\n[project.scripts]\nx = 1\n",
            b"[project]\nname = \"a\"\n[project.gui-scripts]\nx = \":main\"\n",
            // Table targets are Poetry's; a standard project script is a string (#849).
            b"[project]\nname = \"a\"\n[project.scripts]\ncli = { callable = \"fake.module:run\" }\n",
            b"[project]\nname = \"a\"\n[project.scripts]\ncli = {}\n",
            b"[project]\nname = \"a\"\n[project.gui-scripts]\ncli = { callable = \"fake.module:run\" }\n",
            b"[project]\nname = \"a\"\n[project.gui-scripts]\ncli = {}\n",
            b"[project]\nname = \"a\"\n[project.scripts]\ncli = [\"m:f\"]\n",
            b"[project]\nname = \"a\"\n[project.scripts]\ncli = \"\"\n",
            b"[project]\nname = \"a\"\n[project.scripts]\ncli = \"module_only\"\n",
            b"[project]\nname = \"a\"\n[project.scripts]\ncli = \"m:\"\n",
            b"[tool]\npoetry = 1\n",
            b"[tool.poetry]\nname = 4\n",
            b"[tool.poetry]\nname = \"a\"\n[tool.poetry.scripts]\nx = { callable = 1 }\n",
        ] {
            for path in ["pyproject.toml", "nested/pyproject.toml"] {
                let found = run(&[manifest(path, Some(bytes))], &[]);
                assert_eq!(outcome(&found, path), &Outcome::Unread, "{path} {bytes:?}");
            }
        }
    }

    #[test]
    fn poetry_project_is_an_app_with_script_modules() {
        let bytes = br#"
[tool.poetry]
name = "legacy"
[tool.poetry.scripts]
run = "legacy.main:run"
table = { callable = "legacy.table:go", extras = ["x"] }
file = { reference = "bin/tool.sh", type = "file" }
"#;
        let found = run(&[manifest("svc/pyproject.toml", Some(bytes))], &[]);
        let legacy = app(&found, "svc/pyproject.toml");
        assert_eq!(
            (
                legacy.name.as_str(),
                legacy.kind.unwrap(),
                legacy.roots[0].as_str()
            ),
            ("legacy", AppKind::Python, "svc")
        );
        assert_eq!(legacy.entries, ["legacy.main", "legacy.table"]);
        // `[project]` names the app when both are present.
        let both = b"[project]\nname = \"new\"\n[tool.poetry]\nname = \"old\"\n";
        let found = run(&[manifest("pyproject.toml", Some(both))], &[]);
        assert_eq!(app(&found, "pyproject.toml").name, "new");
    }

    #[test]
    fn uv_workspace_root_is_not_an_app_and_every_project_is_judged_alone() {
        let root = br#"
[tool.uv.workspace]
members = ["packages/*"]
exclude = ["packages/skip"]
"#;
        let found = run(
            &[
                manifest("pyproject.toml", Some(root)),
                manifest(
                    "packages/alpha/pyproject.toml",
                    Some(b"[project]\nname = \"alpha\"\n"),
                ),
                manifest(
                    "packages/skip/pyproject.toml",
                    Some(b"[project]\nname = \"skip\"\n"),
                ),
                manifest("packages/tools/pyproject.toml", Some(b"[tool.black]\n")),
                manifest(
                    "elsewhere/pyproject.toml",
                    Some(b"[project]\nname = \"elsewhere\"\n"),
                ),
            ],
            &[],
        );
        assert_eq!(outcome(&found, "pyproject.toml"), &Outcome::NotApp);
        assert_eq!(app(&found, "packages/alpha/pyproject.toml").name, "alpha");
        // Python has no member-only rule: an excluded or non-member project is still an app.
        assert_eq!(app(&found, "packages/skip/pyproject.toml").name, "skip");
        assert_eq!(app(&found, "elsewhere/pyproject.toml").name, "elsewhere");
        assert_eq!(
            outcome(&found, "packages/tools/pyproject.toml"),
            &Outcome::NotApp
        );
        // A root that is both a project and a workspace is an app.
        let both = b"[project]\nname = \"root\"\n[tool.uv.workspace]\nmembers = [\"m\"]\n";
        let found = run(&[manifest("pyproject.toml", Some(both))], &[]);
        assert_eq!(app(&found, "pyproject.toml").name, "root");
    }

    #[test]
    fn hidden_pyproject_is_neither_an_app_nor_unread() {
        let found = run(
            &[
                manifest("pyproject.toml", Some(b"[project]\nname = \"svc\"\n")),
                manifest(
                    "tests/fixtures/app/pyproject.toml",
                    Some(b"[project]\nname = \"fixture\"\n"),
                ),
                manifest("tests/fixtures/bad/pyproject.toml", Some(b"[project\n")),
            ],
            &[],
        );
        let hidden: BTreeSet<String> = [
            "tests/fixtures/app/pyproject.toml",
            "tests/fixtures/bad/pyproject.toml",
        ]
        .map(String::from)
        .into();
        let detected = detect(&found, &hidden);
        let names: Vec<&str> = detected.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["svc"]);
        assert_eq!(detected.unread, 0);
        assert_eq!(detect(&found, &BTreeSet::new()).unread, 1);
    }

    /// mimir: a Python service and a Vite frontend share the checkout root, and a plugin is
    /// its own Python project. The two root apps share root `.` (separating them is #794).
    #[test]
    fn mimir_layout_detects_agent_frontend_and_plugin() {
        let found = run(
            &[
                manifest(
                    "pyproject.toml",
                    Some(b"[project]\nname = \"mimir-agent\"\n[project.scripts]\nmimir = \"mimir.main:main\"\n"),
                ),
                manifest(
                    "package.json",
                    Some(br#"{"name":"mimir-frontend","private":true,"type":"module"}"#),
                ),
                manifest(
                    "plugins/cultivate/pyproject.toml",
                    Some(b"[project]\nname = \"mimir-cultivate\"\n"),
                ),
            ],
            &[
                "mimir/main.py",
                "mimir/agent.py",
                "plugins/cultivate/cultivate/__init__.py",
                "src/main.ts",
                "src/App.ts",
            ],
        );
        let detected = detect(&found, &BTreeSet::new());
        let apps: Vec<(&str, AppKind, &str)> = detected
            .apps
            .iter()
            .map(|a| (a.name.as_str(), a.kind.unwrap(), a.roots[0].as_str()))
            .collect();
        assert_eq!(
            apps,
            [
                ("mimir-frontend", AppKind::Npm, "."),
                ("mimir-cultivate", AppKind::Python, "plugins/cultivate"),
                ("mimir-agent", AppKind::Python, "."),
            ]
        );
        assert_eq!(detected.unread, 0);
        assert_eq!(
            containing(&detected.apps, "plugins/cultivate/cultivate/__init__.py"),
            Some(1)
        );
    }

    #[test]
    fn python_kind_serializes_as_python() {
        assert_eq!(
            serde_json::to_value(AppKind::Python).unwrap(),
            serde_json::json!("python")
        );
    }

    #[test]
    fn parsing_is_bounded_and_checked_per_manifest() {
        let manifests = [
            manifest("package.json", Some(b"{}")),
            manifest("a/package.json", Some(b"{}")),
            manifest("a/pyproject.toml", Some(b"")),
        ];
        let documents = BTreeSet::new();
        let error =
            candidates(&manifests, &documents, &mut Work::new(3), &mut || Ok(())).unwrap_err();
        assert_eq!(error.to_string(), "bounded_work: app manifest parsing");
        let python = [manifest("pyproject.toml", Some(b"[project]"))];
        let error = candidates(&python, &documents, &mut Work::new(8), &mut || Ok(())).unwrap_err();
        assert_eq!(error.to_string(), "bounded_work: app manifest parsing");
        let mut checks = 0;
        candidates(&manifests, &documents, &mut Work::new(100), &mut || {
            checks += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(checks, 3);
        let error = candidates(&manifests, &documents, &mut Work::new(100), &mut || {
            anyhow::bail!("expired")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "expired");
    }

    fn pom(artifact: &str, packaging: Option<&str>, build: &str, modules: &[&str]) -> Vec<u8> {
        let packaging = packaging.map_or(String::new(), |p| format!("<packaging>{p}</packaging>"));
        let modules = if modules.is_empty() {
            String::new()
        } else {
            format!(
                "<modules>{}</modules>",
                modules
                    .iter()
                    .map(|m| format!("<module>{m}</module>"))
                    .collect::<String>()
            )
        };
        format!(
            "<?xml version=\"1.0\"?>\n<project xmlns=\"http://maven.apache.org/POM/4.0.0\"><modelVersion>4.0.0</modelVersion><groupId>com.acme</groupId><artifactId>{artifact}</artifactId>{packaging}{modules}{build}</project>\n"
        )
        .into_bytes()
    }

    fn plugin(artifact: &str, configuration: &str) -> String {
        format!(
            "<build><plugins><plugin><groupId>g</groupId><artifactId>{artifact}</artifactId><configuration>{configuration}</configuration></plugin></plugins></build>"
        )
    }

    fn kind_and_entries(found: &Candidates, path: &str) -> (String, Option<AppKind>, Vec<String>) {
        let app = app(found, path);
        (app.name, app.kind, app.entries)
    }

    #[test]
    fn maven_war_and_jars_with_a_declared_main_class_are_apps() {
        let jar_manifest = plugin(
            "maven-jar-plugin",
            "<archive><manifest><mainClass>com.acme.Cli</mainClass></manifest></archive>",
        );
        let exec = "<build><plugins><plugin><artifactId>exec-maven-plugin</artifactId><executions><execution><configuration><mainClass>${main}</mainClass></configuration></execution></executions></plugin></plugins></build><properties><main>com.acme.Exec</main></properties>";
        let boot = plugin("spring-boot-maven-plugin", "");
        let cases: [(&str, Vec<u8>, Vec<&str>); 5] = [
            ("web", pom("web", Some("war"), "", &[]), vec![]),
            (
                "cli",
                pom("cli", None, &jar_manifest, &[]),
                vec!["com.acme.Cli"],
            ),
            (
                "cli2",
                pom("cli2", Some("jar"), &jar_manifest, &[]),
                vec!["com.acme.Cli"],
            ),
            ("exec", pom("exec", None, exec, &[]), vec!["com.acme.Exec"]),
            ("boot", pom("boot", None, &boot, &[]), vec![]),
        ];
        for (name, bytes, entries) in cases {
            let found = run(&[manifest("pom.xml", Some(&bytes))], &[]);
            assert_eq!(
                kind_and_entries(&found, "pom.xml"),
                (
                    name.to_owned(),
                    Some(AppKind::Maven),
                    entries.into_iter().map(str::to_owned).collect()
                ),
                "{name}"
            );
            assert_eq!(app(&found, "pom.xml").roots, ["."]);
        }
    }

    #[test]
    fn maven_libraries_aggregators_and_other_packagings_are_not_apps() {
        let managed = "<build><pluginManagement><plugins><plugin><artifactId>spring-boot-maven-plugin</artifactId></plugin></plugins></pluginManagement></build>";
        let empty_main = plugin(
            "maven-jar-plugin",
            "<archive><manifest><mainClass> </mainClass></manifest></archive>",
        );
        for bytes in [
            pom("lib", None, "", &[]),
            pom(
                "lib",
                Some("jar"),
                &plugin("maven-compiler-plugin", ""),
                &[],
            ),
            pom("parent", Some("pom"), "", &[]),
            pom(
                "ear",
                Some("ear"),
                &plugin("spring-boot-maven-plugin", ""),
                &[],
            ),
            pom("managed", None, managed, &[]),
            pom("blank", None, &empty_main, &[]),
        ] {
            let found = run(&[manifest("pom.xml", Some(&bytes))], &[]);
            assert_eq!(outcome(&found, "pom.xml"), &Outcome::NotApp);
        }
    }

    #[test]
    fn malformed_or_over_bound_maven_and_gradle_manifests_are_unread() {
        for bytes in [
            &b"<project><artifactId>x</artifactId>"[..],
            b"<project></project>",
            b"<project><artifactId> </artifactId></project>",
            b"<notaproject><artifactId>x</artifactId></notaproject>",
            b"<!DOCTYPE project><project><artifactId>x</artifactId></project>",
            b"\xff<project/>",
            PLACEHOLDER,
        ] {
            let found = run(&[manifest("pom.xml", Some(bytes))], &[]);
            assert_eq!(outcome(&found, "pom.xml"), &Outcome::Unread, "{bytes:?}");
        }
        for bytes in [
            &b"plugins { id 'application'"[..],
            b"mainClass = 'x",
            b"\xff",
            PLACEHOLDER,
        ] {
            for path in ["build.gradle", "build.gradle.kts", "settings.gradle"] {
                let found = run(&[manifest(path, Some(bytes))], &[]);
                assert_eq!(outcome(&found, path), &Outcome::Unread, "{path} {bytes:?}");
            }
        }
    }

    #[test]
    fn maven_multi_module_gives_one_app_per_runnable_listed_module() {
        let root = pom(
            "shop",
            Some("pom"),
            "",
            &["api", "./lib/", "web/pom.xml", "../outside"],
        );
        let api = pom("shop-api", Some("war"), "", &[]);
        let lib = pom("shop-lib", None, "", &["nested"]);
        let nested = pom("shop-nested", Some("war"), "", &[]);
        let web = pom(
            "shop-web",
            None,
            &plugin("spring-boot-maven-plugin", ""),
            &[],
        );
        let stray = pom("stray", Some("war"), "", &[]);
        let found = run(
            &[
                manifest("pom.xml", Some(&root)),
                manifest("api/pom.xml", Some(&api)),
                manifest("lib/pom.xml", Some(&lib)),
                manifest("lib/nested/pom.xml", Some(&nested)),
                manifest("web/pom.xml", Some(&web)),
                manifest("stray/pom.xml", Some(&stray)),
            ],
            &[],
        );
        assert_eq!(outcome(&found, "pom.xml"), &Outcome::NotApp);
        assert_eq!(app(&found, "api/pom.xml").name, "shop-api");
        assert_eq!(app(&found, "api/pom.xml").roots, ["api"]);
        assert_eq!(outcome(&found, "lib/pom.xml"), &Outcome::NotApp);
        assert_eq!(app(&found, "lib/nested/pom.xml").name, "shop-nested");
        assert_eq!(app(&found, "web/pom.xml").name, "shop-web");
        // Below a POM that does not list it, a POM is not judged.
        assert_eq!(outcome(&found, "stray/pom.xml"), &Outcome::NotApp);
    }

    #[test]
    fn a_nested_pom_without_a_parent_pom_is_judged_alone() {
        let service = pom("svc", Some("war"), "", &[]);
        let unread = b"<project>";
        let found = run(
            &[
                manifest("pom.xml", None),
                manifest("backend/svc/pom.xml", Some(&service)),
                manifest("other/pom.xml", Some(unread)),
                manifest("other/child/pom.xml", Some(&service)),
            ],
            &[],
        );
        assert_eq!(outcome(&found, "pom.xml"), &Outcome::NotApp);
        assert_eq!(app(&found, "backend/svc/pom.xml").roots, ["backend/svc"]);
        // An unread parent lists nothing, so its children are not judged.
        assert_eq!(outcome(&found, "other/pom.xml"), &Outcome::Unread);
        assert_eq!(outcome(&found, "other/child/pom.xml"), &Outcome::NotApp);
    }

    #[test]
    fn gradle_application_plugin_or_main_class_makes_an_app() {
        let runnable: [(&str, &str, Vec<&str>); 8] = [
            (
                "build.gradle",
                "plugins {\n    id 'java'\n    id 'application'\n}\n",
                vec![],
            ),
            (
                "build.gradle.kts",
                "plugins {\n    application\n}\n",
                vec![],
            ),
            (
                "build.gradle.kts",
                "plugins { id(\"application\") }\napplication { mainClass.set(\"com.acme.App\") }\n",
                vec!["com.acme.App"],
            ),
            (
                "build.gradle",
                "apply plugin: 'application'\nmainClassName = 'com.acme.Old'\n",
                vec!["com.acme.Old"],
            ),
            (
                "build.gradle.kts",
                "apply(plugin = \"application\")\n",
                vec![],
            ),
            (
                "build.gradle",
                "application {\n  mainClass = \"com.acme.Main\" // run\n}\n",
                vec!["com.acme.Main"],
            ),
            (
                "build.gradle",
                "springBoot { mainClass = \"com.acme.${name}\" }\n",
                vec![],
            ),
            (
                "build.gradle",
                "/* plugins { id 'application' } */ jar { }\nmainClassName = 'a.B'\n",
                vec!["a.B"],
            ),
        ];
        for (path, script, entries) in runnable {
            let found = run(&[manifest(path, Some(script.as_bytes()))], &[]);
            assert_eq!(
                kind_and_entries(&found, path),
                (
                    ".".to_owned(),
                    Some(AppKind::Gradle),
                    entries.into_iter().map(str::to_owned).collect()
                ),
                "{script}"
            );
        }
        for script in [
            "plugins { id 'java-library' }\n",
            "plugins { id 'java' }\n// id 'application'\n",
            "def s = \"id 'application'\"\n",
            "dependencies { implementation project(':application') }\n",
            "tasks.jar { manifest { attributes 'Main-Class': 'a.B' } }\n",
            "println(project.mainClassName)\n",
        ] {
            let found = run(&[manifest("build.gradle", Some(script.as_bytes()))], &[]);
            assert_eq!(
                outcome(&found, "build.gradle"),
                &Outcome::NotApp,
                "{script}"
            );
        }
    }

    #[test]
    fn gradle_multi_module_gives_one_app_per_runnable_included_project() {
        let settings = b"rootProject.name = 'shop'\ninclude 'app', ':lib:core'\ninclude(':tool')\n";
        let app_build = b"plugins { id 'application' }\napplication { mainClass = 'shop.Main' }\n";
        let found = run(
            &[
                manifest("settings.gradle", Some(settings)),
                manifest("build.gradle", Some(b"plugins { id 'application' }\n")),
                manifest("app/build.gradle", Some(app_build)),
                manifest(
                    "lib/core/build.gradle.kts",
                    Some(b"plugins { `java-library` }\n"),
                ),
                manifest("tool/build.gradle.kts", Some(b"plugins { application }\n")),
                manifest(
                    "stray/build.gradle",
                    Some(b"plugins { id 'application' }\n"),
                ),
            ],
            &[],
        );
        assert_eq!(outcome(&found, "settings.gradle"), &Outcome::NotApp);
        assert_eq!(app(&found, "build.gradle").name, "shop");
        let app_module = app(&found, "app/build.gradle");
        assert_eq!(
            (
                app_module.name.as_str(),
                app_module.roots,
                app_module.entries
            ),
            ("app", vec!["app".to_owned()], vec!["shop.Main".to_owned()])
        );
        assert_eq!(
            outcome(&found, "lib/core/build.gradle.kts"),
            &Outcome::NotApp
        );
        assert_eq!(app(&found, "tool/build.gradle.kts").name, "tool");
        // Not included by the root settings: not judged.
        assert_eq!(outcome(&found, "stray/build.gradle"), &Outcome::NotApp);
    }

    #[test]
    fn gradle_without_root_settings_judges_topmost_build_files_alone() {
        let found = run(
            &[
                manifest("settings.gradle", None),
                manifest(
                    "android/build.gradle",
                    Some(b"apply plugin: 'application'\n"),
                ),
                manifest(
                    "android/app/build.gradle",
                    Some(b"apply plugin: 'application'\n"),
                ),
            ],
            &[],
        );
        assert_eq!(app(&found, "android/build.gradle").name, "android");
        assert_eq!(
            outcome(&found, "android/app/build.gradle"),
            &Outcome::NotApp
        );
        let unread = run(
            &[
                manifest("settings.gradle", Some(b"include 'app")),
                manifest("app/build.gradle", Some(b"apply plugin: 'application'\n")),
            ],
            &[],
        );
        assert_eq!(outcome(&unread, "settings.gradle"), &Outcome::Unread);
        assert_eq!(outcome(&unread, "app/build.gradle"), &Outcome::NotApp);
    }

    #[test]
    fn hidden_maven_and_gradle_manifests_are_neither_apps_nor_unread() {
        let found = run(
            &[
                manifest(
                    "tests/fixtures/pom.xml",
                    Some(&pom("fx", Some("war"), "", &[])),
                ),
                manifest("tests/bad/pom.xml", Some(b"<project>")),
                manifest("svc/build.gradle", Some(b"apply plugin: 'application'\n")),
            ],
            &[],
        );
        let hidden: BTreeSet<String> = ["tests/fixtures/pom.xml", "tests/bad/pom.xml"]
            .map(str::to_owned)
            .into();
        let detected = detect(&found, &hidden);
        assert_eq!(names(&detected.apps), ["svc"]);
        assert_eq!(detected.unread, 0);
        assert_eq!(detect(&found, &BTreeSet::new()).unread, 1);
    }

    #[test]
    fn maven_and_gradle_kinds_serialize_lowercase() {
        assert_eq!(
            serde_json::to_value(AppKind::Maven).unwrap(),
            serde_json::json!("maven")
        );
        assert_eq!(
            serde_json::to_value(AppKind::Gradle).unwrap(),
            serde_json::json!("gradle")
        );
    }

    #[test]
    fn java_parsing_is_bounded_and_checked_per_manifest() {
        let documents = BTreeSet::new();
        let poms = [manifest("pom.xml", Some(&pom("a", Some("war"), "", &[])))];
        let size = poms[0].bytes.as_ref().unwrap().len();
        let error =
            candidates(&poms, &documents, &mut Work::new(size - 1), &mut || Ok(())).unwrap_err();
        assert_eq!(error.to_string(), "bounded_work: app manifest parsing");
        candidates(&poms, &documents, &mut Work::new(size), &mut || Ok(())).unwrap();
        let gradle = [
            manifest("settings.gradle", Some(b"include 'a'\n")),
            manifest("a/build.gradle", Some(b"apply plugin: 'application'\n")),
        ];
        let error =
            candidates(&gradle, &documents, &mut Work::new(20), &mut || Ok(())).unwrap_err();
        assert_eq!(error.to_string(), "bounded_work: app manifest parsing");
        let mut checks = 0;
        candidates(&gradle, &documents, &mut Work::new(1000), &mut || {
            checks += 1;
            Ok(())
        })
        .unwrap();
        // One per Java manifest judged, plus one per listed manifest.
        assert_eq!(checks, 4);
    }
}
