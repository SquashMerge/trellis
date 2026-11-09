//! App detection from build manifests (#784): which `Cargo.toml` and `package.json` files
//! name a runnable application. Pure: it reads manifest bytes and the snapshot's document
//! paths, and does no I/O.
#![cfg_attr(not(test), allow(dead_code))]

use crate::references::external::ManifestInput;
use crate::references::work::Work;
use anyhow::{Result, ensure};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AppKind {
    Cargo,
    Npm,
    Python,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct App {
    pub name: String,
    pub kind: AppKind,
    pub manifest: String,
    /// `.` or the checkout-relative directory of the manifest.
    pub root: String,
    pub entries: Vec<String>,
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
            matches!(file_name(path), "Cargo.toml" | "package.json")
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
    let mut out = Vec::with_capacity(listed.len());
    for manifest in listed {
        check()?;
        let path = manifest.path.as_str();
        let outcome = if file_name(path) == "package.json" {
            npm_outcome(manifest, parse(manifest, work)?)
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

/// The index of the app with the most specific root containing `path`. `.` contains every
/// path.
pub(crate) fn containing(apps: &[App], path: &str) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (index, app) in apps.iter().enumerate() {
        let depth = if app.root == "." {
            0
        } else if path == app.root
            || path
                .strip_prefix(app.root.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        {
            app.root.len() + 1
        } else {
            continue;
        };
        if best.is_none_or(|(_, deepest)| depth > deepest) {
            best = Some((index, depth));
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
    Ok(if file_name(&manifest.path) == "Cargo.toml" {
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
    let root = directory(&manifest.path);
    let name = match object.get("name").and_then(|name| name.as_str()) {
        Some(name) if !name.trim().is_empty() => name.to_owned(),
        _ => root.clone(),
    };
    Outcome::App(App {
        name,
        kind: AppKind::Npm,
        manifest: manifest.path.clone(),
        root,
        entries: Vec::new(),
    })
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
    Outcome::App(App {
        name: name.to_owned(),
        kind: AppKind::Cargo,
        manifest: manifest.path.clone(),
        root,
        entries: entries.into_iter().collect(),
    })
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
        assert_eq!(app.kind, AppKind::Cargo);
        assert_eq!(app.root, ".");
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
        assert_eq!(app(&found, "crates/alpha/Cargo.toml").root, "crates/alpha");
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
                manifest("pyproject.toml", Some(b"[project]")),
            ],
            &[],
        );
        let root = app(&found, "package.json");
        assert_eq!(
            (root.name.as_str(), root.root.as_str(), root.kind),
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
        let manifests: Vec<&str> = detected.apps.iter().map(|a| a.manifest.as_str()).collect();
        assert_eq!(manifests, ["a/package.json", "package.json"]);
        assert_eq!(detected.apps[0].name, detected.apps[1].name);
        assert_eq!(detected.unread, 1);
    }

    #[test]
    fn containing_picks_the_most_specific_root() {
        let make = |root: &str| App {
            name: root.into(),
            kind: AppKind::Npm,
            manifest: format!("{root}/package.json"),
            root: root.into(),
            entries: Vec::new(),
        };
        let apps = [make("."), make("web"), make("web/inner"), make("webx")];
        assert_eq!(containing(&apps, "README.md"), Some(0));
        assert_eq!(containing(&apps, "web/app.js"), Some(1));
        assert_eq!(containing(&apps, "web/inner/a.js"), Some(2));
        assert_eq!(containing(&apps, "web"), Some(1));
        assert_eq!(containing(&apps, "webx/a.js"), Some(3));
        assert_eq!(containing(&apps[1..], "README.md"), None);
        assert_eq!(containing(&[], "a"), None);
    }

    #[test]
    fn parsing_is_bounded_and_checked_per_manifest() {
        let manifests = [
            manifest("package.json", Some(b"{}")),
            manifest("a/package.json", Some(b"{}")),
        ];
        let documents = BTreeSet::new();
        let error =
            candidates(&manifests, &documents, &mut Work::new(3), &mut || Ok(())).unwrap_err();
        assert_eq!(error.to_string(), "bounded_work: app manifest parsing");
        let mut checks = 0;
        candidates(&manifests, &documents, &mut Work::new(100), &mut || {
            checks += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(checks, 2);
        let error = candidates(&manifests, &documents, &mut Work::new(100), &mut || {
            anyhow::bail!("expired")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "expired");
    }
}
