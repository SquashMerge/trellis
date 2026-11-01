//! The Cargo target of each Rust document, derived only from authenticated inputs.
//!
//! Owner ruling 2026-10-30 (#394): each Cargo target (lib, bin, build script, example, test,
//! bench) is its own symbol namespace, so rust-analyzer keys repeated across targets (`crate/`,
//! `main().`) are distinct symbols. A target is read from the `Cargo.toml` files retained in
//! the captured native config (the root manifest and every nested one above a Rust source),
//! with Cargo's target rules, and from the captured source bytes: a document belongs to the
//! target whose crate root reaches it through `mod` declarations (`#[path]` included) or a
//! literal `include!`. A document reached by more than one target, by none, or through a
//! construct this module cannot follow exactly has no target and keeps the global key.
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Cargo target kinds, as they appear in a claim's `cargoTarget.kind`.
pub const TARGET_KINDS: [&str; 6] = ["lib", "bin", "build", "example", "test", "bench"];

/// One Cargo target: its kind, its crate root document path, and its package's `[package]`
/// name and literal version (`None` when the manifest has none or inherits it). The root path
/// is the target's namespace identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CargoTarget {
    pub kind: String,
    pub root: String,
    pub package: String,
    pub version: Option<String>,
}

impl CargoTarget {
    pub fn is_lib(&self) -> bool {
        self.kind == "lib"
    }
}

/// How a module file is reached: by a conventional `mod name;`, by `#[path]`, or by a literal
/// `include!`.
#[derive(Debug, Clone)]
enum ModRef {
    Conventional { inline: Vec<String>, name: String },
    Path { inline: Vec<String>, path: String },
    Include(String),
}

#[derive(Debug, Default, Clone)]
struct FileModules {
    refs: Vec<ModRef>,
    /// A construct that names a module file this module cannot resolve exactly.
    uncertain: bool,
}

/// One package manifest: its directory (`""` for the workspace root) and its parsed table,
/// or `None` when it could not be parsed.
struct Manifest {
    dir: String,
    table: Option<toml::Table>,
}

/// Collects the manifests and module references of one captured revision.
pub struct CargoTargetsBuilder {
    manifests: Vec<Manifest>,
    files: BTreeMap<String, FileModules>,
}

/// The Cargo target of every Rust document whose target is determined.
#[derive(Debug, Default, Clone)]
pub struct CargoTargets {
    owners: BTreeMap<String, CargoTarget>,
}

impl CargoTargets {
    /// The target of a Rust document, or `None` when it is not determined.
    pub fn target_of(&self, path: &str) -> Option<&CargoTarget> {
        self.owners.get(path)
    }
}

impl CargoTargetsBuilder {
    /// Reads the present `Cargo.toml` entries of a retained native config.
    pub fn from_native_config(config: &[Value]) -> Self {
        let mut manifests = Vec::new();
        for entry in config {
            let (Some(path), Some("present"), Some(bytes)) = (
                entry["path"].as_str(),
                entry["type"].as_str(),
                entry["bytes"].as_str(),
            ) else {
                continue;
            };
            let dir = match path.strip_suffix("Cargo.toml") {
                Some("") => "",
                Some(dir) => match dir.strip_suffix('/') {
                    Some(dir) => dir,
                    None => continue,
                },
                None => continue,
            };
            if manifests.iter().any(|m: &Manifest| m.dir == dir) {
                continue;
            }
            let table = hex::decode(bytes)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .and_then(|text| toml::from_str::<toml::Table>(&text).ok());
            manifests.push(Manifest {
                dir: dir.to_owned(),
                table,
            });
        }
        Self {
            manifests,
            files: BTreeMap::new(),
        }
    }

    /// Whether any manifest was retained; without one no document has a target.
    pub fn is_empty(&self) -> bool {
        self.manifests.is_empty()
    }

    /// Records the module references of one captured Rust document.
    pub fn add_document(&mut self, path: &str, source: &[u8]) {
        if self.is_empty() {
            return;
        }
        // A source Trellis didn't read (#576) could declare any module: its placeholder names
        // none, so it is as uncertain as one that can't be parsed.
        let unread = std::str::from_utf8(source)
            .is_ok_and(|text| crate::skipped_source::skipped("rust", text).is_some());
        let modules = (!unread).then(|| module_refs(source)).flatten();
        self.files.insert(
            path.to_owned(),
            modules.unwrap_or(FileModules {
                refs: vec![],
                uncertain: true,
            }),
        );
    }

    /// Resolves every document's target.
    pub fn finish(self) -> CargoTargets {
        let documents: BTreeSet<&str> = self.files.keys().map(String::as_str).collect();
        let mut targets: Vec<(usize, CargoTarget)> = Vec::new();
        let mut poisoned = BTreeSet::new();
        for (package, manifest) in self.manifests.iter().enumerate() {
            let Some(table) = &manifest.table else {
                // An unreadable manifest may declare any target: nothing below it is owned.
                poisoned.insert(package);
                continue;
            };
            match package_targets(&manifest.dir, table, &documents) {
                Some(found) => targets.extend(found.into_iter().map(|t| (package, t))),
                None => {
                    poisoned.insert(package);
                }
            }
        }
        let mut owners: BTreeMap<&str, BTreeSet<usize>> = BTreeMap::new();
        for (index, (package, target)) in targets.iter().enumerate() {
            // A partial reach still records its owners, so a file it shares stays shared.
            let (reached, complete) = reach(&target.root, &self.files, &documents);
            if !complete {
                poisoned.insert(*package);
            }
            for path in reached {
                owners.entry(path).or_default().insert(index);
            }
        }
        // Nothing below a manifest whose targets could not all be followed is owned.
        let poisoned_dirs: Vec<&str> = poisoned
            .iter()
            .map(|package| self.manifests[*package].dir.as_str())
            .collect();
        let mut result = BTreeMap::new();
        for (path, owning) in owners {
            let [index] = owning.into_iter().collect::<Vec<_>>()[..] else {
                continue;
            };
            let target = &targets[index].1;
            // A root shared by two targets is no single namespace.
            let shared = targets
                .iter()
                .filter(|(_, other)| other.root == target.root)
                .count()
                > 1;
            let poisoned = poisoned_dirs
                .iter()
                .any(|dir| dir.is_empty() || path.starts_with(&format!("{dir}/")));
            if !shared && !poisoned {
                result.insert(path.to_owned(), target.clone());
            }
        }
        CargoTargets { owners: result }
    }
}

/// `dir/path`, normalized; `None` when it leaves the workspace or is not a document path.
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
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Every document a crate root reaches, and whether every file it reached names only modules
/// this module can follow exactly. A root that is not a captured document reaches nothing.
fn reach<'d>(
    root: &str,
    files: &BTreeMap<String, FileModules>,
    documents: &BTreeSet<&'d str>,
) -> (BTreeSet<&'d str>, bool) {
    let mut reached = BTreeSet::new();
    let Some(root) = documents.get(root).copied() else {
        return (reached, true);
    };
    // (file, owns its directory like a crate root or mod.rs, reached through include!)
    let mut pending = vec![(root, true, false)];
    let mut seen = BTreeSet::new();
    let mut complete = true;
    while let Some((file, mod_rs, included)) = pending.pop() {
        reached.insert(file);
        if !seen.insert((file, mod_rs)) {
            continue;
        }
        let Some(modules) = files.get(file) else {
            complete = false;
            continue;
        };
        // An included file's own modules resolve against its includer; not followed.
        if modules.uncertain
            || included
                && modules
                    .refs
                    .iter()
                    .any(|r| !matches!(r, ModRef::Include(_)))
        {
            complete = false;
            continue;
        }
        let owned = if mod_rs {
            parent(file).to_owned()
        } else {
            file.strip_suffix(".rs").unwrap_or(file).to_owned()
        };
        for module in &modules.refs {
            let (candidates, child_mod_rs, child_included) = match module {
                ModRef::Conventional { inline, name } => {
                    let base = join(&owned, &inline.join("/")).unwrap_or_default();
                    let prefix = if base.is_empty() {
                        name.clone()
                    } else {
                        format!("{base}/{name}")
                    };
                    (
                        vec![format!("{prefix}.rs"), format!("{prefix}/mod.rs")],
                        None,
                        false,
                    )
                }
                ModRef::Path { inline, path } => {
                    let base = if inline.is_empty() {
                        parent(file).to_owned()
                    } else {
                        join(&owned, &inline.join("/")).unwrap_or_default()
                    };
                    (join(&base, path).into_iter().collect(), Some(true), false)
                }
                ModRef::Include(path) => (
                    join(parent(file), path).into_iter().collect::<Vec<_>>(),
                    Some(mod_rs),
                    true,
                ),
            };
            let found: Vec<&str> = candidates
                .iter()
                .filter_map(|c| documents.get(c.as_str()).copied())
                .collect();
            match found[..] {
                // A declared module whose file was not captured may declare modules shared
                // with another target: never assign around it. An included file that was not
                // captured (a build input such as `build/*.rs` outside the sources) declares
                // nothing this module follows.
                [] if child_included => {}
                [] => complete = false,
                [child] => {
                    let child_mod_rs = child_mod_rs.unwrap_or_else(|| child.ends_with("/mod.rs"));
                    pending.push((child, child_mod_rs, child_included));
                }
                // Both `name.rs` and `name/mod.rs`: rustc rejects it; never guess.
                _ => complete = false,
            }
        }
    }
    (reached, complete)
}

fn bool_field(table: &toml::Table, key: &str) -> Result<Option<bool>, ()> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Boolean(value)) => Ok(Some(*value)),
        Some(_) => Err(()),
    }
}

/// The targets one package manifest declares or Cargo infers, by Cargo's target rules.
/// `None` when the manifest declares something this module cannot read exactly.
fn package_targets(
    dir: &str,
    manifest: &toml::Table,
    documents: &BTreeSet<&str>,
) -> Option<Vec<CargoTarget>> {
    let Some(package) = manifest.get("package") else {
        // A virtual (workspace-only) manifest declares no target.
        return Some(vec![]);
    };
    let package = package.as_table()?;
    let name = package.get("name")?.as_str()?;
    let version = package
        .get("version")
        .and_then(toml::Value::as_str)
        .map(str::to_owned);
    let edition_2015 = match package.get("edition") {
        None => Some(true),
        Some(toml::Value::String(edition)) => Some(edition == "2015"),
        // Inherited from the workspace: unknown here.
        Some(_) => None,
    };
    let exists = |path: &str| documents.contains(path);
    let at = |relative: &str| join(dir, relative);
    // One entry per Cargo target, never deduplicated: two targets on one crate root (Cargo
    // accepts two `[[bin]]` with the same `path`) must stay two, so `finish` sees the root
    // shared. An inferred target is skipped where an explicit one claims its name or path.
    let mut targets = Vec::new();
    let mut push = |kind: &str, root: String| {
        targets.push(CargoTarget {
            kind: kind.to_owned(),
            root,
            package: name.to_owned(),
            version: version.clone(),
        });
    };
    // Library: `[lib]` with its path, else `src/lib.rs` unless `autolib = false`.
    let autolib = bool_field(package, "autolib").ok()?.unwrap_or(true);
    match manifest.get("lib") {
        Some(lib) => {
            let lib = lib.as_table()?;
            let path = match lib.get("path") {
                Some(path) => at(path.as_str()?)?,
                None => at("src/lib.rs")?,
            };
            push("lib", path);
        }
        None if autolib => {
            let path = at("src/lib.rs")?;
            if exists(&path) {
                push("lib", path);
            }
        }
        None => {}
    }
    // Build script: `package.build` as a path or a boolean, else `build.rs`.
    match package.get("build") {
        Some(toml::Value::String(path)) => push("build", at(path)?),
        Some(toml::Value::Boolean(false)) => {}
        Some(toml::Value::Boolean(true)) | None => {
            let path = at("build.rs")?;
            if exists(&path) {
                push("build", path);
            }
        }
        Some(_) => return None,
    }
    // Binaries, examples, tests and benches: explicit arrays plus Cargo's inferred targets.
    for (kind, section, auto, directory) in [
        ("bin", "bin", "autobins", "src/bin"),
        ("example", "example", "autoexamples", "examples"),
        ("test", "test", "autotests", "tests"),
        ("bench", "bench", "autobenches", "benches"),
    ] {
        let mut inferred: BTreeMap<String, String> = BTreeMap::new();
        if kind == "bin" {
            let main = at("src/main.rs")?;
            if exists(&main) {
                inferred.insert(name.to_owned(), main);
            }
        }
        let prefix = format!("{}/", at(directory)?);
        for document in documents.range::<&str, _>(prefix.as_str()..) {
            let Some(rest) = document.strip_prefix(&prefix) else {
                break;
            };
            match rest.split('/').collect::<Vec<_>>()[..] {
                [file] => {
                    if let Some(stem) = file.strip_suffix(".rs") {
                        inferred
                            .entry(stem.to_owned())
                            .or_insert((*document).to_owned());
                    }
                }
                [stem, "main.rs"] => {
                    inferred
                        .entry(stem.to_owned())
                        .or_insert((*document).to_owned());
                }
                _ => {}
            }
        }
        let explicit = match manifest.get(section) {
            None => vec![],
            Some(toml::Value::Array(entries)) => entries.iter().collect(),
            Some(_) => return None,
        };
        let mut explicit_names = BTreeSet::new();
        let mut explicit_paths = BTreeSet::new();
        for entry in &explicit {
            let entry = entry.as_table()?;
            let path = match (entry.get("path"), entry.get("name")) {
                (Some(path), _) => at(path.as_str()?)?,
                (None, Some(target_name)) => {
                    let target_name = target_name.as_str()?;
                    match inferred.get(target_name) {
                        Some(path) => path.clone(),
                        None => at(&format!("{directory}/{target_name}.rs"))?,
                    }
                }
                (None, None) => return None,
            };
            if let Some(target_name) = entry.get("name") {
                explicit_names.insert(target_name.as_str()?.to_owned());
            }
            explicit_paths.insert(path.clone());
            push(kind, path);
        }
        let discover = match bool_field(package, auto).ok()? {
            Some(value) => value,
            // Edition 2015 turns discovery off for a kind with explicit targets.
            None => manifest.get(section).is_none() || !edition_2015?,
        };
        if discover {
            for (target_name, path) in inferred {
                if !explicit_names.contains(&target_name) && !explicit_paths.contains(&path) {
                    push(kind, path);
                }
            }
        }
    }
    Some(targets)
}

/// The module references of one Rust source, or `None` when it cannot be parsed.
fn module_refs(source: &[u8]) -> Option<FileModules> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let text = |node: tree_sitter::Node<'_>| std::str::from_utf8(&source[node.byte_range()]).ok();
    let mut modules = FileModules::default();
    // (node, inline module path when the node is an item list of the file's module tree)
    let mut pending = vec![(tree.root_node(), Some(Vec::<String>::new()))];
    while let Some((node, inline)) = pending.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "mod_item" => {
                    let Some(name) = child.child_by_field_name("name").and_then(text) else {
                        modules.uncertain = true;
                        continue;
                    };
                    let path = match path_attribute(child, &text) {
                        Ok(path) => path,
                        Err(()) => {
                            modules.uncertain = true;
                            continue;
                        }
                    };
                    match (child.child_by_field_name("body"), &inline) {
                        (Some(body), Some(inline)) => {
                            // A `#[path]` on an inline module moves its nested modules.
                            if path.is_some() {
                                modules.uncertain = true;
                            }
                            let mut nested = inline.clone();
                            nested.push(name.to_owned());
                            pending.push((body, Some(nested)));
                        }
                        (Some(body), None) => pending.push((body, None)),
                        (None, Some(inline)) => modules.refs.push(match path {
                            Some(path) => ModRef::Path {
                                inline: inline.clone(),
                                path,
                            },
                            None => ModRef::Conventional {
                                inline: inline.clone(),
                                name: name.to_owned(),
                            },
                        }),
                        // A file module declared in a block.
                        (None, None) => modules.uncertain = true,
                    }
                }
                "macro_invocation" => {
                    let is_include = child
                        .child_by_field_name("macro")
                        .and_then(text)
                        .is_some_and(|name| {
                            matches!(name, "include" | "std::include" | "core::include")
                        });
                    if is_include {
                        if let Some(path) = include_path(child, &text) {
                            modules.refs.push(ModRef::Include(path));
                        }
                    } else if declares_file_module(child, &text) {
                        // `mod name;` inside a macro (`cfg_if!`) is invisible to the tree.
                        modules.uncertain = true;
                    }
                }
                _ => pending.push((child, None)),
            }
        }
    }
    Some(modules)
}

/// Whether a macro invocation's tokens, string literals aside, hold `mod <name>;`.
fn declares_file_module<'s>(
    invocation: tree_sitter::Node<'_>,
    text: &impl Fn(tree_sitter::Node<'_>) -> Option<&'s str>,
) -> bool {
    let mut leaves = Vec::new();
    let mut pending = vec![invocation];
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "string_literal" | "raw_string_literal" | "char_literal"
        ) || node.kind().ends_with("comment")
        {
            continue;
        }
        if node.child_count() == 0 {
            leaves.push(text(node).unwrap_or_default());
            continue;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    leaves
        .windows(3)
        .any(|w| w[0] == "mod" && w[2] == ";" && w[1] != ";")
}

/// The literal of a `#[path = "..."]` attribute on `item`; `Err` for a `path` this module
/// cannot read exactly (a non-literal, an escape, or one under `cfg_attr`).
fn path_attribute<'s>(
    item: tree_sitter::Node<'_>,
    text: &impl Fn(tree_sitter::Node<'_>) -> Option<&'s str>,
) -> Result<Option<String>, ()> {
    let mut found = None;
    let mut sibling = item.prev_named_sibling();
    while let Some(node) = sibling {
        match node.kind() {
            "line_comment" | "block_comment" => {}
            "attribute_item" => {
                let attribute = text(node).ok_or(())?;
                let compact: String = attribute.chars().filter(|c| !c.is_whitespace()).collect();
                if compact.starts_with("#[path=") {
                    let literal = string_literal(node, text).ok_or(())?;
                    if found.replace(literal).is_some() {
                        return Err(());
                    }
                } else if compact.starts_with("#[cfg_attr(") && compact.contains("path") {
                    return Err(());
                }
            }
            _ => break,
        }
        sibling = node.prev_named_sibling();
    }
    Ok(found)
}

/// The single plain string literal under `node`, without escapes.
fn string_literal<'s>(
    node: tree_sitter::Node<'_>,
    text: &impl Fn(tree_sitter::Node<'_>) -> Option<&'s str>,
) -> Option<String> {
    let mut literals = Vec::new();
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "string_literal" | "raw_string_literal") {
            literals.push(node);
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    let [literal] = literals[..] else {
        return None;
    };
    let mut cursor = literal.walk();
    let content: Vec<_> = literal.named_children(&mut cursor).collect();
    match &content[..] {
        [part] if part.kind() == "string_content" => {
            let value = text(*part)?;
            (!value.contains('\\') || literal.kind() == "raw_string_literal")
                .then(|| value.to_owned())
        }
        _ => None,
    }
}

/// The path of `include!("...")` when its only argument is a plain string literal.
fn include_path<'s>(
    invocation: tree_sitter::Node<'_>,
    text: &impl Fn(tree_sitter::Node<'_>) -> Option<&'s str>,
) -> Option<String> {
    let mut cursor = invocation.walk();
    let tokens = invocation
        .named_children(&mut cursor)
        .find(|n| n.kind() == "token_tree")?;
    let mut cursor = tokens.walk();
    let [argument] = tokens.named_children(&mut cursor).collect::<Vec<_>>()[..] else {
        return None;
    };
    matches!(argument.kind(), "string_literal" | "raw_string_literal")
        .then(|| string_literal(argument, text))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn targets(manifests: &[(&str, &str)], files: &[(&str, &str)]) -> CargoTargets {
        let config: Vec<Value> = manifests
            .iter()
            .map(|(path, text)| {
                json!({"path": path, "type": "present", "bytes": hex::encode(text), "byteLength": text.len()})
            })
            .chain([json!({"path": "package.json", "type": "present", "bytes": "7b7d"})])
            .collect();
        let mut builder = CargoTargetsBuilder::from_native_config(&config);
        for (path, source) in files {
            builder.add_document(path, source.as_bytes());
        }
        builder.finish()
    }

    fn of(targets: &CargoTargets, path: &str) -> Option<(String, String)> {
        targets
            .target_of(path)
            .map(|t| (t.kind.clone(), t.root.clone()))
    }

    fn owned(kind: &str, root: &str) -> Option<(String, String)> {
        Some((kind.to_owned(), root.to_owned()))
    }

    const PACKAGE: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

    /// #576 review: a crate root Trellis didn't read could still own a module, so a module the
    /// readable control shares between two targets never gets the other one as its owner.
    #[test]
    fn an_unread_crate_root_leaves_its_possible_modules_without_a_target() {
        use crate::skipped_source::{Skip, placeholder};
        let files = |lib: &str| {
            targets(
                &[("Cargo.toml", PACKAGE)],
                &[
                    ("src/lib.rs", lib),
                    ("src/main.rs", "mod shared; fn main() {}\n"),
                    ("src/shared.rs", "pub fn f() {}\n"),
                ],
            )
        };
        assert_eq!(of(&files("mod shared;\n"), "src/shared.rs"), None);
        for skip in [Skip::Oversized, Skip::NotUtf8] {
            let unread = files(&placeholder("rust", skip));
            assert_eq!(of(&unread, "src/shared.rs"), None, "{skip:?}");
        }
    }

    #[test]
    fn conventional_layout_assigns_each_target_its_module_tree() {
        let targets = targets(
            &[("Cargo.toml", PACKAGE)],
            &[
                ("src/lib.rs", "pub mod util;\nmod inner { mod deep; }"),
                ("src/util.rs", "mod helper;"),
                ("src/util/helper.rs", ""),
                ("src/inner/deep.rs", ""),
                ("src/main.rs", "mod cli;\nfn main() {}"),
                ("src/cli/mod.rs", "mod args;"),
                ("src/cli/args.rs", ""),
                ("src/bin/tool.rs", "fn main() {}"),
                ("src/bin/multi/main.rs", "mod part;"),
                ("src/bin/multi/part.rs", ""),
                ("build.rs", "fn main() {}"),
                ("examples/demo.rs", "fn main() {}"),
                ("tests/it.rs", "mod common;\nfn main() {}"),
                ("tests/other.rs", "mod common;"),
                ("tests/common/mod.rs", ""),
                ("benches/b.rs", ""),
                ("src/orphan.rs", ""),
            ],
        );
        assert_eq!(of(&targets, "src/lib.rs"), owned("lib", "src/lib.rs"));
        assert_eq!(of(&targets, "src/util.rs"), owned("lib", "src/lib.rs"));
        assert_eq!(
            of(&targets, "src/util/helper.rs"),
            owned("lib", "src/lib.rs")
        );
        assert_eq!(
            of(&targets, "src/inner/deep.rs"),
            owned("lib", "src/lib.rs")
        );
        assert_eq!(of(&targets, "src/main.rs"), owned("bin", "src/main.rs"));
        assert_eq!(of(&targets, "src/cli/mod.rs"), owned("bin", "src/main.rs"));
        assert_eq!(of(&targets, "src/cli/args.rs"), owned("bin", "src/main.rs"));
        assert_eq!(
            of(&targets, "src/bin/tool.rs"),
            owned("bin", "src/bin/tool.rs")
        );
        assert_eq!(
            of(&targets, "src/bin/multi/part.rs"),
            owned("bin", "src/bin/multi/main.rs")
        );
        assert_eq!(of(&targets, "build.rs"), owned("build", "build.rs"));
        assert_eq!(
            of(&targets, "examples/demo.rs"),
            owned("example", "examples/demo.rs")
        );
        assert_eq!(of(&targets, "tests/it.rs"), owned("test", "tests/it.rs"));
        assert_eq!(of(&targets, "benches/b.rs"), owned("bench", "benches/b.rs"));
        // Shared by two test crates, or reached by none: no target.
        assert_eq!(of(&targets, "tests/common/mod.rs"), None);
        assert_eq!(of(&targets, "src/orphan.rs"), None);
    }

    #[test]
    fn path_attributes_includes_and_explicit_targets_follow_cargo_and_rustc() {
        let manifest = format!(
            "{PACKAGE}build = \"tools/gen.rs\"\nautoexamples = false\n[lib]\npath = \"lib/root.rs\"\n\
             [[bin]]\nname = \"named\"\npath = \"cli/entry.rs\"\n[[test]]\nname = \"suite\"\n"
        );
        let targets = targets(
            &[("Cargo.toml", &manifest)],
            &[
                (
                    "lib/root.rs",
                    "#[path = \"../shared/x.rs\"]\nmod x;\nmod plain;\ninclude!(\"uncaptured.rs\");",
                ),
                ("lib/plain.rs", ""),
                ("shared/x.rs", "mod y;"),
                ("shared/y.rs", ""),
                ("cli/entry.rs", "include!(\"generated.rs\");"),
                ("cli/generated.rs", ""),
                ("tools/gen.rs", ""),
                ("build.rs", ""),
                ("examples/skipped.rs", ""),
                ("tests/suite.rs", "#[path = \"../shared/x.rs\"] mod x;"),
                ("tests/auto.rs", ""),
            ],
        );
        assert_eq!(of(&targets, "lib/plain.rs"), owned("lib", "lib/root.rs"));
        // `#[path]` files own their directory; shared with the test crate, so no target.
        assert_eq!(of(&targets, "shared/x.rs"), None);
        assert_eq!(of(&targets, "shared/y.rs"), None);
        assert_eq!(
            of(&targets, "cli/generated.rs"),
            owned("bin", "cli/entry.rs")
        );
        assert_eq!(of(&targets, "tools/gen.rs"), owned("build", "tools/gen.rs"));
        assert_eq!(of(&targets, "build.rs"), None);
        assert_eq!(of(&targets, "examples/skipped.rs"), None);
        assert_eq!(
            of(&targets, "tests/suite.rs"),
            owned("test", "tests/suite.rs")
        );
        assert_eq!(
            of(&targets, "tests/auto.rs"),
            owned("test", "tests/auto.rs")
        );
    }

    #[test]
    fn constructs_that_cannot_be_followed_exactly_leave_the_package_untargeted() {
        for lib in [
            "#[cfg_attr(unix, path = \"a.rs\")] mod a;",
            "#[path = \"a\\x2ers\"] mod a;",
            "#[path = \"x\"] mod a { mod b; }",
            "fn f() { #[path = \"a.rs\"] mod a; }",
            "mod both;",
            "mod missing;",
            "cfg_if::cfg_if! { if #[cfg(unix)] { mod a; } }",
        ] {
            let found = targets(
                &[("Cargo.toml", PACKAGE)],
                &[
                    ("src/lib.rs", lib),
                    ("src/a.rs", ""),
                    ("src/both.rs", ""),
                    ("src/both/mod.rs", ""),
                    ("tests/t.rs", ""),
                ],
            );
            assert_eq!(of(&found, "src/lib.rs"), None, "{lib}");
            assert_eq!(of(&found, "tests/t.rs"), None, "{lib}");
        }
        // An unreadable manifest, or a target kind given as a non-array, owns nothing.
        for manifest in ["[package", &format!("bin = 1\n{PACKAGE}")] {
            let unread = targets(
                &[("Cargo.toml", manifest)],
                &[("src/lib.rs", ""), ("tests/t.rs", "")],
            );
            assert_eq!(of(&unread, "src/lib.rs"), None);
        }
        // Two targets sharing a root are no single namespace.
        let manifest = format!("{PACKAGE}[[bin]]\nname = \"x\"\npath = \"src/lib.rs\"\n");
        let shared = targets(&[("Cargo.toml", &manifest)], &[("src/lib.rs", "")]);
        assert_eq!(of(&shared, "src/lib.rs"), None);
        // So are two targets of one kind: two `[[bin]]` on src/main.rs, and their modules.
        let manifest = format!(
            "{PACKAGE}[[bin]]\nname = \"a\"\npath = \"src/main.rs\"\n\
             [[bin]]\nname = \"b\"\npath = \"src/main.rs\"\n"
        );
        let shared = targets(
            &[("Cargo.toml", &manifest)],
            &[
                ("src/main.rs", "mod cli;"),
                ("src/cli.rs", ""),
                ("src/lib.rs", ""),
            ],
        );
        assert_eq!(of(&shared, "src/main.rs"), None);
        assert_eq!(of(&shared, "src/cli.rs"), None);
        assert_eq!(of(&shared, "src/lib.rs"), owned("lib", "src/lib.rs"));
        // Edition 2015 (the default) turns discovery off beside explicit targets, unless asked.
        let manifest = "[package]\nname = \"old\"\n[[test]]\nname = \"a\"\n";
        let old = targets(
            &[("Cargo.toml", manifest)],
            &[("tests/a.rs", ""), ("tests/b.rs", "")],
        );
        assert_eq!(of(&old, "tests/a.rs"), owned("test", "tests/a.rs"));
        assert_eq!(of(&old, "tests/b.rs"), None);
        // `autolib = false`: src/lib.rs is no target; an empty explicit list still counts.
        let manifest = "[package]\nname = \"x\"\nautolib = false\n";
        let no_lib = targets(
            &[("Cargo.toml", manifest)],
            &[("src/lib.rs", ""), ("tests/a.rs", "")],
        );
        assert_eq!(of(&no_lib, "src/lib.rs"), None);
        assert_eq!(of(&no_lib, "tests/a.rs"), owned("test", "tests/a.rs"));
        let manifest = "test = []\n[package]\nname = \"x\"\n";
        let none = targets(&[("Cargo.toml", manifest)], &[("tests/a.rs", "")]);
        assert_eq!(of(&none, "tests/a.rs"), None);
    }

    #[test]
    fn nested_packages_have_their_own_targets_and_virtual_manifests_none() {
        let targets = targets(
            &[
                ("Cargo.toml", "[workspace]\nmembers = [\"crates/a\"]\n"),
                ("crates/a/Cargo.toml", PACKAGE),
            ],
            &[
                ("crates/a/src/lib.rs", "mod m;"),
                ("crates/a/src/m.rs", ""),
                ("crates/a/tests/t.rs", ""),
                ("src/lib.rs", ""),
            ],
        );
        assert_eq!(
            of(&targets, "crates/a/src/m.rs"),
            owned("lib", "crates/a/src/lib.rs")
        );
        assert_eq!(
            of(&targets, "crates/a/tests/t.rs"),
            owned("test", "crates/a/tests/t.rs")
        );
        assert_eq!(of(&targets, "src/lib.rs"), None);
    }
}
