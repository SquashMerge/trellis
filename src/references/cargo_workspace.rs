//! Cargo workspace member crates for Rust structural resolution (#772, owner-ratified
//! 2026-11-10). Read from the captured root `Cargo.toml` and the retained member manifests
//! only, bounded, executing nothing: no `cargo metadata`, build script or network fetch.
//!
//! - The members are the directories of the retained nested `Cargo.toml` files that the root
//!   `[workspace] members` literal paths and simple globs match and `exclude` does not, plus
//!   the root package when the root manifest has `[package]`.
//! - A member's library crate root is `[lib] path`, else `src/lib.rs`, relative to the member
//!   (none when `autolib = false` without `[lib]`); its crate name is `[lib] name`, else the
//!   package name with `-` replaced by `_`. Its binary crate root is `src/main.rs` when Cargo
//!   builds it as a binary target, which [`main_root`] decides by Cargo's target
//!   auto-discovery (#1190).
//! - A member binds another member's library by a dependency in `[dependencies]` or
//!   `[dev-dependencies]` (also under `[target.*]`) whose `path`, or the root
//!   `[workspace.dependencies]` entry it inherits with `workspace = true`, is that member's
//!   directory. A renamed dependency (`package = "..."`) binds its key; otherwise it binds
//!   the target's crate name. A binary crate binds its own package's library by crate name.
//! - Two members with one package name, two library roots claiming one file, or one name bound
//!   to two crates prove nothing, so those paths stay unknown.
use super::external::ManifestInput;
use super::work::Work;
use anyhow::{Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

/// The most workspace members one source set proves.
const MAX_MEMBERS: usize = 4096;
const ROOT: &str = "Cargo.toml";

/// One crate root of a proved member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CrateRoot {
    /// The member's index in its source set.
    pub member: usize,
    /// Whether this is the library crate (else a binary crate).
    pub library: bool,
}

#[derive(Debug)]
struct Member {
    dir: String,
    package: String,
    crate_name: String,
    library: Option<String>,
    binary: Option<String>,
    /// Every crate root of the package is proved to be of edition 2018 or later.
    uniform_paths: bool,
    /// Dependency bindings: extern crate name to the member it names, or `None` when the
    /// name is bound to more than one crate or to an unproved one.
    bindings: BTreeMap<String, Option<usize>>,
}

#[derive(Default, Debug)]
struct SourceSet {
    members: Vec<Member>,
    roots: BTreeMap<String, CrateRoot>,
    /// The files more than one retained package manifest, member or not, names as a target
    /// root: each of those packages can compile the file under its own edition.
    shared: BTreeSet<String>,
    /// Each retained package manifest's directory with a directory Cargo discovers targets
    /// in (`src/bin/`, `examples/`, `tests/`, `benches/`): a file under one can be a target
    /// of that package without being named.
    discovered: Vec<(String, String)>,
    /// A retained package manifest names a target root by an absolute path. Nothing relates
    /// that path to a file of the checkout, so no crate root of the checkout is proved.
    absolute: bool,
}

/// The proved Cargo workspace members of each source set.
#[derive(Default, Debug)]
pub(super) struct Workspace {
    sets: BTreeMap<String, SourceSet>,
}

fn parse(manifest: &ManifestInput, work: &mut Work) -> Result<Option<toml::Table>> {
    let Some(bytes) = &manifest.bytes else {
        return Ok(None);
    };
    ensure!(
        work.charge(bytes.len()),
        "bounded_work: reference manifest parsing"
    );
    Ok(std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(text).ok()))
}

/// `path` joined to `dir` and normalized, or `None` when it leaves the checkout or is
/// absolute.
fn join(dir: &str, path: &str) -> Option<String> {
    if path.starts_with('/') || path.contains('\\') {
        return None;
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in path.split('/') {
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

fn strings(value: Option<&toml::Value>) -> Option<Vec<&str>> {
    match value {
        None => Some(Vec::new()),
        Some(toml::Value::Array(items)) => items.iter().map(toml::Value::as_str).collect(),
        Some(_) => None,
    }
}

/// The member directories the root `[workspace]` names among `dirs`, or `None` when the
/// root's `[workspace]` cannot be read.
fn member_dirs<'a>(root: &toml::Table, dirs: &[&'a str]) -> Option<Vec<&'a str>> {
    let workspace = match root.get("workspace") {
        None => return Some(Vec::new()),
        Some(toml::Value::Table(workspace)) => workspace,
        Some(_) => return None,
    };
    let patterns = strings(workspace.get("members"))?;
    let exclude = strings(workspace.get("exclude"))?;
    let mut set = globset::GlobSetBuilder::new();
    for pattern in patterns {
        let pattern = join("", pattern)?;
        set.add(
            globset::GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
                .ok()?,
        );
    }
    let set = set.build().ok()?;
    let exclude: Vec<String> = exclude
        .into_iter()
        .map(|path| join("", path))
        .collect::<Option<_>>()?;
    Some(
        dirs.iter()
            .copied()
            .filter(|dir| {
                set.is_match(dir)
                    && !exclude
                        .iter()
                        .any(|out| dir == out || dir.starts_with(&format!("{out}/")))
            })
            .collect(),
    )
}

/// The member parsed from `table`, or `None` when it is not a proved package.
fn member(dir: &str, table: &toml::Table, root: &toml::Table) -> Option<Member> {
    let package = table.get("package")?.as_table()?;
    let name = package.get("name")?.as_str()?;
    if name.is_empty() {
        return None;
    }
    let mut crate_name = name.replace('-', "_");
    let library = match table.get("lib") {
        Some(toml::Value::Table(lib)) => {
            if let Some(lib_name) = lib.get("name") {
                crate_name = lib_name.as_str()?.to_owned();
            }
            let path = match lib.get("path") {
                None => "src/lib.rs",
                Some(path) => path.as_str()?,
            };
            Some(join(dir, path)?)
        }
        Some(_) => return None,
        None => (package.get("autolib").and_then(toml::Value::as_bool) != Some(false))
            .then(|| join(dir, "src/lib.rs"))
            .flatten(),
    };
    if !crate_name
        .chars()
        .next()
        .is_some_and(|c| c == '_' || c.is_alphabetic())
        || !crate_name.chars().all(|c| c == '_' || c.is_alphanumeric())
    {
        return None;
    }
    let binary = main_root(dir, table, root)?;
    Some(Member {
        dir: dir.to_owned(),
        package: name.to_owned(),
        crate_name,
        library,
        binary,
        uniform_paths: false,
        bindings: BTreeMap::new(),
    })
}

/// The binary crate root `src/main.rs` of the package manifest `table` in `dir`, when Cargo
/// builds it as a binary target, by Cargo's target auto-discovery (#1190); `None` when `table`
/// is no package or its `bin` is not an array. A `[[bin]]` names the file by its `path`, or
/// with no `path` by the package's name. Otherwise Cargo discovers it beside the explicit
/// `[[bin]]` entries unless one of them takes the package's name for another file, or
/// discovery is off: `autobins = false`, or, without an `autobins` key, a package of edition
/// 2015 (or one not proved later) that lists `[[bin]]` at all. Cargo's edition 2015 legacy
/// fallback paths for a `[[bin]]` with no `path` are not modelled, and the shortcut that a
/// `[[bin]]` with no `path` and the package's name is `src/main.rs` is inherited unchanged.
pub(super) fn main_root(
    dir: &str,
    table: &toml::Table,
    root: &toml::Table,
) -> Option<Option<String>> {
    let package = table.get("package")?.as_table()?;
    let name = package.get("name")?.as_str()?;
    let main = join(dir, "src/main.rs")?;
    let bins = match table.get("bin") {
        None => None,
        Some(toml::Value::Array(bins)) => Some(bins),
        Some(_) => return None,
    };
    let mut named = false;
    for bin in bins.into_iter().flatten().filter_map(toml::Value::as_table) {
        let bin_name = bin.get("name").and_then(toml::Value::as_str);
        let names_main = match bin.get("path") {
            Some(path) => path.as_str().and_then(|p| join(dir, p)).as_ref() == Some(&main),
            None => bin_name == Some(name),
        };
        if names_main {
            return Some(Some(main));
        }
        named |= bin_name == Some(name);
    }
    let discovered = match package.get("autobins").and_then(toml::Value::as_bool) {
        Some(autobins) => autobins,
        None => bins.is_none() || edition(package, root).is_some_and(|e| e != "2015"),
    };
    Some((discovered && !named).then_some(main))
}

/// The package's edition: a literal string, or the one a sole `edition.workspace = true`
/// inherits from the `[workspace.package]` of the checkout's root manifest. A missing key is
/// `"2015"`; anything else is `None`.
fn edition<'a>(package: &'a toml::Table, root: &'a toml::Table) -> Option<&'a str> {
    match package.get("edition") {
        None => Some("2015"),
        Some(toml::Value::Table(inherit)) => {
            if inherit.len() != 1
                || inherit.get("workspace").and_then(toml::Value::as_bool) != Some(true)
            {
                return None;
            }
            root.get("workspace")?
                .as_table()?
                .get("package")?
                .as_table()?
                .get("edition")?
                .as_str()
        }
        Some(edition) => edition.as_str(),
    }
}

/// Whether every target of the package `table` is proved to be of edition 2018 or later,
/// where a `use` path starts in its own module and not at the crate root. Only a literal
/// `[package] edition`, or one inherited from the `[workspace.package]` of the checkout's
/// root manifest, proves it. A missing key is edition 2015, and an edition that is not a
/// string or that this list does not name proves nothing. A target can set an edition of
/// its own and compile any file under it (`[[example]] edition = "2015"` with
/// `path = "src/lib.rs"`), so an `edition` key in any other top-level table, or in any entry
/// of a top-level array, proves nothing for the whole package, whatever its value, and so
/// does a top-level array with an entry that is not a table (a `cargo-features` list that
/// is not empty). A package that names
/// another workspace root proves nothing either.
fn uniform_paths(table: &toml::Table, root: &toml::Table) -> bool {
    const UNIFORM: [&str; 3] = ["2018", "2021", "2024"];
    let own_edition = |section: &toml::Value| match section {
        toml::Value::Table(section) => section.contains_key("edition"),
        toml::Value::Array(entries) => entries.iter().any(|entry| {
            entry
                .as_table()
                .is_none_or(|entry| entry.contains_key("edition"))
        }),
        _ => false,
    };
    let Some(package) = table.get("package").and_then(toml::Value::as_table) else {
        return false;
    };
    if package.contains_key("workspace")
        || table.iter().any(|(key, section)| {
            !matches!(key.as_str(), "package" | "workspace") && own_edition(section)
        })
    {
        return false;
    }
    edition(package, root).is_some_and(|edition| UNIFORM.contains(&edition))
}

/// Whether the package manifest `table` names a target root, a target `path` or a string
/// `package.build`, by an absolute path: a leading `/`, a drive letter or a backslash.
fn absolute_target(table: &toml::Table) -> bool {
    let absolute = |path: Option<&toml::Value>| {
        path.and_then(toml::Value::as_str).is_some_and(|path| {
            path.starts_with('/') || path.contains('\\') || path.chars().nth(1) == Some(':')
        })
    };
    let named = |section: &toml::Value| absolute(section.as_table().and_then(|s| s.get("path")));
    let Some(package) = table.get("package").and_then(toml::Value::as_table) else {
        return false;
    };
    absolute(package.get("build"))
        || table.values().any(|section| match section {
            toml::Value::Table(_) => named(section),
            toml::Value::Array(entries) => entries.iter().any(named),
            _ => false,
        })
}

/// The directories the package manifest `table` in `dir` discovers targets in, each with
/// `dir` and a trailing `/`.
fn target_dirs(dir: &str, table: &toml::Table) -> Vec<(String, String)> {
    if !table.contains_key("package") {
        return Vec::new();
    }
    ["src/bin", "examples", "tests", "benches"]
        .into_iter()
        .filter_map(|targets| Some((dir.to_owned(), format!("{}/", join(dir, targets)?))))
        .collect()
}

/// The files the package manifest `table` in `dir` names as a target root: the conventional
/// library, binary and build script roots, a string `package.build`, and the `path` of
/// every top-level table and of every entry of
/// a top-level array (`[lib]`, `[[bin]]`, `[[example]]`, `[[test]]`, `[[bench]]`). A path
/// that leaves the checkout names no file of it. Nothing says whether Cargo would build the
/// target; a file is only ever proved less by being named.
fn target_roots(dir: &str, table: &toml::Table) -> BTreeSet<String> {
    let mut roots = BTreeSet::new();
    if !table.contains_key("package") {
        return roots;
    }
    // A string `package.build` is the custom-build target's root, whatever file it names;
    // `build = true`, `false` or no key claim nothing beyond the conventional `build.rs`.
    let build = table
        .get("package")
        .and_then(toml::Value::as_table)
        .and_then(|package| package.get("build"))
        .and_then(toml::Value::as_str);
    roots.extend(
        ["src/lib.rs", "src/main.rs", "build.rs"]
            .into_iter()
            .chain(build)
            .filter_map(|path| join(dir, path)),
    );
    let named = |section: &toml::Value| {
        section
            .as_table()
            .and_then(|section| section.get("path"))
            .and_then(toml::Value::as_str)
            .and_then(|path| join(dir, path))
    };
    for section in table.values() {
        match section {
            toml::Value::Table(_) => roots.extend(named(section)),
            toml::Value::Array(entries) => roots.extend(entries.iter().filter_map(named)),
            _ => {}
        }
    }
    roots
}

/// Each dependency table of a manifest that crate sources can name: `[dependencies]` and
/// `[dev-dependencies]`, also under `[target.<cfg>]`. Build dependencies serve `build.rs`.
fn dependency_tables(table: &toml::Table) -> Vec<&toml::Table> {
    let targets = table
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|targets| targets.values().filter_map(toml::Value::as_table));
    std::iter::once(table)
        .chain(targets)
        .flat_map(|parent| {
            ["dependencies", "dev-dependencies"]
                .into_iter()
                .filter_map(|section| parent.get(section).and_then(toml::Value::as_table))
        })
        .collect()
}

impl Workspace {
    pub(super) fn new(
        manifests: &[ManifestInput],
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut by_set: BTreeMap<&str, BTreeMap<&str, &ManifestInput>> = BTreeMap::new();
        for manifest in manifests {
            super::owner_step(work, check)?;
            let dir = match manifest.path.strip_suffix(ROOT) {
                Some("") => "",
                Some(dir) => match dir.strip_suffix('/') {
                    Some(dir) => dir,
                    None => continue,
                },
                None => continue,
            };
            if manifest.bytes.is_some() {
                by_set
                    .entry(&manifest.source_set_id)
                    .or_default()
                    .insert(dir, manifest);
            }
        }
        let mut result = Self::default();
        for (source_set, manifests) in by_set {
            check()?;
            let Some(root_manifest) = manifests.get("") else {
                continue;
            };
            let Some(root) = parse(root_manifest, work)? else {
                continue;
            };
            let nested: Vec<&str> = manifests
                .keys()
                .copied()
                .filter(|d| !d.is_empty())
                .collect();
            let Some(dirs) = member_dirs(&root, &nested) else {
                continue;
            };
            // Every retained package manifest, member or not, names target roots: a file two
            // of them name can be compiled under two editions.
            let mut claims: BTreeMap<String, usize> = BTreeMap::new();
            for path in target_roots("", &root) {
                *claims.entry(path).or_default() += 1;
            }
            let mut discovered = target_dirs("", &root);
            let mut absolute = absolute_target(&root);
            let mut tables = Vec::new();
            if let Some(mut member) = member("", &root, &root) {
                member.uniform_paths = uniform_paths(&root, &root);
                tables.push((member, root.clone()));
            }
            for &dir in &nested {
                super::owner_step(work, check)?;
                let Some(table) = parse(manifests[dir], work)? else {
                    continue;
                };
                for path in target_roots(dir, &table) {
                    super::owner_step(work, check)?;
                    *claims.entry(path).or_default() += 1;
                }
                discovered.extend(target_dirs(dir, &table));
                absolute |= absolute_target(&table);
                if dirs.contains(&dir)
                    && let Some(mut member) = member(dir, &table, &root)
                {
                    member.uniform_paths = uniform_paths(&table, &root);
                    tables.push((member, table));
                }
            }
            ensure!(
                tables.len() <= MAX_MEMBERS,
                "bounded_work: reference Cargo workspace members"
            );
            let mut by_package: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            let mut by_dir: BTreeMap<String, usize> = BTreeMap::new();
            for (index, (member, _)) in tables.iter().enumerate() {
                by_package
                    .entry(member.package.clone())
                    .or_default()
                    .push(index);
                by_dir.insert(member.dir.clone(), index);
            }
            let inherited = root
                .get("workspace")
                .and_then(toml::Value::as_table)
                .and_then(|w| w.get("dependencies"))
                .and_then(toml::Value::as_table);
            // The member a dependency declaration names, when it is a unique workspace member.
            let target =
                |dir: &str, key: &str, declaration: &toml::Table| -> Option<(usize, bool)> {
                    let mut package = declaration.get("package");
                    let path = if declaration.get("workspace").and_then(toml::Value::as_bool)
                        == Some(true)
                    {
                        let shared = inherited?.get(key)?.as_table()?;
                        package = package.or_else(|| shared.get("package"));
                        join("", shared.get("path")?.as_str()?)?
                    } else {
                        join(dir, declaration.get("path")?.as_str()?)?
                    };
                    let renamed = package.is_some();
                    let package = match package {
                        Some(package) => package.as_str()?,
                        None => key,
                    };
                    let index = *by_dir.get(&path)?;
                    let owners = by_package.get(package)?;
                    (owners.as_slice() == [index]).then_some((index, renamed))
                };
            let mut bindings = Vec::with_capacity(tables.len());
            for (member, table) in &tables {
                let mut bound: BTreeMap<String, BTreeSet<Option<usize>>> = BTreeMap::new();
                for deps in dependency_tables(table) {
                    for (key, declaration) in deps {
                        super::owner_step(work, check)?;
                        let Some(declaration) = declaration.as_table() else {
                            continue;
                        };
                        if declaration.get("path").is_none()
                            && declaration.get("workspace").is_none()
                        {
                            continue;
                        }
                        match target(&member.dir, key, declaration) {
                            Some((index, renamed)) => {
                                let name = if renamed {
                                    key.replace('-', "_")
                                } else {
                                    tables[index].0.crate_name.clone()
                                };
                                bound.entry(name).or_default().insert(Some(index));
                            }
                            None => {
                                bound.entry(key.replace('-', "_")).or_default().insert(None);
                            }
                        }
                    }
                }
                bindings.push(
                    bound
                        .into_iter()
                        .map(|(name, targets)| {
                            let target = match targets.into_iter().collect::<Vec<_>>().as_slice() {
                                [Some(index)] => Some(*index),
                                _ => None,
                            };
                            (name, target)
                        })
                        .collect::<BTreeMap<_, _>>(),
                );
            }
            let mut set = SourceSet {
                shared: claims
                    .into_iter()
                    .filter_map(|(path, packages)| (packages > 1).then_some(path))
                    .collect(),
                discovered,
                absolute,
                ..SourceSet::default()
            };
            let mut claimed: BTreeMap<String, Vec<CrateRoot>> = BTreeMap::new();
            for (index, ((mut member, _), bindings)) in tables.into_iter().zip(bindings).enumerate()
            {
                member.bindings = bindings;
                if let Some(library) = &member.library {
                    claimed.entry(library.clone()).or_default().push(CrateRoot {
                        member: index,
                        library: true,
                    });
                }
                if let Some(binary) = &member.binary {
                    claimed.entry(binary.clone()).or_default().push(CrateRoot {
                        member: index,
                        library: false,
                    });
                }
                set.members.push(member);
            }
            for (path, mut roots) in claimed {
                if roots.len() == 1 {
                    set.roots.insert(path, roots.pop().expect("one root"));
                }
            }
            result.sets.insert(source_set.to_owned(), set);
        }
        Ok(result)
    }

    /// The crate root at `path`, when a proved member owns it alone.
    pub(super) fn crate_root(&self, source_set: &str, path: &str) -> Option<&CrateRoot> {
        self.sets.get(source_set)?.roots.get(path)
    }

    /// Whether the crate rooted at `path` is proved to be of edition 2018 or later: its
    /// package proves it, no other retained package manifest names the file as a target
    /// root, the file is in no directory another one discovers targets in, and no retained
    /// package manifest names a target root by an absolute path.
    pub(super) fn uniform_paths(&self, source_set: &str, path: &str) -> bool {
        self.sets.get(source_set).is_some_and(|set| {
            !set.absolute
                && !set.shared.contains(path)
                && set.roots.get(path).is_some_and(|root| {
                    let member = &set.members[root.member];
                    member.uniform_paths
                        && !set
                            .discovered
                            .iter()
                            .any(|(dir, targets)| *dir != member.dir && path.starts_with(targets))
                })
        })
    }

    /// The extern crate names the crate `from` binds to member libraries: each dependency on
    /// another member, and a binary's own library, with the library root file, or `None`
    /// when the name is bound but unproved (to two crates, or to a crate with no proved
    /// library root).
    pub(super) fn externs(
        &self,
        source_set: &str,
        from: &CrateRoot,
    ) -> Vec<(String, Option<&str>)> {
        let Some(set) = self.sets.get(source_set) else {
            return Vec::new();
        };
        let library = |target: usize| {
            let library = set.members[target].library.as_deref()?;
            // Another member claiming the same root file leaves it unproved.
            set.roots
                .get(library)
                .is_some_and(|root| root.member == target && root.library)
                .then_some(library)
        };
        let member = &set.members[from.member];
        let mut externs: Vec<(String, Option<&str>)> = member
            .bindings
            .iter()
            .map(|(name, target)| (name.clone(), target.and_then(library)))
            .collect();
        if !from.library && !member.bindings.contains_key(&member.crate_name) {
            externs.push((member.crate_name.clone(), library(from.member)));
        }
        externs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(path: &str, text: &str) -> ManifestInput {
        ManifestInput {
            source_set_id: "s".into(),
            native_revision_id: "r".into(),
            dependency_hash: "d".into(),
            path: path.into(),
            content_hash: Some("h".into()),
            bytes: Some(text.as_bytes().to_vec()),
        }
    }

    fn workspace(files: &[(&str, &str)]) -> Workspace {
        let manifests: Vec<_> = files.iter().map(|(p, t)| manifest(p, t)).collect();
        Workspace::new(&manifests, &mut Work::new(1_000_000), &mut || Ok(())).unwrap()
    }

    const ROOT_TOML: &str = "[workspace]\nmembers = ['crates/*']\nexclude = ['crates/skip']\n[workspace.dependencies]\nshared = { path = 'crates/shared' }\n";

    /// Edition 2018 or later is proved only by a literal or root-inherited `edition`.
    #[test]
    fn uniform_paths_need_a_proved_edition() {
        let ws = workspace(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = ['*', 'host/*/*', 'implicit/sharer']\n[workspace.package]\nedition = '2021'\n",
            ),
            (
                "inherits/Cargo.toml",
                "[package]\nname = 'inherits'\nedition.workspace = true\n",
            ),
            (
                "literal/Cargo.toml",
                "[package]\nname = 'literal'\nedition = '2018'\n",
            ),
            ("absent/Cargo.toml", "[package]\nname = 'absent'\n"),
            (
                "old/Cargo.toml",
                "[package]\nname = 'old'\nedition = '2015'\n",
            ),
            (
                "target/Cargo.toml",
                "[package]\nname = 'target'\nedition = '2021'\n[[bin]]\nname = 'target'\nedition = '2015'\n",
            ),
            (
                "elsewhere/Cargo.toml",
                "[package]\nname = 'elsewhere'\nedition.workspace = true\nworkspace = '../other'\n",
            ),
            // Any target can set an edition of its own and compile any file under it.
            (
                "example/Cargo.toml",
                "[package]\nname = 'example'\nedition = '2021'\n[[example]]\nname = 'old'\nedition = '2015'\npath = 'src/lib.rs'\ncrate-type = ['lib']\n",
            ),
            (
                "test/Cargo.toml",
                "[package]\nname = 'test'\nedition = '2021'\n[[test]]\nname = 't'\nedition = '2021'\n",
            ),
            (
                "bench/Cargo.toml",
                "[package]\nname = 'bench'\nedition = '2021'\n[[bench]]\nname = 'b'\nedition = '2018'\n",
            ),
            (
                "lib/Cargo.toml",
                "[package]\nname = 'lib'\nedition = '2021'\n[lib]\nedition = '2021'\n",
            ),
            (
                "shape/Cargo.toml",
                "bin = ['not a table']\n[package]\nname = 'shape'\nedition = '2021'\n",
            ),
            (
                "unstable/Cargo.toml",
                "cargo-features = ['edition2027']\n[package]\nname = 'unstable'\nedition = '2024'\n",
            ),
            (
                "number/Cargo.toml",
                "[package]\nname = 'number'\nedition = 2021\n",
            ),
            // `plain` proves its own roots, but `borrower` names one of them as a target.
            (
                "plain/Cargo.toml",
                "[package]\nname = 'plain'\nedition = '2021'\n[[example]]\nname = 'e'\n",
            ),
            // `discovered` is a member, and its library root is also an example of `host`.
            (
                "host/Cargo.toml",
                "[package]\nname = 'host'\nedition = '2015'\n",
            ),
            (
                "host/examples/discovered/Cargo.toml",
                "[package]\nname = 'discovered'\nedition = '2021'\n[lib]\npath = 'main.rs'\n",
            ),
            (
                "host/crates/control/Cargo.toml",
                "[package]\nname = 'control'\nedition = '2021'\n",
            ),
            // A custom-build target is one more target: `builder` compiles `built`'s root.
            (
                "built/Cargo.toml",
                "[package]\nname = 'built'\nedition = '2021'\nbuild = false\n",
            ),
            (
                "builder/Cargo.toml",
                "[package]\nname = 'builder'\nedition = '2015'\nbuild = '../built/src/lib.rs'\n",
            ),
            // The conventional `build.rs` of `implicit` is `sharer`'s library root.
            (
                "implicit/Cargo.toml",
                "[package]\nname = 'implicit'\nedition = '2015'\n",
            ),
            (
                "implicit/sharer/Cargo.toml",
                "[package]\nname = 'sharer'\nedition = '2021'\n[lib]\npath = '../build.rs'\n",
            ),
            (
                "borrower/Cargo.toml",
                "[package]\nname = 'borrower'\n[[test]]\nname = 't'\npath = '../plain/src/lib.rs'\n",
            ),
        ]);
        let uniform = |member: &str| ws.uniform_paths("s", &format!("{member}/src/lib.rs"));
        assert!(uniform("inherits"));
        assert!(uniform("literal"));
        for member in [
            "absent",
            "old",
            "target",
            "elsewhere",
            "unknown",
            "example",
            "test",
            "bench",
            "lib",
            "shape",
            "unstable",
            "number",
            "plain",
            "borrower",
            "built",
            "builder",
        ] {
            assert!(!uniform(member), "{member}");
        }
        assert!(ws.uniform_paths("s", "plain/src/main.rs"));
        assert!(ws.uniform_paths("s", "built/src/main.rs"));
        assert!(!ws.uniform_paths("s", "implicit/build.rs"));
        assert!(!ws.uniform_paths("s", "host/examples/discovered/main.rs"));
        assert!(ws.uniform_paths("s", "host/crates/control/src/lib.rs"));
        assert!(ws.uniform_paths("s", "literal/src/main.rs"));
        assert!(!ws.uniform_paths("other", "literal/src/lib.rs"));
        // An absolute target or build path anywhere relates to no file, so nothing is proved.
        let package = "[package]\nname = 'modern'\nedition = '2021'\n";
        for other in [
            "[package]\nname = 'old'\n[lib]\npath = '/checkout/src/lib.rs'\n",
            "[package]\nname = 'old'\nbuild = '/checkout/src/lib.rs'\n",
            "[package]\nname = 'old'\n[[test]]\nname = 't'\npath = 'C:\\\\checkout\\\\src\\\\lib.rs'\n",
        ] {
            let ws = workspace(&[("Cargo.toml", package), ("old/Cargo.toml", other)]);
            assert!(!ws.uniform_paths("s", "src/lib.rs"), "{other}");
        }
        let ws = workspace(&[
            ("Cargo.toml", package),
            ("old/Cargo.toml", "[package]\nname = 'old'\nbuild = false\n"),
        ]);
        assert!(ws.uniform_paths("s", "src/lib.rs"));
    }

    #[test]
    fn members_bind_path_renamed_and_inherited_dependencies() {
        let ws = workspace(&[
            ("Cargo.toml", ROOT_TOML),
            (
                "crates/app/Cargo.toml",
                "[package]\nname = 'app'\n[dependencies]\ncore-lib = { path = '../core' }\nalias = { package = 'util', path = '../util' }\nshared = { workspace = true }\nserde = '1'\n",
            ),
            ("crates/core/Cargo.toml", "[package]\nname = 'core-lib'\n"),
            (
                "crates/util/Cargo.toml",
                "[package]\nname = 'util'\n[lib]\nname = 'utl'\npath = 'lib.rs'\n",
            ),
            ("crates/shared/Cargo.toml", "[package]\nname = 'shared'\n"),
            ("crates/skip/Cargo.toml", "[package]\nname = 'skip'\n"),
        ]);
        let app = ws
            .crate_root("s", "crates/app/src/main.rs")
            .unwrap()
            .clone();
        assert!(!app.library);
        let externs: BTreeMap<_, _> = ws.externs("s", &app).into_iter().collect();
        assert_eq!(
            externs,
            BTreeMap::from([
                ("alias".to_owned(), Some("crates/util/lib.rs")),
                ("core_lib".to_owned(), Some("crates/core/src/lib.rs")),
                ("shared".to_owned(), Some("crates/shared/src/lib.rs")),
                // The binary's own package has no library root here.
                ("app".to_owned(), Some("crates/app/src/lib.rs")),
            ])
        );
        assert!(ws.crate_root("s", "crates/skip/src/lib.rs").is_none());
    }

    #[test]
    fn a_binary_binds_its_own_library_beside_explicit_bins() {
        let ws = workspace(&[(
            "Cargo.toml",
            "[package]\nname = 'my-tool'\nedition = '2021'\n[[bin]]\nname = 'stub'\npath = 'src/bin/stub.rs'\n",
        )]);
        let main = ws.crate_root("s", "src/main.rs").unwrap().clone();
        assert!(!main.library);
        assert_eq!(
            ws.externs("s", &main),
            vec![("my_tool".to_owned(), Some("src/lib.rs"))]
        );
        let lib = ws.crate_root("s", "src/lib.rs").unwrap().clone();
        assert!(ws.externs("s", &lib).is_empty());
        let ws = workspace(&[(
            "Cargo.toml",
            "[package]\nname = 'my-tool'\nautobins = false\n[[bin]]\nname = 'stub'\npath = 'src/bin/stub.rs'\n",
        )]);
        assert!(ws.crate_root("s", "src/main.rs").is_none());
    }

    /// `src/main.rs` follows Cargo's target auto-discovery beside explicit `[[bin]]` entries
    /// (#1190): one root when a `[[bin]]` also names it, none when a `[[bin]]` takes the
    /// package's name for another file or edition 2015 turns discovery off.
    #[test]
    fn src_main_follows_cargo_auto_discovery_beside_explicit_bins() {
        let main = |manifest: &str| {
            workspace(&[("Cargo.toml", manifest)])
                .crate_root("s", "src/main.rs")
                .cloned()
        };
        let stub = "[[bin]]\nname = 'stub'\npath = 'src/bin/stub.rs'\n";
        for named in [
            format!("[package]\nname = 'tool'\nedition = '2021'\n{stub}"),
            format!(
                "[package]\nname = 'tool'\nedition = '2021'\n{stub}[[bin]]\nname = 'tool'\npath = 'src/main.rs'\n"
            ),
            format!("[package]\nname = 'tool'\nedition = '2021'\n{stub}[[bin]]\nname = 'tool'\n"),
            format!(
                "[package]\nname = 'tool'\n{stub}[[bin]]\nname = 'main'\npath = 'src/main.rs'\n"
            ),
            format!("[package]\nname = 'tool'\nautobins = true\n{stub}"),
        ] {
            let root = main(&named).unwrap_or_else(|| panic!("{named}"));
            assert_eq!(
                root,
                CrateRoot {
                    member: 0,
                    library: false
                },
                "{named}"
            );
        }
        for unbuilt in [
            format!("[package]\nname = 'tool'\nedition = '2021'\nautobins = false\n{stub}"),
            format!("[package]\nname = 'tool'\n{stub}"),
            format!("[package]\nname = 'tool'\nedition = '2015'\n{stub}"),
            "[package]\nname = 'tool'\nedition = '2021'\n[[bin]]\nname = 'tool'\npath = 'src/bin/tool.rs'\n".to_owned(),
        ] {
            assert!(main(&unbuilt).is_none(), "{unbuilt}");
        }
        assert!(main("[package]\nname = 'tool'\n").is_some());
    }

    #[test]
    fn duplicate_package_names_and_unlisted_manifests_prove_nothing() {
        let ws = workspace(&[
            ("Cargo.toml", "[workspace]\nmembers = ['a', 'b', 'c']\n"),
            (
                "a/Cargo.toml",
                "[package]\nname = 'a'\n[dependencies]\ndup = { path = '../b' }\n",
            ),
            ("b/Cargo.toml", "[package]\nname = 'dup'\n"),
            ("c/Cargo.toml", "[package]\nname = 'dup'\n"),
            ("d/Cargo.toml", "[package]\nname = 'd'\n"),
        ]);
        let a = ws.crate_root("s", "a/src/lib.rs").unwrap().clone();
        assert_eq!(ws.externs("s", &a), vec![("dup".to_owned(), None)]);
        assert!(ws.crate_root("s", "d/src/lib.rs").is_none());
    }

    #[test]
    fn malformed_manifests_are_skipped_without_failing() {
        for root in [
            "[workspace",
            "workspace = 1",
            "[workspace]\nmembers = 'x'",
            "\u{0}",
        ] {
            let ws = workspace(&[
                ("Cargo.toml", root),
                ("x/Cargo.toml", "[package]\nname='x'\n"),
            ]);
            assert!(ws.crate_root("s", "x/src/lib.rs").is_none(), "{root}");
        }
        let ws = workspace(&[
            ("Cargo.toml", "[workspace]\nmembers = ['x', 'y']\n"),
            ("x/Cargo.toml", "[package"),
            ("y/Cargo.toml", "[package]\nname = 'y'\n"),
        ]);
        assert!(ws.crate_root("s", "x/src/lib.rs").is_none());
        assert!(ws.crate_root("s", "y/src/lib.rs").is_some());
    }

    #[test]
    fn parsing_is_charged_to_the_work_budget() {
        let manifests = [manifest("Cargo.toml", ROOT_TOML)];
        let error = Workspace::new(&manifests, &mut Work::new(4), &mut || Ok(())).unwrap_err();
        assert!(error.to_string().starts_with("bounded_work:"), "{error}");
    }
}
