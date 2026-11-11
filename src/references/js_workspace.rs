//! JavaScript and TypeScript workspace packages (#773, owner-ratified 2026-11-10).
//!
//! The captured root `package.json` `workspaces` (an array, or `{packages: [...]}`) and the
//! captured root `pnpm-workspace.yaml` `packages` name member directories by literal path or
//! simple glob; a `!` pattern excludes. Each retained nested `package.json` in a member
//! directory maps its `name` to that directory. A bare `name` resolves to the member's entry
//! (`exports` when it is a single string, else `module`, else `main`, else `index.*`), and
//! `name/sub/path` to the member-relative path, by the ordinary extension rules. Nothing is
//! executed and `node_modules` is never read. Bounded, and a config this cannot read is a
//! typed reason, never a crash.
use super::Work;
use super::external::ManifestInput;
use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;

/// A workspace config this cannot read: bare specifiers that resolve nowhere else get it.
pub(super) const UNREADABLE: &str = "javascript_workspace_unreadable";
/// Two members declare the name.
pub(super) const AMBIGUOUS: &str = "javascript_workspace_package_ambiguous";
/// The member declares an `exports` map this does not follow, or a subpath its single-string
/// `exports` does not export.
pub(super) const EXPORTS: &str = "javascript_workspace_exports_unsupported";
/// The member's entry or subpath names no indexed script.
pub(super) const NOT_INDEXED: &str = "javascript_workspace_entry_not_indexed";
/// The member's entry leaves the member directory or the checkout.
pub(super) const ENTRY: &str = "javascript_workspace_entry_unsupported";

/// The most workspace patterns read from one source set.
const MAX_PATTERNS: usize = 256;
/// The longest workspace pattern read.
const MAX_PATTERN_BYTES: usize = 512;
const PNPM_WORKSPACE: &str = "pnpm-workspace.yaml";
const ROOT_MANIFEST: &str = "package.json";

/// What a member's entry is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Entry {
    /// A member-relative path from `exports` (a single string): subpaths are not exported.
    Exports(String),
    /// A member-relative path from `module` or `main`.
    Main(String),
    /// No entry field: `index.*` of the member directory.
    Index,
    /// An `exports` map this does not follow.
    Unsupported,
    /// An entry that leaves the member directory.
    Outside,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Member {
    /// The member directory, root-relative.
    pub(super) dir: String,
    pub(super) entry: Entry,
}

#[derive(Debug, PartialEq, Eq)]
enum Members {
    Unreadable,
    Names(BTreeMap<String, Option<Member>>),
}

/// The workspace members of each source set.
#[derive(Debug, Default)]
pub(super) struct Workspaces {
    sets: BTreeMap<String, Members>,
}

/// One workspace lookup: a member path to try, or a typed reason.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Lookup {
    /// A root-relative path and whether it names a directory, with the reason a miss gets.
    Path(String, bool),
    Unknown(&'static str),
}

impl Workspaces {
    pub(super) fn new(
        manifests: &[ManifestInput],
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut by_set: BTreeMap<&str, Vec<&ManifestInput>> = BTreeMap::new();
        for manifest in manifests {
            super::owner_step(work, check)?;
            if manifest.bytes.is_some() {
                by_set
                    .entry(&manifest.source_set_id)
                    .or_default()
                    .push(manifest);
            }
        }
        let mut result = Self::default();
        for (set, manifests) in by_set {
            if let Some(members) = members(&manifests, work, check)? {
                result.sets.insert(set.to_owned(), members);
            }
        }
        Ok(result)
    }

    /// The member path `specifier` names in `source_set_id`, or `None` when it names no member
    /// (and the workspace config, if any, was readable).
    pub(super) fn lookup(
        &self,
        source_set_id: &str,
        specifier: &str,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<Lookup>> {
        super::owner_step(work, check)?;
        let names = match self.sets.get(source_set_id) {
            None => return Ok(None),
            Some(Members::Unreadable) => return Ok(Some(Lookup::Unknown(UNREADABLE))),
            Some(Members::Names(names)) => names,
        };
        let Some((name, subpath)) = split(specifier) else {
            return Ok(None);
        };
        ensure!(
            work.charge(specifier.len() + name.len()),
            "bounded_work: reference workspace lookup"
        );
        check()?;
        let Some(member) = names.get(name) else {
            return Ok(None);
        };
        let Some(member) = member else {
            return Ok(Some(Lookup::Unknown(AMBIGUOUS)));
        };
        let join = |relative: &str| -> Option<String> {
            super::tsconfig::normalize(&if member.dir.is_empty() {
                relative.to_owned()
            } else {
                format!("{}/{relative}", member.dir)
            })
            .filter(|path| {
                member.dir.is_empty()
                    || path == &member.dir
                    || path.starts_with(&format!("{}/", member.dir))
            })
        };
        Ok(Some(match (subpath, &member.entry) {
            (_, Entry::Unsupported) => Lookup::Unknown(EXPORTS),
            (Some(_), Entry::Exports(_)) => Lookup::Unknown(EXPORTS),
            (Some(sub), _) => match join(sub) {
                Some(path) => Lookup::Path(path, sub.ends_with('/')),
                None => Lookup::Unknown(ENTRY),
            },
            (None, Entry::Outside) => Lookup::Unknown(ENTRY),
            (None, Entry::Index) => Lookup::Path(member.dir.clone(), true),
            (None, Entry::Exports(entry) | Entry::Main(entry)) => match join(entry) {
                Some(path) if path == member.dir => Lookup::Path(path, true),
                Some(path) => Lookup::Path(path, entry.ends_with('/')),
                None => Lookup::Unknown(ENTRY),
            },
        }))
    }
}

/// A bare specifier's package name and subpath: `name` or `@scope/name`, then `/sub/path`.
fn split(specifier: &str) -> Option<(&str, Option<&str>)> {
    let mut slashes = specifier.match_indices('/').map(|(at, _)| at);
    let end = if specifier.starts_with('@') {
        slashes.nth(1)
    } else {
        slashes.next()
    };
    let (name, subpath) = match end {
        Some(end) => (&specifier[..end], Some(&specifier[end + 1..])),
        None => (specifier, None),
    };
    (valid_name(name) && subpath.is_none_or(|sub| !sub.is_empty())).then_some((name, subpath))
}

/// An npm package name: `name` or `@scope/name`, each part nonblank lowercase-safe
/// characters, at most 214 bytes.
fn valid_name(name: &str) -> bool {
    let part = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && !part.starts_with('_')
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
    };
    name.len() <= 214
        && match name.strip_prefix('@') {
            Some(scoped) => scoped
                .split_once('/')
                .is_some_and(|(scope, name)| part(scope) && part(name)),
            None => part(name),
        }
}

/// The members of one source set, or `None` when it declares no workspace.
fn members(
    manifests: &[&ManifestInput],
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<Members>> {
    let find = |path: &str| {
        manifests
            .iter()
            .find(|manifest| manifest.path == path)
            .and_then(|manifest| manifest.bytes.as_deref())
    };
    let mut patterns = Vec::new();
    let mut declared = false;
    if let Some(bytes) = find(ROOT_MANIFEST) {
        charge(work, check, bytes.len())?;
        // A root package.json that is not JSON declares nothing here, as for npm labels.
        if let Ok(Value::Object(root)) = serde_json::from_slice::<Value>(bytes) {
            match root.get("workspaces") {
                None => {}
                Some(value) => {
                    declared = true;
                    let list = match value {
                        Value::Object(object) => object.get("packages"),
                        other => Some(other),
                    };
                    match list.and_then(strings) {
                        Some(list) => patterns.extend(list),
                        None => return Ok(Some(Members::Unreadable)),
                    }
                }
            }
        }
    }
    if let Some(bytes) = find(PNPM_WORKSPACE) {
        charge(work, check, bytes.len())?;
        declared = true;
        let parsed = std::str::from_utf8(bytes)
            .ok()
            .filter(|_| bytes != crate::skipped_config::PLACEHOLDER)
            .and_then(|text| pnpm_packages(text).ok());
        match parsed {
            Some(list) => patterns.extend(list.unwrap_or_default()),
            None => return Ok(Some(Members::Unreadable)),
        }
    }
    if !declared {
        return Ok(None);
    }
    if patterns.len() > MAX_PATTERNS || patterns.iter().any(|p| p.len() > MAX_PATTERN_BYTES) {
        return Ok(Some(Members::Unreadable));
    }
    let mut include = globset::GlobSetBuilder::new();
    let mut exclude = globset::GlobSetBuilder::new();
    for pattern in &patterns {
        super::owner_step(work, check)?;
        let (set, pattern) = match pattern.strip_prefix('!') {
            Some(rest) => (&mut exclude, rest),
            None => (&mut include, pattern.as_str()),
        };
        // Absolute or traversing patterns name no captured member.
        let Some(pattern) = member_pattern(pattern) else {
            continue;
        };
        let Ok(glob) = globset::GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()
        else {
            return Ok(Some(Members::Unreadable));
        };
        set.add(glob);
    }
    let (Ok(include), Ok(exclude)) = (include.build(), exclude.build()) else {
        return Ok(Some(Members::Unreadable));
    };
    let mut names: BTreeMap<String, Option<Member>> = BTreeMap::new();
    for manifest in manifests {
        super::owner_step(work, check)?;
        let Some(dir) = manifest.path.strip_suffix("/package.json") else {
            continue;
        };
        if dir.split('/').any(|part| part == "node_modules") {
            continue;
        }
        charge(work, check, dir.len().saturating_mul(patterns.len().max(1)))?;
        if !include.is_match(dir) || exclude.is_match(dir) {
            continue;
        }
        let bytes = manifest.bytes.as_deref().unwrap_or_default();
        charge(work, check, bytes.len())?;
        // A member manifest that cannot be read might claim any name.
        let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(bytes) else {
            return Ok(Some(Members::Unreadable));
        };
        let Some(name) = object.get("name").and_then(Value::as_str) else {
            // npm and pnpm need no name for a private member; nothing imports it by name.
            continue;
        };
        if !valid_name(name) {
            continue;
        }
        let member = Member {
            dir: dir.to_owned(),
            entry: entry(&object),
        };
        names
            .entry(name.to_owned())
            .and_modify(|existing| *existing = None)
            .or_insert(Some(member));
    }
    Ok(Some(Members::Names(names)))
}

fn charge(work: &mut Work, check: &mut impl FnMut() -> Result<()>, amount: usize) -> Result<()> {
    ensure!(
        work.charge(amount),
        "bounded_work: reference workspace parsing"
    );
    check()
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

/// `pattern` as a root-relative directory glob: `./` and a trailing `/` dropped; `None` when
/// it is absolute or traverses.
fn member_pattern(pattern: &str) -> Option<String> {
    let trimmed = pattern.trim_start_matches("./").trim_end_matches('/');
    let outside = trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.contains('\\')
        || trimmed
            .split('/')
            .any(|part| matches!(part, ".." | "." | ""));
    (!outside).then(|| trimmed.to_owned())
}

/// A member's entry: a single-string `exports`, else `module`, else `main`, else `index.*`.
fn entry(object: &serde_json::Map<String, Value>) -> Entry {
    let relative = |value: &str| -> Entry {
        let value = value.strip_prefix("./").unwrap_or(value);
        if value.starts_with('/')
            || value.contains('\\')
            || super::tsconfig::normalize(value).is_none()
        {
            Entry::Outside
        } else {
            Entry::Main(value.to_owned())
        }
    };
    match object.get("exports") {
        None | Some(Value::Null) => {}
        Some(Value::String(target)) => {
            return match relative(target) {
                Entry::Main(path) if target.starts_with("./") => Entry::Exports(path),
                // Node requires an exports target to start with `./`.
                _ => Entry::Unsupported,
            };
        }
        Some(_) => return Entry::Unsupported,
    }
    for field in ["module", "main"] {
        match object.get(field) {
            None | Some(Value::Null) => {}
            Some(Value::String(target)) if !target.is_empty() => return relative(target),
            Some(_) => return Entry::Unsupported,
        }
    }
    Entry::Index
}

/// The `packages` list of a `pnpm-workspace.yaml`, read with a bounded YAML subset: top-level
/// `key:` mappings, a `packages` block sequence (at or below the key's indent) or one-line
/// flow sequence of plain, single- or double-quoted scalars, and `#` comments. Other top-level
/// keys and their indented bodies are skipped. Anything else, including tabs, anchors,
/// aliases, tags, a second document or a duplicate `packages`, is unreadable (`Err`).
/// `Ok(None)` when there is no `packages` key.
pub(super) fn pnpm_packages(text: &str) -> std::result::Result<Option<Vec<String>>, ()> {
    #[derive(PartialEq)]
    enum State {
        Top,
        Skip,
        Packages,
    }
    let mut state = State::Top;
    let mut packages: Option<Vec<String>> = None;
    let mut started = false;
    for raw in text.lines() {
        let line = strip_comment(raw.strip_suffix('\r').unwrap_or(raw))?;
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        if line[indent..].starts_with('\t') || line[..indent].contains('\t') {
            return Err(());
        }
        let content = line[indent..].trim_end();
        if indent == 0 && (content == "---" || content == "...") {
            if started || content == "..." {
                return Err(());
            }
            started = true;
            continue;
        }
        started = true;
        let item = content
            .strip_prefix("- ")
            .or((content == "-").then_some(""));
        if state == State::Packages && item.is_some() {
            let value = scalar(item.unwrap_or_default().trim())?;
            let list = packages.as_mut().ok_or(())?;
            list.push(value);
            if list.len() > MAX_PATTERNS {
                return Err(());
            }
            continue;
        }
        if indent > 0 || item.is_some() {
            if state == State::Skip {
                continue;
            }
            return Err(());
        }
        let (key, value) = mapping(content)?;
        if key != "packages" {
            state = State::Skip;
            continue;
        }
        if packages.is_some() {
            return Err(());
        }
        if value.is_empty() {
            packages = Some(Vec::new());
            state = State::Packages;
        } else {
            packages = Some(flow(value)?);
            state = State::Top;
        }
    }
    Ok(packages)
}

/// `line` without a `#` comment that starts it or follows whitespace, outside quotes.
fn strip_comment(line: &str) -> std::result::Result<&str, ()> {
    let bytes = line.as_bytes();
    let mut quote = None;
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        match quote {
            Some(b'\'') if byte == b'\'' => {
                if bytes.get(at + 1) == Some(&b'\'') {
                    at += 1;
                } else {
                    quote = None;
                }
            }
            Some(b'"') if byte == b'\\' => at += 1,
            Some(b'"') if byte == b'"' => quote = None,
            Some(_) => {}
            None if byte == b'\'' || byte == b'"' => quote = Some(byte),
            None if byte == b'#' && (at == 0 || bytes[at - 1] == b' ') => {
                return Ok(&line[..at]);
            }
            None => {}
        }
        at += 1;
    }
    Ok(line)
}

/// A top-level `key: value` or `key:` line, with a plain or quoted key.
fn mapping(content: &str) -> std::result::Result<(String, &str), ()> {
    let (key, rest) = if content.starts_with(['\'', '"']) {
        let end = closing(content)?;
        (scalar(&content[..=end])?, &content[end + 1..])
    } else {
        let colon = content.find(':').ok_or(())?;
        let key = &content[..colon];
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
        {
            return Err(());
        }
        (key.to_owned(), &content[colon..])
    };
    let rest = rest.strip_prefix(':').ok_or(())?;
    if !rest.is_empty() && !rest.starts_with(' ') {
        return Err(());
    }
    Ok((key, rest.trim()))
}

/// The offset of the quote that closes the quoted scalar starting `text`.
fn closing(text: &str) -> std::result::Result<usize, ()> {
    let bytes = text.as_bytes();
    let quote = bytes[0];
    let mut at = 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' if quote == b'"' => at += 1,
            b'\'' if quote == b'\'' && bytes.get(at + 1) == Some(&b'\'') => at += 1,
            byte if byte == quote => return Ok(at),
            _ => {}
        }
        at += 1;
    }
    Err(())
}

/// One scalar: single-quoted (`''` escapes a quote), double-quoted (`\\` and `\"` only) or
/// plain. Plain scalars may not start with a YAML indicator or hold `: `.
fn scalar(text: &str) -> std::result::Result<String, ()> {
    match text.as_bytes().first() {
        None => Err(()),
        Some(b'\'') => {
            let end = closing(text)?;
            if end + 1 != text.len() {
                return Err(());
            }
            Ok(text[1..end].replace("''", "'"))
        }
        Some(b'"') => {
            let end = closing(text)?;
            if end + 1 != text.len() {
                return Err(());
            }
            let mut out = String::new();
            let mut chars = text[1..end].chars();
            while let Some(ch) = chars.next() {
                if ch == '\\' {
                    match chars.next() {
                        Some(escaped @ ('\\' | '"')) => out.push(escaped),
                        _ => return Err(()),
                    }
                } else {
                    out.push(ch);
                }
            }
            Ok(out)
        }
        Some(first)
            if b"&*!|>%@`[]{},?-:#".contains(first)
                || text.contains(": ")
                || text.ends_with(':') =>
        {
            Err(())
        }
        Some(_) => Ok(text.to_owned()),
    }
}

/// A one-line flow sequence `[a, 'b', "c"]`.
fn flow(value: &str) -> std::result::Result<Vec<String>, ()> {
    let inner = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .ok_or(())?;
    let mut items = Vec::new();
    let mut rest = inner.trim();
    while !rest.is_empty() {
        let end = if rest.starts_with(['\'', '"']) {
            closing(rest)? + 1
        } else {
            rest.find(',').unwrap_or(rest.len())
        };
        let item = rest[..end].trim();
        if item.contains(['[', ']', '{', '}']) {
            return Err(());
        }
        items.push(scalar(item)?);
        if items.len() > MAX_PATTERNS {
            return Err(());
        }
        rest = rest[end..].trim_start();
        match rest.strip_prefix(',') {
            Some(after) => {
                rest = after.trim_start();
                if rest.is_empty() {
                    break;
                }
            }
            None if rest.is_empty() => {}
            None => return Err(()),
        }
    }
    Ok(items)
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

    fn lookup(manifests: &[ManifestInput], specifier: &str) -> Option<Lookup> {
        let mut work = Work::new(1_000_000);
        let workspaces = Workspaces::new(manifests, &mut work, &mut || Ok(())).unwrap();
        workspaces
            .lookup("s", specifier, &mut work, &mut || Ok(()))
            .unwrap()
    }

    #[test]
    fn pnpm_subset_reads_block_flow_and_comments() {
        let text = "# root\npackages:\n  - 'apps/*'\n  - \"packages/**\" # all\n  - tools/x\n  - '!**/test/**'\ncatalog:\n  react: ^18\n";
        assert_eq!(
            pnpm_packages(text),
            Ok(Some(vec![
                "apps/*".into(),
                "packages/**".into(),
                "tools/x".into(),
                "!**/test/**".into()
            ]))
        );
        assert_eq!(
            pnpm_packages("packages:\n- a\n- 'b'\n"),
            Ok(Some(vec!["a".into(), "b".into()]))
        );
        assert_eq!(
            pnpm_packages("---\npackages: ['a', \"b\", c]\n"),
            Ok(Some(vec!["a".into(), "b".into(), "c".into()]))
        );
        assert_eq!(pnpm_packages("catalog:\n  x: 1\n"), Ok(None));
        assert_eq!(
            pnpm_packages("onlyBuiltDependencies:\n- esbuild\npackages:\n- a\n"),
            Ok(Some(vec!["a".into()]))
        );
        assert_eq!(pnpm_packages(""), Ok(None));
    }

    #[test]
    fn pnpm_subset_refuses_what_it_cannot_read() {
        for text in [
            "packages:\n\t- a\n",
            "packages: a\n",
            "packages:\n  - &x a\n",
            "packages:\n  - *x\n",
            "packages:\n  - !tag a\n",
            "packages:\n  - a\npackages:\n  - b\n",
            "packages:\n  - [a]\n",
            "packages:\n  - a: b\n",
            "packages:\n  - 'unterminated\n",
            "packages:\n  - \"bad \\n escape\"\n",
            "packages: [a, [b]]\n",
            "---\npackages:\n  - a\n---\n",
            "  indented: true\n",
            "no colon\n",
            "packages:\n    nested:\n      - a\n",
        ] {
            assert_eq!(pnpm_packages(text), Err(()), "{text:?}");
        }
        let many = format!("packages:\n{}", "  - a\n".repeat(MAX_PATTERNS + 1));
        assert_eq!(pnpm_packages(&many), Err(()));
    }

    #[test]
    fn workspace_members_resolve_by_name_and_subpath() {
        let manifests = [
            manifest(
                "package.json",
                r#"{"name":"root","workspaces":["packages/*"]}"#,
            ),
            manifest(
                "packages/ui/package.json",
                r#"{"name":"@acme/ui","main":"./src/index.js"}"#,
            ),
            manifest("packages/core/package.json", r#"{"name":"core"}"#),
            manifest(
                "packages/out/package.json",
                r#"{"name":"out","main":"../x.js"}"#,
            ),
            manifest("apps/web/package.json", r#"{"name":"web"}"#),
        ];
        assert_eq!(
            lookup(&manifests, "@acme/ui"),
            Some(Lookup::Path("packages/ui/src/index.js".into(), false))
        );
        assert_eq!(
            lookup(&manifests, "@acme/ui/src/button"),
            Some(Lookup::Path("packages/ui/src/button".into(), false))
        );
        assert_eq!(
            lookup(&manifests, "core"),
            Some(Lookup::Path("packages/core".into(), true))
        );
        assert_eq!(lookup(&manifests, "out"), Some(Lookup::Unknown(ENTRY)));
        assert_eq!(
            lookup(&manifests, "core/../../x"),
            Some(Lookup::Unknown(ENTRY))
        );
        // Outside the patterns, and not a member.
        assert_eq!(lookup(&manifests, "web"), None);
        assert_eq!(lookup(&manifests, "lodash"), None);
    }

    #[test]
    fn duplicate_names_exports_and_unreadable_configs_are_typed() {
        let manifests = [
            manifest(
                "package.json",
                r#"{"workspaces":{"packages":["a","b","c","d","!d"]}}"#,
            ),
            manifest("a/package.json", r#"{"name":"dup"}"#),
            manifest("b/package.json", r#"{"name":"dup"}"#),
            manifest(
                "c/package.json",
                r#"{"name":"mapped","exports":{".":"./i.js"}}"#,
            ),
            manifest("d/package.json", r#"{"name":"excluded"}"#),
        ];
        assert_eq!(lookup(&manifests, "dup"), Some(Lookup::Unknown(AMBIGUOUS)));
        assert_eq!(lookup(&manifests, "mapped"), Some(Lookup::Unknown(EXPORTS)));
        assert_eq!(lookup(&manifests, "excluded"), None);
        let single = [
            manifest("package.json", r#"{"workspaces":["s"]}"#),
            manifest(
                "s/package.json",
                r#"{"name":"single","exports":"./lib/i.js"}"#,
            ),
        ];
        assert_eq!(
            lookup(&single, "single"),
            Some(Lookup::Path("s/lib/i.js".into(), false))
        );
        assert_eq!(
            lookup(&single, "single/lib/i.js"),
            Some(Lookup::Unknown(EXPORTS))
        );
        for root in [
            r#"{"workspaces":"packages/*"}"#,
            r#"{"workspaces":[1]}"#,
            r#"{"workspaces":{"nohoist":[]}}"#,
            r#"{"workspaces":["[unclosed"]}"#,
        ] {
            let manifests = [manifest("package.json", root)];
            assert_eq!(
                lookup(&manifests, "anything"),
                Some(Lookup::Unknown(UNREADABLE)),
                "{root}"
            );
        }
        let broken_member = [
            manifest("package.json", r#"{"workspaces":["m"]}"#),
            manifest("m/package.json", "{not json"),
        ];
        assert_eq!(
            lookup(&broken_member, "x"),
            Some(Lookup::Unknown(UNREADABLE))
        );
        let pnpm = [
            manifest(PNPM_WORKSPACE, "packages:\n  - 'libs/*'\n"),
            manifest("libs/kit/package.json", r#"{"name":"kit"}"#),
        ];
        assert_eq!(
            lookup(&pnpm, "kit"),
            Some(Lookup::Path("libs/kit".into(), true))
        );
        let bad_pnpm = [manifest(PNPM_WORKSPACE, "packages: [\n")];
        assert_eq!(lookup(&bad_pnpm, "kit"), Some(Lookup::Unknown(UNREADABLE)));
        // No workspace declared at all.
        assert_eq!(lookup(&[manifest("package.json", "{}")], "kit"), None);
    }

    #[test]
    fn workspace_parsing_is_charged_and_cancellable() {
        let manifests = [
            manifest("package.json", r#"{"workspaces":["p/*"]}"#),
            manifest("p/a/package.json", r#"{"name":"a"}"#),
        ];
        let mut work = Work::new(4);
        assert!(Workspaces::new(&manifests, &mut work, &mut || Ok(())).is_err());
        let mut work = Work::new(1_000_000);
        assert!(
            Workspaces::new(&manifests, &mut work, &mut || anyhow::bail!("cancelled")).is_err()
        );
    }
}
