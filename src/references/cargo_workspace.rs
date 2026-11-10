//! Cargo workspace member crates for Rust structural resolution (#772, owner-ratified
//! 2026-11-10). Read from the captured root `Cargo.toml` and the retained member manifests
//! only, bounded, executing nothing: no `cargo metadata`, build script or network fetch.
//!
//! - The members are the directories of the retained nested `Cargo.toml` files that the root
//!   `[workspace] members` literal paths and simple globs match and `exclude` does not, plus
//!   the root package when the root manifest has `[package]`.
//! - A member's library crate root is `[lib] path`, else `src/lib.rs`, relative to the member
//!   (none when `autolib = false` without `[lib]`); its crate name is `[lib] name`, else the
//!   package name with `-` replaced by `_`. Its binary crate root is `src/main.rs`, which
//!   Cargo discovers unless `autobins = false` (then a `[[bin]]` must name it).
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
    /// Dependency bindings: extern crate name to the member it names, or `None` when the
    /// name is bound to more than one crate or to an unproved one.
    bindings: BTreeMap<String, Option<usize>>,
}

#[derive(Default, Debug)]
struct SourceSet {
    members: Vec<Member>,
    roots: BTreeMap<String, CrateRoot>,
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
fn member(dir: &str, table: &toml::Table) -> Option<Member> {
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
    // As in Cargo, `src/main.rs` is discovered unless `autobins = false`, beside any explicit
    // `[[bin]]`; with discovery off, a `[[bin]]` must name it.
    let main = join(dir, "src/main.rs")?;
    let bins = match table.get("bin") {
        None => &Vec::new(),
        Some(toml::Value::Array(bins)) => bins,
        Some(_) => return None,
    };
    let declared = bins.iter().any(|bin| {
        bin.as_table().is_some_and(|bin| match bin.get("path") {
            Some(path) => path.as_str().and_then(|p| join(dir, p)).as_ref() == Some(&main),
            None => bin.get("name").and_then(toml::Value::as_str) == Some(name),
        })
    });
    let discovered = package.get("autobins").and_then(toml::Value::as_bool) != Some(false);
    let binary = (declared || discovered).then_some(main);
    Some(Member {
        dir: dir.to_owned(),
        package: name.to_owned(),
        crate_name,
        library,
        binary,
        bindings: BTreeMap::new(),
    })
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
            let mut tables = Vec::new();
            if let Some(member) = member("", &root) {
                tables.push((member, root.clone()));
            }
            for dir in dirs {
                super::owner_step(work, check)?;
                if let Some(table) = parse(manifests[dir], work)?
                    && let Some(member) = member(dir, &table)
                {
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
            let mut set = SourceSet::default();
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
            "[package]\nname = 'my-tool'\n[[bin]]\nname = 'stub'\npath = 'src/bin/stub.rs'\n",
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
