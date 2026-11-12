//! Bounded, read-only architecture rules over one admitted source snapshot.
use crate::native_evidence::{Declaration, DeclarationRole, Range};
use crate::references::parallel_check::RequestCheck;
use crate::references::work::{self, Work};
use crate::references::{Projection, Snapshot};
use anyhow::{Context, Result, ensure};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use toml::Spanned;

mod glob_language;

pub const RULES_PATH: &str = ".trellis/rules.toml";
pub(crate) const CONFIG_BYTES: usize = 2 * 1024 * 1024;
const COLLECTION_LIMIT: usize = 1024;
const PATTERN_LIMIT: usize = 8192;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleError {
    pub code: String,
    pub kind: String,
    pub path: String,
    pub range: Range,
    pub line: usize,
    pub column: usize,
    pub attribution: String,
    pub message: String,
}
impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}:{}:{}: {}",
            self.code, self.path, self.line, self.column, self.message
        )
    }
}
impl std::error::Error for RuleError {}
pub fn diagnostic(error: &anyhow::Error) -> Option<Value> {
    error.downcast_ref::<RuleError>().map(|e| json!(e))
}
fn error(
    text: &str,
    span: std::ops::Range<usize>,
    kind: &str,
    attribution: &str,
    message: impl Into<String>,
) -> anyhow::Error {
    let mut start = span.start.min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = span.end.max(start).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = &text[..start];
    RuleError {
        code: "invalid_rules".into(),
        kind: kind.into(),
        path: RULES_PATH.into(),
        range: Range { start, end },
        line: prefix.bytes().filter(|b| *b == b'\n').count() + 1,
        column: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
        attribution: attribution.into(),
        message: message.into(),
    }
    .into()
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Raw {
    version: Option<Spanned<i64>>,
    #[serde(default)]
    layer_policy: Policy,
    #[serde(default)]
    external_policy: ExternalPolicy,
    #[serde(default)]
    layers: BTreeMap<String, Spanned<RawLayer>>,
    #[serde(default)]
    roles: BTreeMap<String, Spanned<RawRole>>,
    #[serde(default)]
    rules: Vec<Spanned<RawRule>>,
    #[serde(default)]
    exceptions: Vec<Spanned<RawException>>,
    /// Authored apps (#794).
    #[serde(default)]
    apps: BTreeMap<String, Spanned<RawApp>>,
    /// HTTP interface settings (#855).
    #[serde(default)]
    interfaces: RawInterfaces,
}
/// The `[interfaces]` table (#855). `wrappers` is its only key.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawInterfaces {
    #[serde(default)]
    wrappers: Vec<Spanned<RawWrapper>>,
}
/// One `[[interfaces.wrappers]]` entry (#855).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWrapper {
    callee: Option<Spanned<String>>,
    method: Option<Spanned<String>>,
    at: Option<Spanned<i64>>,
    prefix: Option<Spanned<String>>,
    root: Option<Spanned<String>>,
    app: Option<Spanned<String>>,
}
/// An authored `[apps.<name>]` table (#794). `roots` is its only key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApp {
    roots: Option<Spanned<Vec<Spanned<String>>>>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Policy {
    peers: Option<Spanned<String>>,
    shared_outbound: Option<Spanned<String>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLayer {
    order: Option<i64>,
    shared: Option<bool>,
    /// Browser presentation hint only (#391); never affects evaluation.
    display: Option<Spanned<String>>,
    /// The app an ordered layer belongs to (#784).
    app: Option<Spanned<String>>,
    /// The apps a shared layer serves (#784).
    apps: Option<Spanned<Vec<Spanned<String>>>>,
    #[serde(default)]
    paths: Vec<Spanned<String>>,
    #[serde(default)]
    external: Vec<Spanned<ExternalSelector>>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ExternalPolicy {
    #[serde(default)]
    deny: Vec<Spanned<ExternalSelector>>,
}
#[derive(Clone, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct ExternalSelector {
    language: String,
    package: String,
    stdlib: bool,
}
impl ExternalSelector {
    fn from_identity(identity: &Value) -> Option<Self> {
        (identity["kind"] == "externalPackage").then_some(())?;
        Some(Self {
            language: identity["language"].as_str()?.into(),
            package: identity["package"].as_str()?.into(),
            stdlib: identity["standardLibrary"].as_bool()?,
        })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRole {
    select: Spanned<RawSelector>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawSelector {
    paths: Option<Vec<Spanned<String>>>,
    name: Option<Spanned<String>>,
    language: Option<Vec<Spanned<String>>>,
    extends: Option<Spanned<String>>,
    implements: Option<Spanned<String>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    id: Spanned<String>,
    for_each: Spanned<String>,
    #[serde(default)]
    require: Vec<Spanned<BTreeMap<String, String>>>,
    #[serde(default)]
    forbid: Vec<Spanned<BTreeMap<String, String>>>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct ConfigTarget {
    source_set: String,
    language: String,
    path: String,
    sid: String,
}
impl ConfigTarget {
    fn wire(&self) -> Value {
        json!({"sourceSetId":self.source_set,"language":self.language,"path":self.path,"sid":self.sid})
    }
}
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct ModulePathTarget {
    language: String,
    path: String,
}
impl ModulePathTarget {
    fn valid(&self) -> bool {
        languages(&self.language)
            && !self.path.is_empty()
            && crate::file_tree::valid_path(&self.path)
            && !self.path.contains(['*', '?'])
    }
    fn matches(&self, identity: &Value, source_set: &str) -> bool {
        identity["sourceSetId"] == source_set
            && identity["language"] == self.language
            && identity["path"] == self.path
    }
}
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(untagged)]
enum ExceptionTarget {
    Native(ConfigTarget),
    ModulePath(ModulePathTarget),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawException {
    rule: Spanned<String>,
    target: Spanned<ExceptionTarget>,
    destination: Option<Spanned<ExceptionTarget>>,
    reason: Spanned<String>,
}
struct LayerPattern {
    text: String,
    matcher: GlobSet,
}
struct Layer {
    order: Option<i64>,
    display: Option<String>,
    app: Option<(String, std::ops::Range<usize>)>,
    apps: Option<Vec<(String, std::ops::Range<usize>)>>,
    patterns: Vec<LayerPattern>,
    external: BTreeSet<ExternalSelector>,
    span: std::ops::Range<usize>,
}
struct Selector {
    paths: Vec<GlobSet>,
    name: Option<GlobSet>,
    language: Vec<String>,
    extends: Option<String>,
    implements: Option<String>,
}
struct Constraint {
    mode: &'static str,
    kind: String,
    role: String,
}
struct Rule {
    id: String,
    role: String,
    constraints: Vec<Constraint>,
}
enum ExceptionKind {
    Declaration(Value),
    ModulePair {
        source: ModulePathTarget,
        destination: ModulePathTarget,
        span: std::ops::Range<usize>,
    },
}
struct Exception {
    rule: String,
    kind: ExceptionKind,
    reason: String,
}
/// An authored app (#794), located for errors.
struct AuthoredApp {
    name: String,
    span: std::ops::Range<usize>,
    roots: Vec<(String, std::ops::Range<usize>)>,
}
pub struct Config {
    pub text: String,
    pub content_hash: String,
    layers: BTreeMap<String, Layer>,
    roles: BTreeMap<String, Selector>,
    rules: Vec<Rule>,
    exceptions: Vec<Exception>,
    peers: Option<String>,
    shared_outbound: Option<String>,
    external_deny: BTreeSet<ExternalSelector>,
    /// Authored `[apps.<name>]` tables in name order (#794).
    apps: Vec<AuthoredApp>,
    /// Authored `[[interfaces.wrappers]]` entries in written order (#855).
    wrappers: Vec<InterfaceWrapper>,
}

/// Where a configured request wrapper reads its method (#855).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WrapperMethod {
    /// `method` in the options object at this argument.
    Options(usize),
    /// A string at this argument.
    Argument(usize),
}

/// An authored `[[interfaces.wrappers]]` entry (#855): a JavaScript or TypeScript callee whose
/// argument 0 is a request path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InterfaceWrapper {
    /// An identifier, or `object.property`.
    pub(crate) callee: String,
    pub(crate) method: WrapperMethod,
    /// A rooted literal the wrapper adds to every path.
    pub(crate) prefix: Option<String>,
    /// The checkout-relative directory whose files the wrapper applies to; `.` is every file.
    /// It is `.` when `app` is set.
    pub(crate) root: String,
    /// The app whose files the wrapper applies to, and where it is written. It names an app of
    /// the effective inventory, which rule preparation checks.
    pub(crate) app: Option<(String, std::ops::Range<usize>)>,
}

impl Config {
    /// Authored `[[exceptions]]` entries, as the rules inventory lists them.
    pub fn exception_count(&self) -> usize {
        self.exceptions.len()
    }

    /// The authored request wrappers, in written order (#855).
    pub(crate) fn interface_wrappers(&self) -> &[InterfaceWrapper] {
        &self.wrappers
    }

    /// How many `[apps.<name>]` tables the file authors (#794).
    pub(crate) fn authored_app_count(&self) -> usize {
        self.apps.len()
    }

    /// The authored apps in name order, as inventory entries.
    fn authored_apps(&self) -> Vec<crate::apps::App> {
        self.apps
            .iter()
            .map(|app| {
                crate::apps::App::authored(
                    app.name.clone(),
                    app.roots.iter().map(|(root, _)| root.clone()).collect(),
                )
            })
            .collect()
    }
}

pub fn load(root: &Path) -> Result<Option<Config>> {
    let dir = crate::file_tree::SourceDir::open(root)?;
    match dir.read_file(RULES_PATH) {
        Ok(text) => parse(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(error(
            "",
            0..0,
            if e.kind() == std::io::ErrorKind::FileTooLarge {
                "too_large"
            } else {
                "read"
            },
            "file",
            e.to_string(),
        )),
    }
}
pub fn descriptor(config: &Config) -> Value {
    json!({"path":RULES_PATH,"source":"checkout-config","contentHash":config.content_hash})
}
fn pattern(text: &str, value: &Spanned<String>, path: bool, budget: &mut usize) -> Result<GlobSet> {
    *budget += 1;
    if *budget > PATTERN_LIMIT {
        return Err(error(
            text,
            value.span(),
            "too_large",
            "token",
            "aggregate glob pattern limit exceeded",
        ));
    }
    let p = value.get_ref();
    let invalid = p.is_empty()
        || p.contains(['\0', '\\', '[', ']', '{', '}', '!'])
        || (path
            && (p.starts_with('/')
                || p.contains(':')
                || p.split('/').any(|c| c.is_empty() || c == "." || c == "..")))
        || p.split('/').any(|c| c.contains("**") && c != "**");
    if invalid {
        return Err(error(
            text,
            value.span(),
            "glob",
            "token",
            "unsupported or invalid glob",
        ));
    }
    let glob = GlobBuilder::new(p)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map_err(|e| error(text, value.span(), "glob", "token", e.to_string()))?;
    let mut set = GlobSetBuilder::new();
    set.add(glob);
    set.build()
        .map_err(|e| error(text, value.span(), "glob", "token", e.to_string()))
}
fn app_name(text: &str, value: Spanned<String>) -> Result<(String, std::ops::Range<usize>)> {
    let span = value.span();
    let name = value.into_inner();
    if name.trim().is_empty() {
        return Err(error(
            text,
            span,
            "schema",
            "token",
            "app name must not be blank",
        ));
    }
    Ok((name, span))
}
fn app_names(
    text: &str,
    values: Spanned<Vec<Spanned<String>>>,
) -> Result<Vec<(String, std::ops::Range<usize>)>> {
    let list_span = values.span();
    let values = values.into_inner();
    if values.is_empty() {
        return Err(error(
            text,
            list_span,
            "schema",
            "token",
            "apps must name at least one app",
        ));
    }
    if values.len() > COLLECTION_LIMIT {
        return Err(error(
            text,
            values[COLLECTION_LIMIT].span(),
            "too_large",
            "token",
            "apps collection exceeds 1024 entries",
        ));
    }
    let mut names = Vec::with_capacity(values.len());
    for value in values {
        let (name, span) = app_name(text, value)?;
        if names.iter().any(|(seen, _)| *seen == name) {
            return Err(error(
                text,
                span,
                "schema",
                "token",
                format!("app {name} is listed twice"),
            ));
        }
        names.push((name, span));
    }
    Ok(names)
}
fn external_selectors(
    text: &str,
    selectors: &[Spanned<ExternalSelector>],
    budget: &mut usize,
) -> Result<BTreeSet<ExternalSelector>> {
    if selectors.len() > COLLECTION_LIMIT {
        return Err(error(
            text,
            selectors[COLLECTION_LIMIT].span(),
            "too_large",
            "token",
            "external selector collection exceeds 1024 entries",
        ));
    }
    let mut selected = BTreeSet::new();
    for selector in selectors {
        *budget += 1;
        if *budget > PATTERN_LIMIT {
            return Err(error(
                text,
                selector.span(),
                "too_large",
                "token",
                "aggregate selector and glob pattern limit exceeded",
            ));
        }
        let value = selector.get_ref();
        if !crate::references::external::valid_package(
            &value.language,
            &value.package,
            value.stdlib,
        ) {
            return Err(error(
                text,
                selector.span(),
                "selector",
                "token",
                "invalid canonical external package identity",
            ));
        }
        selected.insert(value.clone());
    }
    Ok(selected)
}
fn target_valid(t: &ConfigTarget) -> bool {
    !t.source_set.is_empty()
        && languages(&t.language)
        && !t.path.is_empty()
        && crate::file_tree::valid_path(&t.path)
        && t.sid.len() == 39
        && t.sid.starts_with("sid:v1:")
        && t.sid[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn languages(s: &str) -> bool {
    matches!(s, "java" | "rust" | "python" | "javascript" | "typescript")
}
pub fn parse(text: &str) -> Result<Config> {
    if text.len() > CONFIG_BYTES {
        return Err(error(
            text,
            CONFIG_BYTES..CONFIG_BYTES,
            "too_large",
            "boundary",
            "rules file exceeds 2 MiB",
        ));
    }
    let raw: Raw = toml::from_str(text).map_err(|e: toml::de::Error| {
        let depth = e.message().contains("recurs") || e.message().contains("depth");
        let span = e.span().unwrap_or(0..0);
        let attribution = if depth { "file" } else { "token" };
        error(
            text,
            span,
            if depth { "too_deep" } else { "schema" },
            attribution,
            e.message(),
        )
    })?;
    if raw.version.as_ref().map(Spanned::get_ref) != Some(&1) {
        return Err(error(
            text,
            raw.version.as_ref().map(Spanned::span).unwrap_or(0..0),
            "schema",
            "file",
            "version must be 1",
        ));
    }
    if [
        raw.layers.len(),
        raw.roles.len(),
        raw.rules.len(),
        raw.exceptions.len(),
        raw.apps.len(),
    ]
    .into_iter()
    .any(|n| n > COLLECTION_LIMIT)
    {
        return Err(error(
            text,
            0..0,
            "too_large",
            "file",
            "named collection exceeds 1024 entries",
        ));
    }
    let policy = |v: Option<Spanned<String>>| -> Result<Option<String>> {
        v.map(|v| {
            if matches!(v.get_ref().as_str(), "allow" | "forbid") {
                Ok(v.into_inner())
            } else {
                Err(error(
                    text,
                    v.span(),
                    "schema",
                    "token",
                    "policy must be allow or forbid",
                ))
            }
        })
        .transpose()
    };
    let peers = policy(raw.layer_policy.peers)?;
    let shared_outbound = policy(raw.layer_policy.shared_outbound)?;
    let mut patterns = 0;
    let external_deny = external_selectors(text, &raw.external_policy.deny, &mut patterns)?;
    let mut layers = BTreeMap::new();
    let mut external_owners = BTreeMap::new();
    for (name, raw) in raw.layers {
        let span = raw.span();
        let layer = raw.into_inner();
        if name.trim().is_empty()
            || (layer.paths.is_empty() && layer.external.is_empty())
            || !matches!(
                (layer.order, layer.shared),
                (Some(_), None) | (None, Some(true))
            )
        {
            return Err(error(
                text,
                span,
                "schema",
                "token",
                "layer requires nonempty paths or external and exactly one integer order or shared=true",
            ));
        }
        let display = layer
            .display
            .map(|v| {
                if matches!(v.get_ref().as_str(), "expanded" | "collapsed" | "hidden") {
                    Ok(v.into_inner())
                } else {
                    Err(error(
                        text,
                        v.span(),
                        "schema",
                        "token",
                        "layer display must be expanded, collapsed or hidden",
                    ))
                }
            })
            .transpose()?;
        let app = layer.app.map(|v| app_name(text, v)).transpose()?;
        if let (Some((_, span)), None) = (&app, layer.order) {
            return Err(error(
                text,
                span.clone(),
                "schema",
                "token",
                "app belongs on an ordered layer; a shared layer lists apps",
            ));
        }
        let apps = layer.apps.map(|v| app_names(text, v)).transpose()?;
        if let (Some(apps), Some(_)) = (&apps, layer.order) {
            return Err(error(
                text,
                apps.first().map_or(span.clone(), |(_, s)| s.clone()),
                "schema",
                "token",
                "apps belongs on a shared layer; an ordered layer names one app",
            ));
        }
        let mut globs = layer
            .paths
            .iter()
            .map(|v| {
                Ok(LayerPattern {
                    text: v.get_ref().clone(),
                    matcher: pattern(text, v, true, &mut patterns)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        globs.sort_by(|a, b| a.text.cmp(&b.text));
        globs.dedup_by(|a, b| a.text == b.text);
        let external = external_selectors(text, &layer.external, &mut patterns)?;
        for selector in &external {
            if let Some(previous) = external_owners.insert(selector.clone(), name.clone()) {
                return Err(error(
                    text,
                    span.clone(),
                    "ambiguity",
                    "token",
                    format!(
                        "external package {} matches layers {previous} and {name}",
                        selector.package
                    ),
                ));
            }
        }
        layers.insert(
            name,
            Layer {
                order: layer.order,
                display,
                app,
                apps,
                patterns: globs,
                external,
                span,
            },
        );
    }
    let apps = authored_apps(text, raw.apps)?;
    let wrappers = interface_wrappers(text, raw.interfaces)?;
    let mut roles = BTreeMap::new();
    for (name, raw) in raw.roles {
        let selector = raw.get_ref();
        let s = selector.select.get_ref();
        if name.trim().is_empty()
            || (s.paths.is_none()
                && s.name.is_none()
                && s.language.is_none()
                && s.extends.is_none()
                && s.implements.is_none())
        {
            return Err(error(
                text,
                selector.select.span(),
                "selector",
                "token",
                "selector requires at least one field",
            ));
        }
        if s.paths.as_ref().is_some_and(Vec::is_empty)
            || s.language.as_ref().is_some_and(Vec::is_empty)
        {
            return Err(error(
                text,
                selector.select.span(),
                "selector",
                "token",
                "selector list must not be empty",
            ));
        }
        for language in s.language.iter().flatten() {
            if !languages(language.get_ref()) {
                return Err(error(
                    text,
                    language.span(),
                    "selector",
                    "token",
                    "unsupported language",
                ));
            }
        }
        for base in [s.extends.as_ref(), s.implements.as_ref()]
            .into_iter()
            .flatten()
        {
            if base.get_ref().trim().is_empty() {
                return Err(error(
                    text,
                    base.span(),
                    "selector",
                    "token",
                    "base spelling must not be blank",
                ));
            }
        }
        roles.insert(
            name,
            Selector {
                paths: s
                    .paths
                    .iter()
                    .flatten()
                    .map(|p| pattern(text, p, true, &mut patterns))
                    .collect::<Result<_>>()?,
                name: s
                    .name
                    .as_ref()
                    .map(|p| pattern(text, p, false, &mut patterns))
                    .transpose()?,
                language: s
                    .language
                    .iter()
                    .flatten()
                    .map(|l| l.get_ref().clone())
                    .collect(),
                extends: s.extends.as_ref().map(|s| s.get_ref().clone()),
                implements: s.implements.as_ref().map(|s| s.get_ref().clone()),
            },
        );
    }
    let mut rules = Vec::new();
    let mut ids = BTreeSet::new();
    for raw in raw.rules {
        let r = raw.get_ref();
        let id = r.id.get_ref();
        if id.trim().is_empty()
            || matches!(id.as_str(), "$layers" | "$external")
            || !ids.insert(id.clone())
        {
            return Err(error(
                text,
                r.id.span(),
                "reference",
                "token",
                "duplicate, reserved or blank rule id",
            ));
        }
        if !roles.contains_key(r.for_each.get_ref()) {
            return Err(error(
                text,
                r.for_each.span(),
                "reference",
                "token",
                "unknown for_each role",
            ));
        }
        if r.require.is_empty() && r.forbid.is_empty() {
            return Err(error(
                text,
                raw.span(),
                "constraint",
                "token",
                "rule requires a constraint",
            ));
        }
        let mut constraints = Vec::new();
        for (mode, items) in [("require", &r.require), ("forbid", &r.forbid)] {
            for value in items {
                let c = value.get_ref();
                if c.len() != 1 {
                    return Err(error(
                        text,
                        value.span(),
                        "constraint",
                        "token",
                        "constraint must contain exactly one edge kind",
                    ));
                }
                let (kind, role) = c.iter().next().expect("one checked constraint");
                if !matches!(kind.as_str(), "calls" | "extends" | "implements") {
                    return Err(error(
                        text,
                        value.span(),
                        "constraint",
                        "token",
                        "unknown edge constraint",
                    ));
                }
                if !roles.contains_key(role) {
                    return Err(error(
                        text,
                        value.span(),
                        "reference",
                        "token",
                        "unknown constraint role",
                    ));
                }
                constraints.push(Constraint {
                    mode,
                    kind: kind.clone(),
                    role: role.clone(),
                });
            }
        }
        rules.push(Rule {
            id: id.clone(),
            role: r.for_each.get_ref().clone(),
            constraints,
        });
    }
    let mut exceptions = Vec::new();
    let mut exception_keys = BTreeSet::new();
    for raw in raw.exceptions {
        let e = raw.get_ref();
        if !(ids.contains(e.rule.get_ref()) || e.rule.get_ref() == "$layers") {
            return Err(error(
                text,
                e.rule.span(),
                "reference",
                "token",
                "unknown exception rule",
            ));
        }
        let kind = match (
            e.rule.get_ref().as_str(),
            e.target.get_ref(),
            e.destination.as_ref().map(Spanned::get_ref),
        ) {
            (
                "$layers",
                ExceptionTarget::ModulePath(source),
                Some(ExceptionTarget::ModulePath(destination)),
            ) if source.valid() && destination.valid() => ExceptionKind::ModulePair {
                source: source.clone(),
                destination: destination.clone(),
                span: raw.span(),
            },
            (rule, ExceptionTarget::Native(target), None)
                if rule != "$layers" && target_valid(target) =>
            {
                ExceptionKind::Declaration(target.wire())
            }
            _ => {
                return Err(error(
                    text,
                    raw.span(),
                    "reference",
                    "token",
                    "layer exceptions require two literal language/path endpoints; declaration exceptions require one native identity",
                ));
            }
        };
        if e.reason.get_ref().trim().is_empty() {
            return Err(error(
                text,
                e.reason.span(),
                "schema",
                "token",
                "exception reason must not be blank",
            ));
        }
        let key = (
            e.rule.get_ref().clone(),
            e.target.get_ref().clone(),
            e.destination.as_ref().map(|d| d.get_ref().clone()),
        );
        if !exception_keys.insert(key) {
            return Err(error(
                text,
                raw.span(),
                "reference",
                "token",
                "duplicate exception",
            ));
        }
        exceptions.push(Exception {
            rule: e.rule.get_ref().clone(),
            kind,
            reason: e.reason.get_ref().clone(),
        });
    }
    Ok(Config {
        text: text.into(),
        content_hash: hex::encode(Sha256::digest(text.as_bytes())),
        layers,
        roles,
        rules,
        exceptions,
        peers,
        shared_outbound,
        external_deny,
        apps,
        wrappers,
    })
}

/// The most wrappers `[interfaces]` may author.
const WRAPPER_LIMIT: usize = 64;

/// The authored `[[interfaces.wrappers]]` entries (#855). Each has a `callee` that is an
/// identifier or `object.property`; `method` is `"options"` (the default) or `"argument"`;
/// `at` is 1 to 8 (default 1); `prefix` is a rooted literal with no query, fragment or
/// pattern; `root` is a valid app root (default `.`). A callee appears once per root.
fn interface_wrappers(text: &str, raw: RawInterfaces) -> Result<Vec<InterfaceWrapper>> {
    if raw.wrappers.len() > WRAPPER_LIMIT {
        return Err(error(
            text,
            raw.wrappers[WRAPPER_LIMIT].span(),
            "too_large",
            "token",
            "interfaces.wrappers exceeds 64 entries",
        ));
    }
    let identifier = |part: &str| {
        let mut chars = part.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    };
    let mut wrappers: Vec<InterfaceWrapper> = Vec::with_capacity(raw.wrappers.len());
    for entry in raw.wrappers {
        let span = entry.span();
        let entry = entry.into_inner();
        let invalid = |span: std::ops::Range<usize>, message: String| {
            error(text, span, "schema", "token", message)
        };
        let Some(callee) = entry.callee else {
            return Err(invalid(
                span,
                "an interfaces wrapper requires callee".into(),
            ));
        };
        let callee_span = callee.span();
        let callee = callee.into_inner();
        let parts: Vec<&str> = callee.split('.').collect();
        if parts.len() > 2 || !parts.iter().all(|part| identifier(part)) {
            return Err(invalid(
                callee_span,
                format!("wrapper callee {callee:?} must be an identifier or object.property"),
            ));
        }
        let at = match entry.at {
            None => 1,
            Some(at) => {
                let at_span = at.span();
                match usize::try_from(*at.get_ref()) {
                    Ok(at @ 1..=8) => at,
                    _ => {
                        return Err(invalid(at_span, "wrapper at must be 1 to 8".into()));
                    }
                }
            }
        };
        let method = match entry
            .method
            .as_ref()
            .map(|m| (m.get_ref().as_str(), m.span()))
        {
            None | Some(("options", _)) => WrapperMethod::Options(at),
            Some(("argument", _)) => WrapperMethod::Argument(at),
            Some((other, method_span)) => {
                return Err(invalid(
                    method_span,
                    format!("wrapper method {other:?} must be \"options\" or \"argument\""),
                ));
            }
        };
        let prefix = match entry.prefix {
            None => None,
            Some(prefix) => {
                let prefix_span = prefix.span();
                let prefix = prefix.into_inner();
                if !prefix.starts_with('/') || prefix.contains(['?', '#', '{', '}', '*']) {
                    return Err(invalid(
                        prefix_span,
                        format!(
                            "wrapper prefix {prefix:?} must start with / and have no query, fragment or pattern"
                        ),
                    ));
                }
                Some(prefix)
            }
        };
        if let (Some(_), Some(app)) = (&entry.root, &entry.app) {
            return Err(invalid(
                app.span(),
                "a wrapper names either root or app, not both".into(),
            ));
        }
        let app = match entry.app {
            None => None,
            Some(app) => {
                let app_span = app.span();
                let app = app.into_inner();
                if app.trim().is_empty() {
                    return Err(invalid(app_span, "wrapper app must not be blank".into()));
                }
                Some((app, app_span))
            }
        };
        let root = match entry.root {
            None => ".".to_owned(),
            Some(root) => {
                let root_span = root.span();
                let root = root.into_inner();
                if !valid_root(&root) {
                    return Err(invalid(
                        root_span,
                        format!(
                            "wrapper root {root:?} must be \".\" or a checkout-relative directory: no leading or trailing slash, no \".\" or \"..\" component, no glob, backslash or colon"
                        ),
                    ));
                }
                root
            }
        };
        let scope_name = |w: &InterfaceWrapper| match &w.app {
            Some((app, _)) => format!("app {app}"),
            None => format!("root {}", w.root),
        };
        let wrapper = InterfaceWrapper {
            callee,
            method,
            prefix,
            root,
            app,
        };
        if wrappers
            .iter()
            .any(|w| w.callee == wrapper.callee && scope_name(w) == scope_name(&wrapper))
        {
            return Err(error(
                text,
                callee_span,
                "ambiguity",
                "token",
                format!(
                    "wrapper {} is authored twice for {}",
                    wrapper.callee,
                    scope_name(&wrapper)
                ),
            ));
        }
        wrappers.push(wrapper);
    }
    Ok(wrappers)
}

/// Whether an authored root is `.` or a literal checkout-relative directory: no leading or
/// trailing slash, no empty, `.` or `..` component, and no glob, backslash, colon or NUL.
fn valid_root(root: &str) -> bool {
    root == "."
        || (!root.is_empty()
            && crate::file_tree::valid_path(root)
            && !root.contains(['*', '?', '[', ']', '{', '}', '!']))
}

/// The authored `[apps.<name>]` tables (#794), in name order. Checks that need no snapshot:
/// each table has a nonblank name and a nonempty list of valid, distinct roots, and no root is
/// authored by two apps.
fn authored_apps(text: &str, raw: BTreeMap<String, Spanned<RawApp>>) -> Result<Vec<AuthoredApp>> {
    let mut apps = Vec::with_capacity(raw.len());
    let mut owners: BTreeMap<String, String> = BTreeMap::new();
    for (name, table) in raw {
        let span = table.span();
        if name.trim().is_empty() {
            return Err(error(
                text,
                span,
                "schema",
                "token",
                "app name must not be blank",
            ));
        }
        let Some(list) = table.into_inner().roots else {
            return Err(error(
                text,
                span,
                "schema",
                "token",
                format!("app {name} requires roots"),
            ));
        };
        let list_span = list.span();
        let list = list.into_inner();
        if list.is_empty() {
            return Err(error(
                text,
                list_span,
                "schema",
                "token",
                format!("app {name} must list at least one root"),
            ));
        }
        if list.len() > COLLECTION_LIMIT {
            return Err(error(
                text,
                list[COLLECTION_LIMIT].span(),
                "too_large",
                "token",
                "app roots collection exceeds 1024 entries",
            ));
        }
        let mut roots: Vec<(String, std::ops::Range<usize>)> = Vec::with_capacity(list.len());
        for value in list {
            let root_span = value.span();
            let root = value.into_inner();
            if !valid_root(&root) {
                return Err(error(
                    text,
                    root_span,
                    "schema",
                    "token",
                    format!(
                        "app root {root:?} must be \".\" or a checkout-relative directory: no leading or trailing slash, no \".\" or \"..\" component, no glob, backslash or colon"
                    ),
                ));
            }
            if roots.iter().any(|(seen, _)| *seen == root) {
                return Err(error(
                    text,
                    root_span,
                    "schema",
                    "token",
                    format!("app {name} lists root {root} twice"),
                ));
            }
            if let Some(owner) = owners.get(&root) {
                return Err(error(
                    text,
                    root_span,
                    "ambiguity",
                    "token",
                    format!("root {root} is authored by apps {owner} and {name}"),
                ));
            }
            owners.insert(root.clone(), name.clone());
            roots.push((root, root_span));
        }
        apps.push(AuthoredApp { name, span, roots });
    }
    Ok(apps)
}

fn identity(d: &Declaration) -> Value {
    json!({"sourceSetId":d.document.source_set_id,"language":d.document.language,"path":d.document.path,"sid":d.syntax_id})
}
fn native_evidence(d: &Declaration) -> Value {
    json!({"document":d.document,"revisionId":d.revision_id,"range":d.range,"kind":"declaration","syntaxId":d.syntax_id,"callId":null,"provenanceId":d.provenance_id})
}
fn glob_matches(patterns: &[GlobSet], path: &str) -> bool {
    patterns.is_empty() || patterns.iter().any(|p| p.is_match(path))
}
/// Browser-only authored inventory; does not change the closed rules payload.
pub fn view_layers(config: &Config) -> Vec<Value> {
    config.layers.iter().map(|(name, layer)| {
        json!({"name": name, "order": layer.order, "shared": layer.order.is_none(), "display": layer.display,
            "app": layer.app.as_ref().map(|(app, _)| app),
            "apps": layer.apps.as_ref().map(|apps| apps.iter().map(|(app, _)| app).collect::<Vec<_>>())})
    }).collect()
}

/// The outcome of every build manifest in `snapshot`, under its own work budget. The budget
/// is sized like rules preparation's, from the whole admitted snapshot, so a manifest that
/// rules preparation can parse is parsed here too.
pub(crate) fn app_candidates(
    snapshot: &Snapshot,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<crate::apps::Candidates> {
    let documents: BTreeSet<&str> = snapshot
        .documents
        .iter()
        .map(|document| document.key.path.as_str())
        .collect();
    let mut work = Work::new(work::rules_limit(
        snapshot.documents.len(),
        work::snapshot_bytes(snapshot, check)?,
    ));
    crate::apps::candidates(&snapshot.manifests, &documents, &mut work, check)
}

/// The prompt's `payload.apps` (#783, #794): the effective apps against `config`'s bytes, so a
/// manifest a hidden layer selects is left out and authored apps shadow the detected apps
/// under their roots. Without a config every manifest is listed. The authored apps are not
/// checked against the snapshot here; `trellis_rules_check` reports those errors.
pub(crate) fn prompt_apps(
    config: Option<&Config>,
    candidates: &crate::apps::Candidates,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<Value>> {
    let hidden = match config {
        Some(config) => {
            let paths: Vec<&str> = candidates
                .manifests
                .iter()
                .map(|(path, _)| path.as_str())
                .collect();
            hidden_layer_paths(config, &paths, check)?
        }
        None => BTreeSet::new(),
    };
    let detected = crate::apps::detect(candidates, &hidden).apps;
    let authored = config.map(Config::authored_apps).unwrap_or_default();
    Ok(crate::apps::effective(authored, detected)
        .into_iter()
        .map(|app| {
            json!({"name": app.name, "source": app.source, "kind": app.kind,
                "manifest": app.manifest, "roots": app.roots, "entries": app.entries})
        })
        .collect())
}

/// The effective app inventory of a snapshot (#794): `config`'s authored apps, then the
/// `detected` apps they do not shadow. In name order, the first authored app with a problem
/// fails: a root with no snapshot document under it is a `reference` error at the root, and a
/// name that a surviving detected app also has is an `ambiguity` error at the table.
fn effective_apps(
    config: &Config,
    detected: Vec<crate::apps::App>,
    documents: &BTreeSet<&str>,
) -> Result<Vec<crate::apps::App>> {
    let apps = crate::apps::effective(config.authored_apps(), detected);
    for authored in &config.apps {
        for (root, span) in &authored.roots {
            let found = if root == "." {
                !documents.is_empty()
            } else {
                let prefix = format!("{root}/");
                documents
                    .range::<&str, _>(prefix.as_str()..)
                    .next()
                    .is_some_and(|path| path.starts_with(&prefix))
            };
            if !found {
                return Err(error(
                    &config.text,
                    span.clone(),
                    "reference",
                    "token",
                    format!(
                        "app {} names root {root}, but no indexed document lies under it",
                        authored.name
                    ),
                ));
            }
        }
        let manifests: Vec<&str> = apps
            .iter()
            .filter(|app| {
                app.source == crate::apps::AppSource::Manifest && app.name == authored.name
            })
            .filter_map(|app| app.manifest.as_deref())
            .collect();
        if !manifests.is_empty() {
            return Err(error(
                &config.text,
                authored.span.clone(),
                "ambiguity",
                "token",
                format!(
                    "app {} is authored here and also detected by {}; author it at the detected root to replace that app",
                    authored.name,
                    manifests.join(" and ")
                ),
            ));
        }
    }
    Ok(apps)
}

/// The private payload field that carries [`path_apps`] to the MCP aggregation (#837). The
/// MCP layer removes it before it answers.
pub(crate) const PATH_APPS: &str = "_pathApps";

/// The app of every module that `references` emits, for `trellis_references`
/// `groupBy:"package"` (#837): `{path: manifest|null}`, null for a file in no app. It is the
/// rules check's and the Architecture summary's file assignment. With a rules file, `apps`
/// are the rules base's apps (a manifest a hidden layer selects is no app) and `layers` is
/// the whole checkout's layer assignment from the same evaluation check runs, so `package`
/// fails exactly when check does; a file's app is [`AppScope::file`] through its own layer.
/// Without one, `apps` are every detected manifest and a file's app is the one whose root
/// most specifically contains it.
pub(crate) fn path_apps(
    config: Option<&Config>,
    apps: &[crate::apps::App],
    layers: &BTreeMap<&str, Option<&str>>,
    references: &Projection,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Value> {
    let mut out = serde_json::Map::new();
    for module in references.data["modules"]
        .as_array()
        .context("store_unavailable: reference modules absent")?
    {
        check()?;
        let path = module["identity"]["path"]
            .as_str()
            .context("store_unavailable: module path absent")?;
        let app = match config {
            Some(config) => {
                AppScope::new(config, apps).file(path, layers.get(path).copied().flatten())
            }
            None => crate::apps::containing(apps, path),
        };
        out.insert(
            path.to_owned(),
            json!(app.map(|index| crate::apps::group_text(&apps[index]))),
        );
    }
    Ok(Value::Object(out))
}

/// The apps detected without a rules file: every manifest that detects one (#837).
pub(crate) fn unruled_apps(candidates: &crate::apps::Candidates) -> Vec<crate::apps::App> {
    crate::apps::detect(candidates, &BTreeSet::new()).apps
}

/// Resolve authored hidden membership for the explicit workbench metrics population.
/// Uses the same exact-match, specificity and ambiguity rules as architecture policy.
pub(crate) fn hidden_layer_paths(
    config: &Config,
    paths: &[&str],
    check: &mut impl FnMut() -> Result<()>,
) -> Result<BTreeSet<String>> {
    let mut work = Work::new(work::rules_limit(paths.len(), 0));
    let patterns: usize = config
        .layers
        .values()
        .map(|layer| layer.patterns.len())
        .sum();
    ensure!(
        work.charge(paths.len().saturating_mul(patterns)),
        "bounded_work: layer assignment pattern budget"
    );
    let mut matcher = LayerMatcher::new(config);
    let mut hidden = BTreeSet::new();
    for path in paths {
        check()?;
        if let Some(layer) = matcher.select(path, &mut work, check)?
            && hidden_layer(config, layer)
        {
            hidden.insert((*path).to_owned());
        }
    }
    Ok(hidden)
}

/// Whether `layer` is authored `display = "hidden"`: tooling, test harnesses and fixtures.
pub(crate) fn hidden_layer(config: &Config, layer: &str) -> bool {
    config
        .layers
        .get(layer)
        .is_some_and(|layer| layer.display.as_deref() == Some("hidden"))
}

struct LayerMatcher<'a> {
    config: &'a Config,
    relations: BTreeMap<(&'a str, &'a str), glob_language::Relation>,
}
impl<'a> LayerMatcher<'a> {
    fn new(config: &'a Config) -> Self {
        Self {
            config,
            relations: BTreeMap::new(),
        }
    }

    fn select(
        &mut self,
        path: &str,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<&'a str>> {
        let mut matching = Vec::new();
        for (name, layer) in &self.config.layers {
            for pattern in &layer.patterns {
                check()?;
                if pattern.matcher.is_match(path) {
                    matching.push((name.as_str(), layer, pattern));
                }
            }
        }
        let exact = matching
            .iter()
            .any(|(_, _, p)| !p.text.contains(['*', '?']));
        if exact {
            matching.retain(|(_, _, p)| !p.text.contains(['*', '?']));
        }
        let Some(first) = matching.first() else {
            return Ok(None);
        };
        if matching.iter().all(|(name, _, _)| *name == first.0) {
            return Ok(Some(first.0));
        }
        let mut dominated = vec![false; matching.len()];
        for i in 0..matching.len() {
            if exact {
                break;
            }
            for j in i + 1..matching.len() {
                check()?;
                ensure!(work.charge(1), "bounded_work: layer specificity comparison");
                let (a, b) = (matching[i].2.text.as_str(), matching[j].2.text.as_str());
                let (key, reverse) = if a <= b {
                    ((a, b), false)
                } else {
                    ((b, a), true)
                };
                let relation = if a == b {
                    glob_language::Relation::Equivalent
                } else if let Some(relation) = self.relations.get(&key) {
                    *relation
                } else {
                    let relation = glob_language::relation(key.0, key.1, work, check)?;
                    self.relations.insert(key, relation);
                    relation
                };
                use glob_language::Relation::*;
                match (relation, reverse) {
                    (StrictSubset, false) | (StrictSuperset, true) => dominated[j] = true,
                    (StrictSuperset, false) | (StrictSubset, true) => dominated[i] = true,
                    _ => {}
                }
            }
        }
        let mut remaining: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for ((name, _, pattern), dominated) in matching.iter().zip(dominated) {
            if !dominated {
                remaining.entry(name).or_default().push(&pattern.text);
            }
        }
        let (name, _) = remaining
            .first_key_value()
            .expect("strict inclusion has a minimum");
        if remaining.len() == 1 {
            return Ok(Some(name));
        }
        let tied = remaining
            .iter()
            .map(|(name, patterns)| format!("{name} ({})", patterns.join(", ")))
            .collect::<Vec<_>>()
            .join("; ");
        Err(error(
            &self.config.text,
            self.config.layers[*name].span.clone(),
            "ambiguity",
            "token",
            format!("path {path} matches equally specific layers: {tied}"),
        ))
    }
}

/// Which apps of the effective inventory each layer belongs to (#784, #794).
pub(crate) struct AppScope<'a> {
    config: &'a Config,
    apps: &'a [crate::apps::App],
}
impl<'a> AppScope<'a> {
    pub(crate) fn new(config: &'a Config, apps: &'a [crate::apps::App]) -> Self {
        Self { config, apps }
    }

    fn index(&self, name: &str) -> Option<usize> {
        self.apps.iter().position(|app| app.name == name)
    }

    /// The apps of layer `name`: an ordered layer's one app, or a shared layer's list.
    pub(crate) fn layer(&self, name: &str) -> BTreeSet<usize> {
        let Some(layer) = self.config.layers.get(name) else {
            return BTreeSet::new();
        };
        if layer.order.is_some() {
            return match &layer.app {
                Some((app, _)) => self.index(app).into_iter().collect(),
                None if self.apps.len() == 1 => BTreeSet::from([0]),
                None => BTreeSet::new(),
            };
        }
        match &layer.apps {
            Some(apps) => apps.iter().filter_map(|(app, _)| self.index(app)).collect(),
            None => (0..self.apps.len()).collect(),
        }
    }

    /// The name of the app at `index`, as returned by [`AppScope::file`].
    pub(crate) fn name(&self, index: usize) -> Option<&str> {
        self.apps.get(index).map(|app| app.name.as_str())
    }

    /// Whether layers `a` and `b` can be compared: always with fewer than two apps.
    pub(crate) fn share(&self, a: &str, b: &str) -> bool {
        self.apps.len() < 2 || !self.layer(a).is_disjoint(&self.layer(b))
    }

    /// The app of `path`: the one app of its `layer`, otherwise the app whose root contains it.
    pub(crate) fn file(&self, path: &str, layer: Option<&str>) -> Option<usize> {
        if self.apps.is_empty() {
            return None;
        }
        if let Some(layer) = layer {
            let apps = self.layer(layer);
            if apps.len() == 1 {
                return apps.into_iter().next();
            }
        }
        crate::apps::containing(self.apps, path)
    }

    /// The one app a rule row counts under; `None` is "no app" (#784). A `$layers` row between
    /// two layers belongs to their single common app. Every other row follows its target file,
    /// through the row's source layer or, when that is null, the file's own assigned layer.
    pub(crate) fn bucket(
        &self,
        assignments: &BTreeMap<&str, Option<&str>>,
        row: &Value,
    ) -> Option<usize> {
        self.bucket_with(|path| assignments.get(path).copied().flatten(), row)
    }

    /// [`AppScope::bucket`] with each file's assigned layer from `layer_of`.
    fn bucket_with<'l>(
        &self,
        layer_of: impl Fn(&str) -> Option<&'l str>,
        row: &'l Value,
    ) -> Option<usize> {
        if self.apps.is_empty() {
            return None;
        }
        let source = row["sourceLayer"].as_str();
        if row["rule"] == "$layers"
            && let (Some(source), Some(destination)) = (source, row["destinationLayer"].as_str())
        {
            let (from, to) = (self.layer(source), self.layer(destination));
            let mut common = from.intersection(&to).copied();
            return match (common.next(), common.next()) {
                (Some(app), None) => Some(app),
                _ => None,
            };
        }
        let path = row["target"]["path"].as_str()?;
        let layer = source.or_else(|| layer_of(path));
        self.file(path, layer)
    }
}

/// Each file's app (#851), as `trellis_rules_check`, `groupBy:"package"` and the Architecture
/// summary assign it: through its layer with a rules file ([`AppScope::file`]), by the most
/// specific root without one. It owns its layer map, so a read keeps it past its evaluation.
pub(crate) struct AppAssignment<'a> {
    config: Option<&'a Config>,
    apps: Vec<crate::apps::App>,
    layers: BTreeMap<String, Option<String>>,
}

impl<'a> AppAssignment<'a> {
    /// The assignment from an evaluation's layer map. Without a config every file goes by root.
    pub(crate) fn new(
        config: Option<&'a Config>,
        apps: Vec<crate::apps::App>,
        layers: &BTreeMap<&str, Option<&str>>,
    ) -> Self {
        let layers = layers
            .iter()
            .map(|(path, layer)| ((*path).to_owned(), layer.map(str::to_owned)))
            .collect();
        Self {
            config,
            apps,
            layers,
        }
    }

    /// The assignment of exactly `paths`, each matched to its layer as evaluation matches it.
    pub(crate) fn for_paths(
        config: &'a Config,
        apps: Vec<crate::apps::App>,
        paths: &[&str],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut work = Work::new(work::rules_limit(paths.len(), 0));
        let patterns: usize = config
            .layers
            .values()
            .map(|layer| layer.patterns.len())
            .sum();
        ensure!(
            work.charge(paths.len().saturating_mul(patterns)),
            "bounded_work: layer assignment pattern budget"
        );
        let mut matcher = LayerMatcher::new(config);
        let mut layers = BTreeMap::new();
        for path in paths {
            check()?;
            let layer = matcher.select(path, &mut work, check)?;
            layers.insert((*path).to_owned(), layer.map(str::to_owned));
        }
        Ok(Self {
            config: Some(config),
            apps,
            layers,
        })
    }

    /// The effective app inventory, in inventory order.
    pub(crate) fn apps(&self) -> &[crate::apps::App] {
        &self.apps
    }

    fn layer(&self, path: &str) -> Option<&str> {
        self.layers.get(path).and_then(|layer| layer.as_deref())
    }

    /// The index of the app `path` belongs to; `None` is no app.
    pub(crate) fn file(&self, path: &str) -> Option<usize> {
        match self.config {
            Some(config) => AppScope::new(config, &self.apps).file(path, self.layer(path)),
            None => crate::apps::containing(&self.apps, path),
        }
    }

    /// Whether `path`'s language counts toward its app's `languages` (#909): not when its
    /// layer is authored hidden (tooling, test harnesses and fixtures).
    pub(crate) fn counts_language(&self, path: &str) -> bool {
        match (self.config, self.layer(path)) {
            (Some(config), Some(layer)) => !hidden_layer(config, layer),
            _ => true,
        }
    }

    /// The one app a rule row counts under (#784), as `apps[].counts` counts it.
    pub(crate) fn bucket(&self, row: &Value) -> Option<usize> {
        let config = self.config?;
        AppScope::new(config, &self.apps).bucket_with(|path| self.layer(path), row)
    }

    /// The apps an `app` argument names (#851): every app whose name or manifest path is
    /// `app`. None is `not_found`.
    pub(crate) fn named(&self, app: &str) -> Result<BTreeSet<usize>> {
        let named: BTreeSet<usize> = self
            .apps
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                candidate.name == app || candidate.manifest.as_deref() == Some(app)
            })
            .map(|(index, _)| index)
            .collect();
        ensure!(!named.is_empty(), "not_found: {UNKNOWN_APP}");
        Ok(named)
    }
}

/// The message of an `app` argument that names no app of the effective inventory (#851).
pub(crate) const UNKNOWN_APP: &str =
    "No app of this checkout has that name or manifest path; trellis_workspace_describe lists them";

/// Check every layer's `app` and `apps` against the effective apps (#794); the first layer in
/// name order with a problem wins. Then each `[[interfaces.wrappers]]` `app`, in written
/// order (#855).
fn validate_apps(config: &Config, apps: &[crate::apps::App]) -> Result<()> {
    validate_layer_apps(config, apps)?;
    for wrapper in &config.wrappers {
        let Some((name, span)) = &wrapper.app else {
            continue;
        };
        let matching = apps.iter().filter(|app| app.name == *name).count();
        if matching == 1 {
            continue;
        }
        let known = apps
            .iter()
            .map(|app| app.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(error(
            &config.text,
            span.clone(),
            if matching == 0 {
                "reference"
            } else {
                "ambiguity"
            },
            "token",
            if matching == 0 {
                format!(
                    "wrapper {} names app {name}, which is not detected or authored (apps: {known})",
                    wrapper.callee
                )
            } else {
                format!(
                    "wrapper {} names app {name}, which {matching} apps share",
                    wrapper.callee
                )
            },
        ));
    }
    Ok(())
}

/// The app of each path, as rule evaluation assigns it: the one app of the path's layer,
/// otherwise the app whose root contains it (#784). Used for app-scoped interface wrappers
/// (#855).
pub(crate) fn file_apps<'p>(
    config: &Config,
    base: &RulesBase,
    paths: impl IntoIterator<Item = &'p str>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<BTreeMap<String, String>> {
    let scope = AppScope::new(config, base.apps());
    let patterns: usize = config
        .layers
        .values()
        .map(|layer| layer.patterns.len())
        .sum();
    let mut matcher = LayerMatcher::new(config);
    let mut assigned = BTreeMap::new();
    for path in paths {
        check()?;
        // `select` charges only specificity comparisons; its scan of every pattern is charged
        // here, as the other callers precharge it.
        ensure!(
            work.charge(patterns),
            "bounded_work: layer assignment pattern budget"
        );
        let layer = matcher.select(path, work, check)?;
        if let Some(app) = scope.file(path, layer).and_then(|at| scope.name(at)) {
            assigned.insert(path.to_owned(), app.to_owned());
        }
    }
    Ok(assigned)
}

fn validate_layer_apps(config: &Config, apps: &[crate::apps::App]) -> Result<()> {
    for (layer_name, layer) in &config.layers {
        let named: Vec<&(String, std::ops::Range<usize>)> = layer
            .app
            .iter()
            .chain(layer.apps.iter().flatten())
            .collect();
        if apps.len() >= 2 && layer.order.is_some() && layer.app.is_none() {
            return Err(error(
                &config.text,
                layer.span.clone(),
                "schema",
                "token",
                format!(
                    "layer {layer_name} must name its app: {} apps are detected",
                    apps.len()
                ),
            ));
        }
        for (name, span) in named {
            let matching: Vec<&crate::apps::App> =
                apps.iter().filter(|app| app.name == *name).collect();
            match matching.as_slice() {
                [_] => {}
                [] if apps.is_empty() => {
                    return Err(error(
                        &config.text,
                        span.clone(),
                        "reference",
                        "token",
                        format!(
                            "layer {layer_name} names app {name}, but no app is detected or authored"
                        ),
                    ));
                }
                [] => {
                    let detected = apps
                        .iter()
                        .map(|app| app.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(error(
                        &config.text,
                        span.clone(),
                        "reference",
                        "token",
                        format!(
                            "layer {layer_name} names app {name}, which is not detected or authored (apps: {detected})"
                        ),
                    ));
                }
                many => {
                    let manifests = many
                        .iter()
                        .filter_map(|app| app.manifest.as_deref())
                        .collect::<Vec<_>>()
                        .join(" and ");
                    return Err(error(
                        &config.text,
                        span.clone(),
                        "ambiguity",
                        "token",
                        format!("app {name} is detected by {manifests}"),
                    ));
                }
            }
        }
    }
    Ok(())
}
fn layer_status(
    config: &Config,
    scope: &AppScope,
    source: Option<&str>,
    target: Option<&str>,
) -> (&'static str, Vec<String>) {
    let (Some(s), Some(t)) = (source, target) else {
        return ("unknown", vec!["layer_unassigned".into()]);
    };
    let from = &config.layers[s];
    let to = &config.layers[t];
    if s == t {
        return ("conforms", vec![]);
    }
    if !scope.share(s, t) {
        return ("unknown", vec!["cross_app".into()]);
    }
    if to.order.is_none() {
        return ("conforms", vec![]);
    }
    let policy = match (from.order, to.order) {
        (Some(a), Some(b)) if a < b => return ("conforms", vec![]),
        (Some(a), Some(b)) if a > b => return ("deviates", vec![]),
        (Some(_), Some(_)) => config.peers.as_deref(),
        _ => config.shared_outbound.as_deref(),
    };
    if policy == Some("allow") {
        ("conforms", vec![])
    } else {
        ("deviates", vec![])
    }
}
fn portable_source_set<'a>(
    config: &Config,
    snapshot: &'a Snapshot,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<&'a str>> {
    let Some(span) = config
        .exceptions
        .iter()
        .find_map(|exception| match &exception.kind {
            ExceptionKind::ModulePair { span, .. } => Some(span),
            ExceptionKind::Declaration(_) => None,
        })
    else {
        return Ok(None);
    };
    let mut scope = None;
    for source_set in snapshot
        .documents
        .iter()
        .map(|doc| doc.key.source_set_id.as_str())
        .chain(
            snapshot
                .manifests
                .iter()
                .map(|manifest| manifest.source_set_id.as_str()),
        )
    {
        check()?;
        ensure!(
            work.charge(1),
            "bounded_work: portable exception scope budget"
        );
        if scope.is_some_and(|current| current != source_set) {
            return Err(error(
                &config.text,
                span.clone(),
                "ambiguity",
                "token",
                "portable layer exceptions require one source set across the full checkout",
            ));
        }
        scope = Some(source_set);
    }
    scope.map(Some).ok_or_else(|| {
        error(
            &config.text,
            span.clone(),
            "reference",
            "token",
            "portable layer exceptions require an admitted source set",
        )
    })
}
fn decorate(config: &Config, source_set: Option<&str>, row: &mut Value) {
    if let Some(e) = config.exceptions.iter().find(|e| {
        row["rule"] == e.rule
            && match &e.kind {
                ExceptionKind::Declaration(target) => {
                    row["target"] == *target && row["destination"].is_null()
                }
                ExceptionKind::ModulePair {
                    source,
                    destination,
                    ..
                } => source_set.is_some_and(|scope| {
                    source.matches(&row["target"], scope)
                        && destination.matches(&row["destination"], scope)
                }),
            }
    }) {
        row["excepted"] = json!(true);
        row["exceptionReason"] = json!(e.reason);
    }
}
fn counts<'a>(rows: impl IntoIterator<Item = &'a Value>) -> Value {
    let mut comparable = 0;
    let mut conforms = 0;
    let mut deviates = 0;
    let mut unknown = 0;
    let mut excepted = 0;
    for row in rows {
        comparable += 1;
        if row["excepted"] == true {
            excepted += 1;
        } else {
            match row["status"].as_str() {
                Some("conforms") => conforms += 1,
                Some("deviates") => deviates += 1,
                _ => unknown += 1,
            }
        }
    }
    json!({"comparable":comparable,"conforms":conforms,"deviates":deviates,"unknown":unknown,"excepted":excepted})
}
/// A reference site as rule evidence: a semantic site carries its binding's proof (#523).
/// Every reference site also carries its call-site `lines`, `relationKind`, `spelling`,
/// `owner`, reference `target` and `targetReason`, copied as the site has them (#785).
fn reference_evidence(module: &Value, site: &Value) -> Value {
    let (kind, provenance) = if site["mapping"] == "semantic" {
        ("semanticBinding", &site["provenanceId"])
    } else {
        ("structuralReference", &module["provenanceId"])
    };
    let mut evidence = json!({"document":{"sourceSetId":module["identity"]["sourceSetId"],"language":module["identity"]["language"],"path":module["identity"]["path"]},
        "revisionId":module["revisionId"],"range":site["range"],"kind":kind,"syntaxId":site["ownerSyntaxId"],"callId":site["callId"],"provenanceId":provenance});
    evidence["caller"] = site["caller"].clone();
    evidence["callerReason"] = site["callerReason"].clone();
    for key in [
        "lines",
        "relationKind",
        "spelling",
        "owner",
        "target",
        "targetReason",
    ] {
        evidence[key] = site[key].clone();
    }
    if site["mapping"] == "semantic" {
        for key in [
            "projection",
            "freshness",
            "historicalFreshness",
            "historicalContentHash",
            "evidenceKind",
            "derivedFrom",
            "basis",
            "historicalTargets",
            "historicalExternalTargets",
            "historicalResolution",
            "historicalDispatch",
            "historicalJoin",
            "semanticBoundary",
        ] {
            if let Some(value) = site.get(key) {
                evidence[key] = value.clone();
            }
        }
        if let Some(revision) = site["projection"]["basisNativeRevisionId"].as_str() {
            evidence["revisionId"] = json!(revision);
        }
    }
    evidence
}

/// `kindCounts` of an unknown row that holds one site.
fn unknown_kind_counts(site: &Value) -> Value {
    let mut counts = json!({"call":0,"type":0,"import":0,"reference":0});
    if let Some(kind) = site["relationKind"].as_str() {
        counts[kind] = json!(1);
    }
    counts
}

fn semantic_evidence(mut evidence: Value, metadata: &BTreeMap<String, Value>) -> Value {
    for (key, value) in metadata {
        evidence[key] = value.clone();
    }
    if let Some(revision) = evidence["projection"]["basisNativeRevisionId"].as_str() {
        evidence["revisionId"] = json!(revision);
    }
    evidence
}
#[derive(Default)]
struct Bases {
    extends: Option<Vec<String>>,
    implements: Option<Vec<String>>,
}
fn children(node: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
    let mut c = node.walk();
    node.named_children(&mut c).collect()
}
fn base_texts(node: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    children(node)
        .into_iter()
        .flat_map(|n| {
            if matches!(n.kind(), "type_list" | "trait_bounds") {
                children(n)
            } else {
                vec![n]
            }
        })
        .filter_map(|n| source.get(n.byte_range()).map(str::to_owned))
        .collect()
}
/// Each `extends` value from its start to the end of the `type_arguments` that follows it.
fn ts_extends_texts(clause: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let nodes = children(clause);
    let mut texts = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        if clause.field_name_for_named_child(index as u32) != Some("value") {
            continue;
        }
        let end = nodes
            .get(index + 1)
            .filter(|next| next.kind() == "type_arguments")
            .map_or(node.end_byte(), |next| next.end_byte());
        if let Some(text) = source.get(node.start_byte()..end) {
            texts.push(text.to_owned());
        }
    }
    texts
}
fn has_bases(node: tree_sitter::Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class_declaration"
            | "abstract_class_declaration"
            | "class_definition"
            | "interface_declaration"
            | "type_alias_declaration"
            | "impl_item"
            | "trait_item"
            | "enum_declaration"
            | "record_declaration"
    )
}
fn parse_bases(
    doc: &crate::references::DocumentInput,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<BTreeMap<String, Bases>> {
    check()?;
    ensure!(
        doc.file.text.len() <= CONFIG_BYTES,
        "bounded_work: selector source exceeds file bound"
    );
    let language = match doc.key.language.as_str() {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => crate::indexer_ts::grammar(&doc.key.path),
        _ => anyhow::bail!("store_unavailable: unknown selector language"),
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language)?;
    let source = &doc.file.text;
    let mut aborted = None;
    let tree = parser.parse_with_options(
        &mut |offset, _| &source.as_bytes()[offset..],
        None,
        Some(
            tree_sitter::ParseOptions::new().progress_callback(&mut |_| {
                if let Err(e) = check() {
                    aborted = Some(e);
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            }),
        ),
    );
    if let Some(e) = aborted {
        return Err(e);
    }
    let tree = tree.context("bounded_work: selector parser stopped")?;
    // A base clause inside a parser error goes unseen, so its file's bases stay unknown.
    let broken = tree.root_node().has_error();
    let mut types = BTreeMap::new();
    // A type past the walk's bounds is unknown here, and references report its file.
    crate::references::walk(tree.root_node(), check, &mut |node, _check| {
        if has_bases(node) {
            types.insert((node.start_byte(), node.end_byte()), node);
        }
        Ok(false)
    })?;
    let mut expansions = crate::native_rust_macros::Expansions::new(source);
    let mut result = BTreeMap::new();
    for d in &doc.declarations {
        check()?;
        if !matches!(d.kind.as_str(), "type" | "implementation") {
            result.insert(
                d.syntax_id.clone(),
                Bases {
                    extends: Some(vec![]),
                    implements: Some(vec![]),
                },
            );
            continue;
        }
        if broken {
            result.insert(d.syntax_id.clone(), Bases::default());
            continue;
        }
        let mut node = types.get(&(d.range.start, d.range.end)).copied();
        if node.is_none() && doc.key.language == "rust" {
            // An `impl` measured inside a macro has syntax only in that macro's expansion
            // (#551). Only measured macros are reached, so none of it can exceed a bound here.
            node = expansions
                .root_for(tree.root_node(), d.range.start, d.range.end)
                .descendant_for_byte_range(d.range.start, d.range.end)
                .filter(|n| {
                    has_bases(*n) && n.start_byte() == d.range.start && n.end_byte() == d.range.end
                });
        }
        let mut bases = Bases::default();
        if let Some(n) = node {
            match doc.key.language.as_str() {
                "java" => {
                    let mut extends = Vec::new();
                    let mut implements = Vec::new();
                    if let Some(b) = n.child_by_field_name("superclass") {
                        extends.extend(base_texts(b, source));
                    }
                    if let Some(b) = n.child_by_field_name("interfaces") {
                        implements.extend(base_texts(b, source));
                    }
                    for b in children(n)
                        .into_iter()
                        .filter(|n| n.kind() == "extends_interfaces")
                    {
                        extends.extend(base_texts(b, source));
                    }
                    bases = Bases {
                        extends: Some(extends),
                        implements: Some(implements),
                    };
                }
                "python" => {
                    bases.extends = Some(
                        n.child_by_field_name("superclasses")
                            .map(|b| base_texts(b, source))
                            .unwrap_or_default(),
                    );
                }
                "javascript" => {
                    bases.extends = Some(
                        children(n)
                            .into_iter()
                            .find(|b| b.kind() == "class_heritage")
                            .map(|b| base_texts(b, source))
                            .unwrap_or_default(),
                    );
                }
                "typescript" => {
                    let mut extends = Vec::new();
                    let mut implements = Vec::new();
                    for heritage in children(n)
                        .into_iter()
                        .filter(|c| c.kind() == "class_heritage")
                    {
                        for clause in children(heritage) {
                            match clause.kind() {
                                "extends_clause" => {
                                    extends.extend(ts_extends_texts(clause, source))
                                }
                                "implements_clause" => {
                                    implements.extend(base_texts(clause, source))
                                }
                                _ => {}
                            }
                        }
                    }
                    for clause in children(n)
                        .into_iter()
                        .filter(|c| c.kind() == "extends_type_clause")
                    {
                        extends.extend(base_texts(clause, source));
                    }
                    bases = Bases {
                        extends: Some(extends),
                        implements: Some(implements),
                    };
                }
                "rust" if n.kind() == "impl_item" => {
                    bases.implements = Some(
                        n.child_by_field_name("trait")
                            .and_then(|b| source.get(b.byte_range()).map(str::to_owned))
                            .into_iter()
                            .collect(),
                    );
                }
                _ => {}
            }
        }
        result.insert(d.syntax_id.clone(), bases);
    }
    Ok(result)
}
fn selection(selector: &Selector, d: &Declaration, bases: Option<&Bases>) -> Option<bool> {
    if !glob_matches(&selector.paths, &d.document.path)
        || (!selector.language.is_empty() && !selector.language.contains(&d.document.language))
        || selector
            .name
            .as_ref()
            .is_some_and(|p| d.name.as_ref().is_none_or(|n| !p.is_match(n)))
    {
        return Some(false);
    }
    let mut unknown = false;
    for (wanted, actual) in [
        (&selector.extends, bases.and_then(|b| b.extends.as_ref())),
        (
            &selector.implements,
            bases.and_then(|b| b.implements.as_ref()),
        ),
    ] {
        if let Some(wanted) = wanted {
            if let Some(actual) = actual {
                if !actual.contains(wanted) {
                    return Some(false);
                }
            } else {
                unknown = true;
            }
        }
    }
    if unknown { None } else { Some(true) }
}
fn package_closure(
    packages: &[Value],
    instances: &[Value],
    test_only: &[Value],
    unassigned: &[Value],
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<Value>> {
    let mut needed = BTreeSet::new();
    for identity in instances
        .iter()
        .chain(test_only)
        .map(|row| &row["destination"])
        .chain(unassigned)
    {
        check()?;
        ensure!(
            work.charge(1),
            "bounded_work: rules external package closure budget"
        );
        if identity["kind"] == "externalPackage" {
            needed.insert(identity.to_string());
        }
    }
    let mut retained = Vec::new();
    for package in packages {
        check()?;
        ensure!(
            work.charge(1),
            "bounded_work: rules external package closure budget"
        );
        if needed.remove(&package["identity"].to_string()) {
            retained.push(package.clone());
        }
    }
    ensure!(
        needed.is_empty(),
        "store_unavailable: rules external package closure absent"
    );
    Ok(retained)
}
/// Every declaration of the snapshot by `(document index, syntax ID)`: semantic binding
/// targets are selected by role against these (#523).
type Targets<'a> = BTreeMap<(usize, &'a str), &'a Declaration>;

struct DocumentEvaluation<'a> {
    config: &'a Config,
    targets: &'a Targets<'a>,
    inheritance: bool,
    source_set: Option<&'a str>,
    #[cfg(test)]
    before_document: Option<&'a (dyn Fn(&crate::references::DocumentInput) + Sync)>,
}
impl DocumentEvaluation<'_> {
    fn run(
        &self,
        documents: &[&crate::references::DocumentInput],
        work: &mut Work,
        workers: usize,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<(Vec<Value>, Vec<Value>)> {
        let workers = workers.clamp(1, crate::parallel::MAX_WORKERS);
        let request = RequestCheck::new(check);
        let mut instances = Vec::new();
        let mut test_only = Vec::new();
        for batch in documents.chunks(workers) {
            request.result(request.check())?;
            let remaining = work.remaining();
            let outcomes =
                crate::parallel::map_ordered(batch, workers, &request.stopped, |_, doc| {
                    #[cfg(test)]
                    if let Some(before) = self.before_document {
                        before(doc);
                    }
                    evaluate_document(
                        self.config,
                        self.targets,
                        doc,
                        self.inheritance,
                        self.source_set,
                        remaining,
                        &mut request.worker(),
                    )
                });
            request.result(Ok(()))?;
            for outcome in outcomes {
                let rows =
                    request.result(outcome.context("cancelled: rules worker not started")?)?;
                ensure!(
                    work.charge(rows.work.used),
                    "bounded_work: rules document budget"
                );
                instances.extend(rows.instances);
                test_only.extend(rows.test_only);
                request.result(request.check())?;
            }
        }
        Ok((instances, test_only))
    }
}

struct DocumentRows {
    instances: Vec<Value>,
    test_only: Vec<Value>,
    work: Work,
}

fn evaluate_document(
    config: &Config,
    targets: &Targets<'_>,
    doc: &crate::references::DocumentInput,
    inheritance: bool,
    source_set: Option<&str>,
    limit: usize,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<DocumentRows> {
    let mut work = Work::new(limit);
    let callers = crate::references::callers::Callers::new(doc);
    let mut instances = Vec::new();
    let mut test_only = Vec::new();
    let bases = if inheritance {
        parse_bases(doc, check)?
    } else {
        BTreeMap::new()
    };
    let mut anchors = BTreeMap::new();
    if !config.rules.is_empty() {
        for anchor in &doc.semantic.anchors {
            ensure!(work.charge(1), "bounded_work: rule constraint budget");
            if let Some(call) = &anchor.call_id {
                anchors.insert(call.as_str(), anchor);
            }
        }
    }
    for d in &doc.declarations {
        check()?;
        for rule in &config.rules {
            ensure!(
                work.charge(1 + config.roles[&rule.role].paths.len()),
                "bounded_work: rules selection budget"
            );
            check()?;
            let selected = selection(&config.roles[&rule.role], d, bases.get(&d.syntax_id));
            if selected == Some(false) {
                continue;
            }
            let mut unresolved = Vec::new();
            if selected.is_none() {
                unresolved.push("selector_relationship_unavailable".to_owned());
            }
            let mut constraints = Vec::new();
            let mut statuses = Vec::new();
            for c in &rule.constraints {
                ensure!(
                    work.charge(
                        1 + if c.kind == "calls" {
                            doc.calls.len()
                        } else {
                            doc.semantic.relationships.len()
                        },
                    ),
                    "bounded_work: rule constraint budget"
                );
                check()?;
                let (status, reason, mut evidence) =
                    constraint(config, targets, &anchors, doc, d, c, &mut work, check)?;
                for site in &mut evidence {
                    callers.annotate(site);
                }
                statuses.push(status);
                constraints.push(json!({"mode":c.mode,"kind":c.kind,"targetRole":c.role,"status":status,"evidence":evidence,"unresolved":reason.into_iter().collect::<Vec<_>>(),"projection":doc.semantic.projection}));
                if let Some(reason) = reason
                    && !unresolved.iter().any(|r| r == reason)
                {
                    unresolved.push(reason.to_owned());
                }
            }
            // A proven deviation wins over uncertainty; conformance needs every constraint.
            let status = if selected != Some(true) || statuses.is_empty() {
                "unknown"
            } else if statuses.contains(&"deviates") {
                "deviates"
            } else if statuses.iter().all(|status| *status == "conforms") {
                "conforms"
            } else {
                "unknown"
            };
            let test = d.role == DeclarationRole::Test;
            if test {
                unresolved.push("test_only".into());
            }
            let mut row = json!({"rule":rule.id,"target":identity(d),"destination":null,"sourceLayer":null,"destinationLayer":null,"status":status,"excepted":false,"exceptionReason":null,
                    "constraints":constraints,"evidence":[native_evidence(d)],"unresolved":unresolved,"referenceCount":0,"projection":doc.semantic.projection});
            ensure!(
                work.charge(config.exceptions.len()),
                "bounded_work: exception match budget"
            );
            decorate(config, source_set, &mut row);
            if test {
                test_only.push(row)
            } else {
                instances.push(row)
            }
        }
    }
    Ok(DocumentRows {
        instances,
        test_only,
        work,
    })
}

/// One semantic constraint of a selected declaration (#523): its status, the reason it is
/// unknown, and its evidence. A call counts when its selected binding is direct (or a
/// constructor) to a declaration the target role selects; a type relationship counts when
/// its target is such a declaration. A required edge is proven by one counted binding; a
/// forbidden one deviates on one. A forbidden call conforms only when every measured call
/// of the declaration is bound, in a document the producer covers completely. Absence of a
/// required edge never establishes a violation.
#[allow(clippy::too_many_arguments)]
fn constraint(
    config: &Config,
    targets: &Targets<'_>,
    anchors: &BTreeMap<&str, &crate::references::SemanticAnchor>,
    doc: &crate::references::DocumentInput,
    d: &Declaration,
    c: &Constraint,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<(&'static str, Option<&'static str>, Vec<Value>)> {
    use crate::references::SemanticOutcome;
    let role = &config.roles[&c.role];
    let bound = doc.semantic.producer_id.is_some();
    let calls = c.kind == "calls";
    let mut evidence = vec![native_evidence(d)];
    let (mut hits, mut uncertain) = (0usize, 0usize);
    let select = |document: &usize, sid: &str| {
        targets
            .get(&(*document, sid))
            .and_then(|target| selection(role, target, None))
    };
    if calls {
        for call in doc
            .calls
            .iter()
            .filter(|call| call.owner_syntax_id == d.syntax_id)
        {
            ensure!(work.charge(1), "bounded_work: rule evidence budget");
            check()?;
            evidence.push(json!({"document":call.document,"revisionId":call.revision_id,"range":call.range,"kind":"call","syntaxId":call.owner_syntax_id,"callId":call.id,"provenanceId":call.provenance_id}));
            if !bound {
                continue;
            }
            let Some(anchor) = anchors.get(call.id.as_str()) else {
                uncertain += 1;
                continue;
            };
            if anchor.metadata.contains_key("projection") {
                evidence.push(semantic_evidence(json!({"document":call.document,"revisionId":call.revision_id,"range":anchor.range,"kind":"semanticBinding","syntaxId":call.owner_syntax_id,"callId":call.id,"provenanceId":anchor.provenance_id}), &anchor.metadata));
            }
            match &anchor.outcome {
                SemanticOutcome::Internal(key, sid) if anchor.direct => match select(key, sid) {
                    Some(true) => {
                        hits += 1;
                        evidence.push(semantic_evidence(json!({"document":call.document,"revisionId":call.revision_id,"range":anchor.range,"kind":"semanticBinding","syntaxId":sid,"callId":call.id,"provenanceId":anchor.provenance_id}), &anchor.metadata));
                    }
                    Some(false) => {}
                    None => uncertain += 1,
                },
                SemanticOutcome::External => {}
                _ => uncertain += 1,
            }
        }
    } else if bound {
        for relationship in doc
            .semantic
            .relationships
            .iter()
            .filter(|r| r.source_syntax_id == d.syntax_id && r.kind == c.kind)
        {
            ensure!(work.charge(1), "bounded_work: rule evidence budget");
            check()?;
            if relationship.metadata.contains_key("projection") {
                evidence.push(semantic_evidence(json!({"document":d.document,"revisionId":d.revision_id,"range":d.range,"kind":"typeRelationship","syntaxId":d.syntax_id,"callId":null,"provenanceId":relationship.provenance_id}), &relationship.metadata));
            }
            let Some((key, sid)) = &relationship.target else {
                uncertain += usize::from(
                    relationship.metadata.get("semanticBoundary") == Some(&json!(true)),
                );
                continue;
            };
            match select(key, sid) {
                Some(true) => {
                    hits += 1;
                    evidence.push(semantic_evidence(json!({"document":d.document,"revisionId":d.revision_id,"range":d.range,"kind":"typeRelationship","syntaxId":sid,"callId":null,"provenanceId":relationship.provenance_id}), &relationship.metadata));
                }
                Some(false) => {}
                None => uncertain += 1,
            }
        }
    }
    let (unavailable, incomplete, absent) = if calls {
        (
            "call_binding_unavailable",
            "call_binding_incomplete",
            "required_call_absent",
        )
    } else {
        (
            "type_relationship_unavailable",
            "type_relationship_incomplete",
            "type_relationship_absent",
        )
    };
    let (status, reason) = match (c.mode, bound, hits > 0) {
        (_, false, _) => ("unknown", Some(unavailable)),
        (_, _, false) if doc.semantic.projection.is_some() => {
            ("unknown", Some("historical_semantic_absence_unproven"))
        }
        ("require", _, true) => ("conforms", None),
        ("forbid", _, true) => ("deviates", None),
        ("forbid", _, false)
            if calls
                && uncertain == 0
                && doc.semantic.complete
                && doc.semantic.projection.is_none() =>
        {
            ("conforms", None)
        }
        _ if uncertain > 0 || (calls && !doc.semantic.complete) => ("unknown", Some(incomplete)),
        // Only a forbidden type relationship reaches here: absence is not proof.
        ("forbid", ..) => ("unknown", Some("type_relationship_absence_unproven")),
        _ => ("unknown", Some(absent)),
    };
    Ok((status, reason, evidence))
}

pub fn evaluate(
    config: &Config,
    snapshot: &Snapshot,
    refs: &Projection,
    paths: &[String],
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Value> {
    Ok(evaluate_with_layers(config, snapshot, refs, paths, check)?.data)
}

pub(crate) struct Evaluation<'a> {
    pub data: Value,
    pub layers: BTreeMap<&'a str, Option<&'a str>>,
}

pub(crate) fn evaluate_with_layers<'a>(
    config: &'a Config,
    snapshot: &Snapshot,
    refs: &'a Projection,
    paths: &[String],
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Evaluation<'a>> {
    evaluate_with_layers_workers(
        config,
        snapshot,
        refs,
        paths,
        check,
        crate::parallel::default_workers(),
    )
}

#[cfg(feature = "test-support")]
pub fn evaluate_with_workers(
    config: &Config,
    snapshot: &Snapshot,
    refs: &Projection,
    paths: &[String],
    workers: usize,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Value> {
    Ok(evaluate_with_layers_workers(config, snapshot, refs, paths, check, workers)?.data)
}

fn evaluate_with_layers_workers<'a>(
    config: &'a Config,
    snapshot: &Snapshot,
    refs: &'a Projection,
    paths: &[String],
    check: &mut (impl FnMut() -> Result<()> + Send),
    workers: usize,
) -> Result<Evaluation<'a>> {
    let base = prepare_with_workers(config, snapshot, paths, check, workers)?;
    evaluate_prepared(config, &base, refs, paths, check)
}

/// What rule evaluation needs from a snapshot: the budget it sets and each document's
/// declaration rows, so that later evaluations need no source (#556).
pub(crate) struct RulesBase {
    limit: usize,
    source_set: Option<String>,
    semantic: bool,
    declarations: Vec<Value>,
    test_declarations: Vec<Value>,
    work_used: usize,
    apps: Vec<crate::apps::App>,
    unread_manifests: usize,
}

impl RulesBase {
    /// The effective apps for this snapshot (#794): authored apps by name, then the detected
    /// apps they do not shadow, in manifest-path order.
    pub(crate) fn apps(&self) -> &[crate::apps::App] {
        &self.apps
    }
}

/// Evaluate every document's declaration rules once; `evaluate_prepared` selects from them.
pub(crate) fn prepare(
    config: &Config,
    snapshot: &Snapshot,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<RulesBase> {
    prepare_with_workers(
        config,
        snapshot,
        &[],
        check,
        crate::parallel::default_workers(),
    )
}

fn path_filter(config: &Config, paths: &[String]) -> Result<Vec<GlobSet>> {
    let path_values: Vec<_> = paths
        .iter()
        .map(|p| Spanned::new(0..0, p.clone()))
        .collect();
    let mut budget = 0;
    path_values
        .iter()
        .map(|p| pattern(&config.text, p, true, &mut budget))
        .collect::<Result<Vec<_>>>()
        .map_err(|_| anyhow::anyhow!("invalid_request: invalid rules source path glob"))
}

fn prepare_with_workers(
    config: &Config,
    snapshot: &Snapshot,
    paths: &[String],
    check: &mut (impl FnMut() -> Result<()> + Send),
    workers: usize,
) -> Result<RulesBase> {
    let limit = work::rules_limit(
        snapshot.documents.len(),
        work::snapshot_bytes(snapshot, check)?,
    );
    let filter = path_filter(config, paths)?;
    let mut work = Work::new(limit);
    let source_set = portable_source_set(config, snapshot, &mut work, check)?;
    let inheritance = config
        .roles
        .values()
        .any(|s| s.extends.is_some() || s.implements.is_some());
    let mut source_bytes = 0usize;
    let mut documents = Vec::new();
    for doc in &snapshot.documents {
        check()?;
        if !glob_matches(&filter, &doc.key.path) {
            continue;
        }
        source_bytes = source_bytes.saturating_add(doc.file.text.len());
        ensure!(
            source_bytes <= 256 * 1024 * 1024,
            "bounded_work: selector total source bound"
        );
        documents.push(doc);
    }
    let apps = {
        let document_paths: BTreeSet<&str> = snapshot
            .documents
            .iter()
            .map(|doc| doc.key.path.as_str())
            .collect();
        let candidates =
            crate::apps::candidates(&snapshot.manifests, &document_paths, &mut work, check)?;
        let manifest_paths: Vec<&str> = candidates
            .manifests
            .iter()
            .map(|(path, _)| path.as_str())
            .collect();
        let hidden = hidden_layer_paths(config, &manifest_paths, check)?;
        let detected = crate::apps::detect(&candidates, &hidden);
        (
            effective_apps(config, detected.apps, &document_paths)?,
            detected.unread,
        )
    };
    validate_apps(config, &apps.0)?;
    let mut targets: Targets<'_> = BTreeMap::new();
    let semantic = snapshot
        .documents
        .iter()
        .any(|doc| doc.semantic.producer_id.is_some());
    if semantic && !config.rules.is_empty() {
        for (index, doc) in snapshot.documents.iter().enumerate() {
            for d in &doc.declarations {
                check()?;
                ensure!(work.charge(1), "bounded_work: rules selection budget");
                targets.insert((index, d.syntax_id.as_str()), d);
            }
        }
    }
    let (declarations, test_declarations) = DocumentEvaluation {
        config,
        targets: &targets,
        inheritance,
        source_set,
        #[cfg(test)]
        before_document: None,
    }
    .run(&documents, &mut work, workers, check)?;
    Ok(RulesBase {
        limit,
        source_set: source_set.map(str::to_owned),
        semantic,
        declarations,
        test_declarations,
        work_used: work.used,
        apps: apps.0,
        unread_manifests: apps.1,
    })
}

/// Layer and external policy over `refs`, joined with the prepared declaration rows of the
/// documents `paths` selects.
pub(crate) fn evaluate_prepared<'a>(
    config: &'a Config,
    base: &RulesBase,
    refs: &'a Projection,
    paths: &[String],
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Evaluation<'a>> {
    let filter = path_filter(config, paths)?;
    let source_set = base.source_set.as_deref();
    let semantic = base.semantic;
    let modules = &refs.modules;
    let layer_patterns: usize = config
        .layers
        .values()
        .map(|layer| layer.patterns.len())
        .sum();
    let mut work = Work::new(base.limit);
    ensure!(
        work.charge(base.work_used),
        "bounded_work: rules document budget"
    );
    ensure!(
        work.charge(modules.len().saturating_mul(layer_patterns)),
        "bounded_work: layer assignment pattern budget"
    );
    let scope = AppScope::new(config, base.apps());
    let mut matcher = LayerMatcher::new(config);
    let mut unassigned = Vec::new();
    let mut unassigned_files = 0usize;
    let mut assignments = BTreeMap::new();
    let mut module_by_id = BTreeMap::new();
    for module in modules {
        check()?;
        let path = module["identity"]["path"]
            .as_str()
            .context("store_unavailable: module path absent")?;
        let layer = matcher.select(path, &mut work, check)?;
        assignments.insert(path, layer);
        module_by_id.insert(module["identity"].to_string(), module);
        // Owner ruling 2026-10-31 (#559): the list follows the source filter; the total does not.
        // Destinations still resolve across the snapshot.
        if layer.is_none() {
            unassigned_files += 1;
            if glob_matches(&filter, path) {
                unassigned.push(module["identity"].clone());
            }
        }
    }
    let mut external_assignments = BTreeMap::new();
    let external_packages = refs.data["externalPackages"]
        .as_array()
        .context("store_unavailable: external package descriptors absent")?;
    for package in external_packages {
        check()?;
        let identity = &package["identity"];
        let selector = ExternalSelector::from_identity(identity)
            .context("store_unavailable: external package identity absent")?;
        ensure!(
            work.charge(config.layers.len()),
            "bounded_work: external layer assignment budget"
        );
        let layer = config
            .layers
            .iter()
            .find(|(_, layer)| layer.external.contains(&selector))
            .map(|(name, _)| name.as_str());
        external_assignments.insert(identity.to_string(), layer);
        if layer.is_none() {
            unassigned.push(identity.clone());
        }
    }
    let mut unresolved_positions = BTreeMap::new();
    let mut instances = Vec::new();
    let mut test_only = Vec::new();
    for (field, test) in [("production", false), ("tests", true)] {
        for edge in refs.data[field]
            .as_array()
            .context("store_unavailable: references edges absent")?
        {
            check()?;
            let source = &edge["source"];
            let target = &edge["target"];
            let path = source["path"]
                .as_str()
                .context("store_unavailable: edge source path absent")?;
            if !glob_matches(&filter, path) {
                continue;
            }
            let source_layer = assignments.get(path).copied().flatten();
            let external = ExternalSelector::from_identity(target);
            let target_layer = if external.is_some() {
                *external_assignments
                    .get(&target.to_string())
                    .context("store_unavailable: edge external package descriptor absent")?
            } else {
                assignments
                    .get(
                        target["path"]
                            .as_str()
                            .context("store_unavailable: edge target path absent")?,
                    )
                    .copied()
                    .flatten()
            };
            let denied = external
                .as_ref()
                .is_some_and(|s| config.external_deny.contains(s));
            if config.layers.is_empty() && !denied {
                continue;
            }
            let module = module_by_id
                .get(&source.to_string())
                .context("store_unavailable: edge source module absent")?;
            let sites = edge["sites"]
                .as_array()
                .context("store_unavailable: edge sites absent")?;
            ensure!(
                work.charge(1 + sites.len()),
                "bounded_work: rules reference evidence budget"
            );
            let mut evidence = Vec::new();
            for site in sites {
                check()?;
                evidence.push(reference_evidence(module, site));
            }
            let rows = if test { &mut test_only } else { &mut instances };
            if !config.layers.is_empty() {
                let (status, mut unresolved) =
                    layer_status(config, &scope, source_layer, target_layer);
                if test {
                    unresolved.push("test_only".into());
                }
                let mut row = json!({"rule":"$layers","target":source,"destination":target,"sourceLayer":source_layer,"destinationLayer":target_layer,"status":status,
                    "excepted":false,"exceptionReason":null,"constraints":[],"evidence":evidence,"unresolved":unresolved,"referenceCount":edge["count"],"kindCounts":edge["kindCounts"],"projection":edge["projection"]});
                ensure!(
                    work.charge(config.exceptions.len()),
                    "bounded_work: exception match budget"
                );
                decorate(config, source_set, &mut row);
                rows.push(row);
            }
            if denied {
                let unresolved = if test { vec!["test_only"] } else { vec![] };
                rows.push(json!({"rule":"$external","target":source,"destination":target,"sourceLayer":source_layer,"destinationLayer":target_layer,"status":"deviates",
                    "excepted":false,"exceptionReason":null,"constraints":[],"evidence":evidence,"unresolved":unresolved,"referenceCount":edge["count"],"kindCounts":edge["kindCounts"],"projection":edge["projection"]}));
            }
        }
    }
    if !config.layers.is_empty() {
        for unresolved in refs.data["unresolved"]
            .as_array()
            .context("store_unavailable: unresolved references absent")?
        {
            check()?;
            let source = &unresolved["source"];
            let site = &unresolved["site"];
            let path = source["path"]
                .as_str()
                .context("store_unavailable: unresolved source path absent")?;
            if !glob_matches(&filter, path) {
                continue;
            }
            let module = module_by_id
                .get(&source.to_string())
                .context("store_unavailable: unresolved source module absent")?;
            let test = site["role"] == "test";
            let mut reasons = vec![
                unresolved["reason"]
                    .as_str()
                    .unwrap_or("reference_unresolved")
                    .to_owned(),
            ];
            if test {
                reasons.push("test_only".into());
            }
            let row = json!({"rule":"$layers","target":source,"destination":null,"sourceLayer":assignments.get(path).copied().flatten(),"destinationLayer":null,"status":"unknown",
            "excepted":false,"exceptionReason":null,"constraints":[],"evidence":[reference_evidence(module,site)],"unresolved":reasons,"referenceCount":1,"kindCounts":unknown_kind_counts(site)});
            // One unresolved source instance keeps every unresolved observed site.
            let rows = if test { &mut test_only } else { &mut instances };
            let key = (test, source.to_string());
            if let Some(index) = unresolved_positions.get(&key).copied() {
                let existing: &mut Value = &mut rows[index];
                existing["referenceCount"] =
                    json!(existing["referenceCount"].as_u64().unwrap_or(0) + 1);
                if let Some(kind) = site["relationKind"].as_str() {
                    existing["kindCounts"][kind] =
                        json!(existing["kindCounts"][kind].as_u64().unwrap_or(0) + 1);
                }
                existing["evidence"]
                    .as_array_mut()
                    .expect("constructed evidence array")
                    .push(reference_evidence(module, site));
                let reasons = existing["unresolved"]
                    .as_array_mut()
                    .expect("constructed reason array");
                for reason in row["unresolved"].as_array().expect("constructed reasons") {
                    if !reasons.contains(reason) {
                        reasons.push(reason.clone());
                    }
                }
            } else {
                unresolved_positions.insert(key, rows.len());
                rows.push(row);
            }
        }
    }
    // Prepared rows carry their document in `target`.
    for (prepared, rows) in [
        (&base.declarations, &mut instances),
        (&base.test_declarations, &mut test_only),
    ] {
        for row in prepared {
            check()?;
            let path = row["target"]["path"]
                .as_str()
                .context("store_unavailable: rule target path absent")?;
            if glob_matches(&filter, path) {
                rows.push(row.clone());
            }
        }
    }
    let sort_rows = |rows: &mut Vec<Value>| {
        rows.sort_by_cached_key(|row| {
            (
                row["rule"].to_string(),
                row["target"].to_string(),
                row["destination"].to_string(),
            )
        })
    };
    sort_rows(&mut instances);
    sort_rows(&mut test_only);
    check()?;
    let external_packages = package_closure(
        external_packages,
        &instances,
        &test_only,
        &unassigned,
        &mut work,
        check,
    )?;
    check()?;
    let partial_files = &refs.data["partialFiles"];
    let mut limitations = limitations(semantic);
    if base.unread_manifests > 0 {
        let n = base.unread_manifests;
        limitations.push(format!(
            "{n} build manifest{} could not be read for app detection (malformed or over the size bound); they detect no app.",
            if n == 1 { "" } else { "s" }
        ));
    }
    let partial = partial_files.as_array().map_or(0, Vec::len);
    if partial > 0 {
        limitations.push(crate::syntax_bounds::limitation(partial));
    }
    let app_counts: Vec<Value> = base
        .apps()
        .iter()
        .enumerate()
        .map(|(index, app)| {
            let rows = instances
                .iter()
                .filter(|row| scope.bucket(&assignments, row) == Some(index));
            json!({"name":scope.name(index),"source":app.source,"manifest":app.manifest,"counts":counts(rows)})
        })
        .collect();
    let no_app = counts(
        instances
            .iter()
            .filter(|row| scope.bucket(&assignments, row).is_none()),
    );
    Ok(Evaluation {
        data: json!({"rulesVersion":"rules:v2","indexGeneration":refs.data["indexGeneration"],"indexRevision":refs.data["indexRevision"],"rulesFile":descriptor(config),
        "counts":counts(&instances),"apps":app_counts,"noApp":{"counts":no_app},"testCounts":counts(&test_only),"instances":instances,"testOnly":test_only,"unassigned":unassigned,"unassignedFileCount":unassigned_files,"externalPackages":external_packages,
        "partialFiles":partial_files,"limitations":limitations,"semanticDocuments":refs.data["semanticDocuments"],"partial":refs.data["partial"]}),
        layers: assignments,
    })
}
/// The evaluation's limitations: the bindings note says whether any were admitted (#523).
fn limitations(semantic: bool) -> Vec<String> {
    [
        "Layer results describe observed mapped references, not absence of dependencies.",
        if semantic {
            "Call and type constraints use selected producer bindings with their proof basis. Historical results are basis-P claims beside native N, never fresh or current absence proofs. Pending endpoints, unbound or dispatched calls and missing relationships remain unknown."
        } else {
            "Calls and type relationships have no admitted bindings and remain unknown."
        },
        "testOnly policy statuses are hypothetical and never production violations.",
        "Structural reference provenance attests captured source bytes, not a semantic target association; semantic sites carry their binding's provenance.",
    ]
    .map(str::to_owned)
    .to_vec()
}

pub fn explain(
    mut payload: Value,
    rule: &str,
    target: &Value,
    destination: Option<&Value>,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Value> {
    if let Some(destination) = destination.filter(|d| {
        d.get("kind").is_some() || d.get("package").is_some() || d.get("standardLibrary").is_some()
    }) {
        let identity: crate::references::external::ExternalIdentity =
            serde_json::from_value(destination.clone())
                .context("invalid_request: malformed external destination identity")?;
        ensure!(
            !identity.source_set_id.is_empty()
                && crate::references::external::valid_package(
                    &identity.language,
                    &identity.package,
                    identity.standard_library
                ),
            "invalid_request: malformed external destination identity"
        );
    }
    let mut found = false;
    if let Some(fields) = payload.as_object_mut() {
        fields.remove("apps");
        fields.remove("noApp");
    }
    for (field, count_field) in [("instances", "counts"), ("testOnly", "testCounts")] {
        let rows = payload[field]
            .as_array_mut()
            .context("store_unavailable: rules rows absent")?;
        rows.retain(|row| {
            row["rule"] == rule
                && row["target"] == *target
                && destination.map_or(row["destination"].is_null(), |d| row["destination"] == *d)
        });
        found |= !rows.is_empty();
        let count = counts(&*rows);
        payload[count_field] = count;
    }
    ensure!(found, "not_found: rule instance");
    // #912: the informational unassigned catalog keeps only the explained instances'
    // own ends (their source or destination). `unassignedFileCount` stays checkout-wide,
    // and `trellis_rules_check` lists the whole catalog.
    let ends: BTreeSet<String> = ["instances", "testOnly"]
        .into_iter()
        .flat_map(|field| payload[field].as_array().expect("checked rows"))
        .flat_map(|row| [row["target"].to_string(), row["destination"].to_string()])
        .collect();
    payload["unassigned"]
        .as_array_mut()
        .context("store_unavailable: unassigned catalog absent")?
        .retain(|identity| ends.contains(&identity.to_string()));
    payload["externalPackages"] = json!(package_closure(
        payload["externalPackages"]
            .as_array()
            .context("store_unavailable: external package descriptors absent")?,
        payload["instances"].as_array().expect("checked instances"),
        payload["testOnly"].as_array().expect("checked test rows"),
        payload["unassigned"]
            .as_array()
            .context("store_unavailable: unassigned catalog absent")?,
        // This is a subset of an admitted evaluation, whose request limit was <= this ceiling.
        &mut Work::new(work::RULES_CEILING),
        check,
    )?);
    Ok(payload)
}

#[cfg(test)]
mod specificity_tests {
    use super::*;

    #[test]
    fn global_minima_and_cached_comparisons_share_work_and_cancellation() {
        let config = parse("version=1\n[layers.alpha]\norder=1\npaths=['src/a*.rs']\n[layers.beta]\norder=2\npaths=['src/*a.rs']\n[layers.winner]\norder=3\npaths=['src/a*a.rs']\n").unwrap();
        let mut matcher = LayerMatcher::new(&config);
        let mut work = Work::new(work::rules_limit(0, 0));
        assert_eq!(
            matcher
                .select("src/aa.rs", &mut work, &mut || Ok(()))
                .unwrap(),
            Some("winner")
        );
        assert_eq!(matcher.relations.len(), 3);
        let compiled_work = work.used;
        assert_eq!(
            matcher
                .select("src/aaa.rs", &mut work, &mut || Ok(()))
                .unwrap(),
            Some("winner")
        );
        assert_eq!(work.used - compiled_work, 3);
        // The first three checks inspect matches; the fourth visits a cached pair.
        let mut checks = 0;
        let error = matcher
            .select("src/aa.rs", &mut work, &mut || {
                checks += 1;
                ensure!(checks < 4, "cancelled");
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
        let error = matcher
            .select("src/aa.rs", &mut Work::new(0), &mut || Ok(()))
            .unwrap_err();
        assert!(error.to_string().contains("bounded_work"));
    }
}

#[cfg(test)]
mod portable_exception_tests {
    use super::*;

    #[test]
    fn portable_scope_scan_is_cancellable_and_shares_existing_work_limit() {
        let config = parse("version=1\n[[exceptions]]\nrule='$layers'\ntarget={language='rust',path='a.rs'}\ndestination={language='rust',path='b.rs'}\nreason='Reviewed'").unwrap();
        let snapshot = Snapshot {
            pin: crate::model::IndexPin {
                index_generation: uuid::Uuid::nil(),
                index_revision: 1,
            },
            documents: Vec::new(),
            manifests: vec![crate::references::external::ManifestInput {
                source_set_id: "one".into(),
                native_revision_id: "revision".into(),
                dependency_hash: "hash".into(),
                path: "Cargo.toml".into(),
                content_hash: None,
                bytes: None,
            }],
            root_tsconfig: None,
            evidence_gaps: Default::default(),
        };
        let error = portable_source_set(
            &config,
            &snapshot,
            &mut Work::new(work::rules_limit(0, 0)),
            &mut || anyhow::bail!("cancelled"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
        let error =
            portable_source_set(&config, &snapshot, &mut Work::new(0), &mut || Ok(())).unwrap_err();
        assert!(error.to_string().contains("bounded_work"));
        let mut work = Work {
            used: 17,
            limit: work::rules_limit(0, 0),
        };
        assert_eq!(
            portable_source_set(&config, &snapshot, &mut work, &mut || Ok(())).unwrap(),
            Some("one")
        );
        assert_eq!(work.used, 18);
        let declarations_only = parse("version=1").unwrap();
        assert_eq!(
            portable_source_set(
                &declarations_only,
                &snapshot,
                &mut work,
                &mut || anyhow::bail!("should not scan")
            )
            .unwrap(),
            None
        );
        assert_eq!(work.used, 18);
    }
}

#[cfg(test)]
mod document_parallel_tests {
    use super::*;
    use crate::references::DocumentInput;
    use std::sync::{Arc, Mutex, atomic::AtomicBool};

    fn snapshot(files: &[(&str, &str)]) -> Snapshot {
        let root = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let identity =
            crate::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
                .unwrap();
        let (graph, native) = crate::indexer::index_workspace_with_native(
            &crate::indexer::IndexOptions::new(root.path().to_owned()),
            &identity.record_id,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let documents = graph
            .files
            .into_iter()
            .map(|file| {
                let key = native
                    .revision
                    .documents
                    .iter()
                    .find(|d| d.key.path == file.path)
                    .unwrap()
                    .key
                    .clone();
                let declarations = native
                    .declarations
                    .iter()
                    .filter(|d| d.document == key)
                    .cloned()
                    .collect();
                let calls = native
                    .calls
                    .iter()
                    .filter(|c| c.document == key)
                    .cloned()
                    .collect();
                DocumentInput {
                    semantic: Default::default(),
                    key,
                    file,
                    declarations,
                    calls,
                }
            })
            .collect();
        Snapshot {
            pin: crate::model::IndexPin {
                index_generation: uuid::Uuid::new_v4(),
                index_revision: 4,
            },
            documents,
            manifests: vec![],
            root_tsconfig: None,
            evidence_gaps: Default::default(),
        }
    }

    fn fixture() -> (Config, Snapshot) {
        let config = parse("version=1\n[roles.child]\nselect={name='Child',extends='Base'}\n[[rules]]\nid='inheritance'\nfor_each='child'\nrequire=[{calls='child'}]\n").unwrap();
        let snapshot = snapshot(&[
            ("A.java", "class Child extends Base { void run() {} }"),
            ("B.java", "class Child extends Base { void run() {} }"),
        ]);
        assert_eq!(snapshot.documents.len(), 2);
        (config, snapshot)
    }

    #[test]
    fn detached_rule_proof_retains_historical_basis_and_revision() {
        let metadata = BTreeMap::from([
            (
                "projection".into(),
                json!({"currentPin":{"indexRevision":4},"semanticBasisPin":{"indexRevision":3},"basisNativeRevisionId":"basis-native","semanticStatus":"olderBasis"}),
            ),
            ("historicalFreshness".into(), json!("fresh")),
            ("freshness".into(), json!("possiblyStale")),
            (
                "basis".into(),
                json!({"artifactHash":"artifact","revisionId":"capture-native"}),
            ),
        ]);
        let proof = semantic_evidence(
            json!({"revisionId":"current-native","kind":"semanticBinding"}),
            &metadata,
        );
        assert_eq!(proof["revisionId"], "basis-native");
        assert_eq!(proof["freshness"], "possiblyStale");
        assert_eq!(proof["historicalFreshness"], "fresh");
        assert_eq!(proof["basis"]["revisionId"], "capture-native");
    }

    #[test]
    fn historical_complete_coverage_never_proves_forbidden_call_absence() {
        let config = parse("version=1\n[roles.all]\nselect={name='*'}\n[[rules]]\nid='no-calls'\nfor_each='all'\nforbid=[{calls='all'}]\n").unwrap();
        let mut snapshot = snapshot(&[("src/lib.rs", "pub fn run() {}")]);
        let doc = &mut snapshot.documents[0];
        doc.semantic.producer_id = Some("producer".into());
        doc.semantic.complete = true;
        doc.semantic.projection = Some(crate::model::SemanticProjection {
            current_pin: snapshot.pin,
            semantic_basis_pin: crate::model::IndexPin {
                index_revision: snapshot.pin.index_revision - 1,
                ..snapshot.pin
            },
            basis_native_revision_id: "basis-native".into(),
            semantic_status: crate::model::SemanticStatus::OlderBasis,
        });
        let base = prepare(&config, &snapshot, &mut || Ok(())).unwrap();
        assert!(!base.declarations.is_empty());
        for row in &base.declarations {
            assert_eq!(row["status"], "unknown");
            assert_eq!(
                row["constraints"][0]["unresolved"][0],
                "historical_semantic_absence_unproven"
            );
            assert_eq!(
                row["constraints"][0]["projection"]["semanticStatus"],
                "olderBasis"
            );
        }
    }

    /// #556: rows prepared for every document select what a scoped evaluation returns.
    #[test]
    fn prepared_rows_select_what_a_scoped_evaluation_returns() {
        let snapshot = snapshot(&[
            ("src/lib.rs", "mod a; mod b;"),
            ("src/a.rs", "pub fn a() {}"),
            ("src/b.rs", "pub fn b() { crate::a::a(); }"),
        ]);
        let config = parse(
            "version=1\n[layers.high]\norder=1\npaths=['src/b.rs']\n\
             [layers.low]\norder=2\npaths=['src/a.rs']\n\
             [roles.handler]\nselect={paths=['src/**'],name='*'}\n\
             [roles.target]\nselect={paths=['src/a.rs']}\n\
             [[rules]]\nid='calls'\nfor_each='handler'\nrequire=[{calls='target'}]\n",
        )
        .unwrap();
        let prepared = prepare(&config, &snapshot, &mut || Ok(())).unwrap();
        assert!(!prepared.declarations.is_empty());
        for paths in [
            vec![],
            vec!["src/a.rs".to_owned()],
            vec!["src/b.rs".to_owned()],
            vec!["missing/**".to_owned()],
        ] {
            let refs = crate::references::project(&snapshot, &paths, 5, &mut || Ok(())).unwrap();
            let scoped = evaluate(&config, &snapshot, &refs, &paths, &mut || Ok(())).unwrap();
            let selected = evaluate_prepared(&config, &prepared, &refs, &paths, &mut || Ok(()))
                .unwrap()
                .data;
            assert_eq!(scoped, selected, "{paths:?}");
        }
    }

    #[test]
    fn document_work_overlaps_but_rows_and_counts_are_admitted_in_order() {
        let (config, snapshot) = fixture();
        let documents: Vec<_> = snapshot.documents.iter().collect();
        let targets = Targets::new();
        let evaluation = DocumentEvaluation {
            config: &config,
            targets: &targets,
            inheritance: true,
            source_set: None,
            before_document: None,
        };
        let mut serial_work = Work::new(work::rules_limit(2, 0));
        let serial = evaluation
            .run(&documents, &mut serial_work, 1, &mut || Ok(()))
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let rx = Mutex::new(rx);
        let first = &documents[0].key.path;
        let mut parallel_work = Work::new(serial_work.limit);
        let hook = |doc: &DocumentInput| {
            if &doc.key.path == first {
                rx.lock().unwrap().recv().unwrap();
            } else {
                tx.send(()).unwrap();
            }
        };
        let parallel = DocumentEvaluation {
            before_document: Some(&hook),
            ..evaluation
        }
        .run(&documents, &mut parallel_work, 4, &mut || Ok(()))
        .unwrap();
        assert!(!serial.0.is_empty());
        assert_eq!(
            serde_json::to_vec(&serial).unwrap(),
            serde_json::to_vec(&parallel).unwrap()
        );
        assert_eq!(serial_work.used, parallel_work.used);
        assert!(serial_work.used > 0);

        // Each file fits locally, but the sum exceeds the single request allowance.
        let per_file = evaluate_document(
            &config,
            &targets,
            documents[0],
            true,
            None,
            serial_work.limit,
            &mut || Ok(()),
        )
        .unwrap();
        assert!(per_file.work.used > 0 && per_file.work.used < serial_work.used);
        for workers in [1, 4] {
            let mut work = Work::new(serial_work.used - 1);
            let error = evaluation
                .run(&documents, &mut work, workers, &mut || Ok(()))
                .unwrap_err();
            assert!(error.to_string().starts_with("bounded_work:"));
        }
    }

    #[derive(Debug)]
    struct Deadline;
    impl std::fmt::Display for Deadline {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("deadline_exceeded")
        }
    }
    impl std::error::Error for Deadline {}

    #[test]
    fn worker_callback_error_is_preserved_without_rows() {
        let (config, snapshot) = fixture();
        let documents: Vec<_> = snapshot.documents.iter().collect();
        let targets = Targets::new();
        let evaluation = DocumentEvaluation {
            config: &config,
            targets: &targets,
            inheritance: true,
            source_set: None,
            before_document: None,
        };
        let stopped = AtomicBool::new(false);
        let mut callback = || {
            if stopped.load(std::sync::atomic::Ordering::Acquire) {
                Err(Deadline.into())
            } else {
                Ok(())
            }
        };
        let hook = |_: &DocumentInput| {
            stopped.store(true, std::sync::atomic::Ordering::Release);
        };
        let error = DocumentEvaluation {
            before_document: Some(&hook),
            ..evaluation
        }
        .run(
            &documents,
            &mut Work::new(work::rules_limit(2, 0)),
            2,
            &mut callback,
        )
        .unwrap_err();
        assert!(error.downcast_ref::<Deadline>().is_some());
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;

    #[test]
    fn layer_display_hint_is_optional_and_passed_to_the_browser_inventory() {
        let config = parse("version=1\n[layers.a]\norder=1\npaths=['a/**']\ndisplay='expanded'\n[layers.b]\norder=2\npaths=['b/**']\ndisplay='collapsed'\n[layers.c]\nshared=true\npaths=['c/**']\ndisplay='hidden'\n[layers.d]\norder=3\npaths=['d/**']\n").unwrap();
        let layers = view_layers(&config);
        let display: Vec<_> = layers.iter().map(|l| l["display"].clone()).collect();
        assert_eq!(
            display,
            [
                json!("expanded"),
                json!("collapsed"),
                json!("hidden"),
                Value::Null
            ]
        );
    }

    #[test]
    fn view_layers_carry_the_authored_app_and_apps_or_null() {
        let config = parse("version=1\n[layers.a]\napp='svc'\norder=1\npaths=['a/**']\n[layers.b]\napps=['svc','ui']\nshared=true\npaths=['b/**']\n[layers.c]\norder=2\npaths=['c/**']\n").unwrap();
        let layers = view_layers(&config);
        let field = |key: &str| -> Vec<Value> { layers.iter().map(|l| l[key].clone()).collect() };
        assert_eq!(field("app"), [json!("svc"), Value::Null, Value::Null]);
        assert_eq!(
            field("apps"),
            [Value::Null, json!(["svc", "ui"]), Value::Null]
        );
    }

    #[test]
    fn layer_display_hint_leaves_assignment_inputs_unchanged() {
        let plain = parse("version=1\n[layers.a]\norder=1\npaths=['a/**']\n").unwrap();
        let hinted =
            parse("version=1\n[layers.a]\norder=1\npaths=['a/**']\ndisplay='hidden'\n").unwrap();
        let shape = |c: &Config| {
            c.layers
                .iter()
                .map(|(n, l)| (n.clone(), l.order, l.patterns.len(), l.external.len()))
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&plain), shape(&hinted));
    }

    #[test]
    fn unknown_layer_display_value_is_a_located_typed_error() {
        let text = "version=1\n[layers.a]\norder=1\npaths=['a/**']\ndisplay='folded'\n";
        let Err(error) = parse(text) else {
            panic!("unknown display value must be refused")
        };
        let found = diagnostic(&error).expect("typed rules diagnostic");
        assert_eq!(found["code"], "invalid_rules");
        assert_eq!(found["kind"], "schema");
        assert_eq!(found["line"], 5);
        assert_eq!(
            found["message"],
            "layer display must be expanded, collapsed or hidden"
        );
        let Err(error) = parse("version=1\n[layers.a]\norder=1\npaths=['a/**']\ndisplay=true\n")
        else {
            panic!("non-string display value must be refused")
        };
        assert_eq!(diagnostic(&error).expect("typed")["code"], "invalid_rules");
    }
}

#[cfg(test)]
mod app_scope_tests {
    use super::*;
    use crate::apps::{App, AppKind, Candidates, Outcome};

    fn app(name: &str, kind: AppKind, root: &str) -> App {
        App::detected(name.into(), kind, &format!("{root}/manifest"), vec![])
    }

    const TWO_APPS: &str = "version=1\n[layers.up]\napp='svc'\norder=1\npaths=['svc/**']\n[layers.web]\napp='ui'\norder=1\npaths=['ui/**']\n[layers.base]\nshared=true\napps=['svc']\npaths=['base/**']\n";

    #[test]
    fn a_python_app_validates() {
        let config = parse(TWO_APPS).unwrap();
        let apps = [
            app("svc", AppKind::Python, "svc"),
            app("ui", AppKind::Npm, "ui"),
        ];
        validate_apps(&config, &apps).unwrap();
    }

    #[test]
    fn scope_layer_share_and_file() {
        let config = parse(TWO_APPS).unwrap();
        let apps = [
            app("svc", AppKind::Cargo, "svc"),
            app("ui", AppKind::Npm, "ui"),
        ];
        let scope = AppScope::new(&config, &apps);
        assert_eq!(scope.layer("up"), BTreeSet::from([0]));
        assert_eq!(scope.layer("base"), BTreeSet::from([0]));
        assert!(scope.share("up", "base"));
        assert!(!scope.share("web", "base"));
        assert!(!scope.share("up", "web"));
        assert_eq!(scope.file("ui/x.js", Some("up")), Some(0));
        assert_eq!(scope.file("ui/x.js", None), Some(1));
        assert_eq!(scope.file("other/x", None), None);
        let none = AppScope::new(&config, &[]);
        assert_eq!(none.file("ui/x.js", None), None);
        assert!(none.share("up", "web"));
    }

    #[test]
    fn a_repeated_app_is_located_at_the_repeat() {
        let text = "version=1\n[layers.s]\nshared=true\napps=[\n'x',\n'x'\n]\npaths=['a']\n";
        let d = diagnostic(&parse(text).err().unwrap()).unwrap();
        assert_eq!(d["kind"], "schema");
        assert_eq!(d["line"], 6);
    }

    #[test]
    fn too_many_apps_are_too_large_at_the_extra_entry() {
        let entries: String = (0..=COLLECTION_LIMIT)
            .map(|i| format!("'a{i}',\n"))
            .collect();
        let text = format!("version=1\n[layers.s]\nshared=true\napps=[\n{entries}]\npaths=['a']\n");
        let d = diagnostic(&parse(&text).err().unwrap()).unwrap();
        assert_eq!(d["kind"], "too_large");
        assert_eq!(d["line"], 5 + COLLECTION_LIMIT);
    }

    /// #855: a wrapper's `app` names an effective app. A known app validates; an unknown or
    /// shared name is a located error at the `app` value, checked where apps are resolved.
    #[test]
    fn a_wrapper_app_names_an_effective_app() {
        let apps = [
            app("svc", AppKind::Cargo, "svc"),
            app("ui", AppKind::Npm, "ui"),
        ];
        let known = parse(&format!(
            "{TWO_APPS}[[interfaces.wrappers]]\ncallee='api2'\napp='ui'\n"
        ))
        .unwrap();
        assert_eq!(
            known.interface_wrappers()[0]
                .app
                .as_ref()
                .map(|(name, _)| name.as_str()),
            Some("ui")
        );
        assert_eq!(known.interface_wrappers()[0].root, ".");
        validate_apps(&known, &apps).unwrap();
        let unknown = parse(&format!(
            "{TWO_APPS}[[interfaces.wrappers]]\ncallee='api2'\napp='admin'\n"
        ))
        .unwrap();
        let d = diagnostic(&validate_apps(&unknown, &apps).unwrap_err()).unwrap();
        assert_eq!(d["kind"], "reference");
        assert_eq!(d["line"], 16, "{d}");
        assert!(
            d["message"]
                .as_str()
                .unwrap()
                .contains("names app admin, which is not detected or authored (apps: svc, ui)"),
            "{d}"
        );
        let twins = [
            app("ui", AppKind::Npm, "ui"),
            app("ui", AppKind::Npm, "other"),
            app("svc", AppKind::Cargo, "svc"),
        ];
        let d = diagnostic(&validate_apps(&known, &twins).unwrap_err()).unwrap();
        assert_eq!(d["kind"], "ambiguity", "{d}");
    }

    /// #855 review: assigning files to apps for app-scoped wrappers charges every path's scan
    /// of the layer patterns, so a budget below paths x patterns fails typed `bounded_work`.
    #[test]
    fn file_apps_charges_each_path_pattern_scan() {
        let config = parse(TWO_APPS).unwrap();
        let base = RulesBase {
            apps: vec![
                app("svc", AppKind::Cargo, "svc"),
                app("ui", AppKind::Npm, "ui"),
            ],
            limit: 0,
            source_set: None,
            semantic: false,
            declarations: vec![],
            test_declarations: vec![],
            work_used: 0,
            unread_manifests: 0,
        };
        let paths = ["ui/a.js", "svc/b.js", "other/c.js"];
        let scans = paths.len() * 3;
        for limit in [0, scans - 1] {
            let error = file_apps(&config, &base, paths, &mut Work::new(limit), &mut || Ok(()))
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "bounded_work: layer assignment pattern budget",
                "{limit}"
            );
        }
        let mut work = Work::new(scans);
        let assigned = file_apps(&config, &base, paths, &mut work, &mut || Ok(())).unwrap();
        assert_eq!(assigned.get("ui/a.js").map(String::as_str), Some("ui"));
        assert_eq!(assigned.get("svc/b.js").map(String::as_str), Some("svc"));
        assert_eq!(work.used, scans);
    }

    /// #855: `root` and `app` are exclusive, a blank app is invalid, and a callee is authored
    /// once per app.
    #[test]
    fn a_wrapper_scope_is_root_or_app() {
        for (body, kind, line, needle) in [
            (
                "[[interfaces.wrappers]]\ncallee='a'\nroot='web'\napp='web'\n",
                "schema",
                5,
                "either root or app",
            ),
            (
                "[[interfaces.wrappers]]\ncallee='a'\napp=' '\n",
                "schema",
                4,
                "blank",
            ),
            (
                "[[interfaces.wrappers]]\ncallee='a'\napp='web'\n[[interfaces.wrappers]]\ncallee='a'\napp='web'\n",
                "ambiguity",
                6,
                "twice for app web",
            ),
        ] {
            let d = located(&format!("version=1\n{body}"));
            assert_eq!(d["kind"], kind, "{body}: {d}");
            assert_eq!(d["line"], line, "{body}: {d}");
            assert!(
                d["message"].as_str().unwrap().contains(needle),
                "{body}: {d}"
            );
        }
        parse("version=1\n[[interfaces.wrappers]]\ncallee='a'\napp='web'\n[[interfaces.wrappers]]\ncallee='a'\nroot='web'\n[[interfaces.wrappers]]\ncallee='a'\n").unwrap();
    }

    #[test]
    fn the_first_bad_layer_in_name_order_wins() {
        let text = "version=1\n[layers.zeta]\napp='nope'\norder=1\npaths=['z']\n[layers.alpha]\napp='nada'\norder=2\npaths=['a']\n";
        let config = parse(text).unwrap();
        let apps = [app("svc", AppKind::Cargo, "svc")];
        let d = diagnostic(&validate_apps(&config, &apps).unwrap_err()).unwrap();
        assert_eq!(d["kind"], "reference");
        assert_eq!(d["line"], 7);
    }

    #[test]
    fn scope_names_the_app_of_a_file_index() {
        let config = parse(TWO_APPS).unwrap();
        let apps = [
            app("svc", AppKind::Cargo, "svc"),
            app("ui", AppKind::Npm, "ui"),
        ];
        let scope = AppScope::new(&config, &apps);
        let index = scope.file("ui/x.js", None).unwrap();
        assert_eq!(scope.name(index), Some("ui"));
        assert_eq!(scope.name(2), None);
    }

    #[test]
    fn app_keys_are_checked_syntactically() {
        for (extra, line) in [
            ("[layers.s]\nshared=true\napp='x'\npaths=['a']\n", 4),
            ("[layers.s]\norder=1\napps=['x']\npaths=['a']\n", 4),
            ("[layers.s]\norder=1\napp=' '\npaths=['a']\n", 4),
            ("[layers.s]\nshared=true\napps=[]\npaths=['a']\n", 4),
        ] {
            let error = parse(&format!("version=1\n{extra}")).err().unwrap();
            let d = diagnostic(&error).unwrap();
            assert_eq!(d["kind"], "schema", "{extra}");
            assert_eq!(d["line"], line, "{extra}");
        }
    }

    fn located(text: &str) -> Value {
        diagnostic(&parse(text).err().expect("invalid rules")).unwrap()
    }

    /// #855: `[[interfaces.wrappers]]` parse in written order, with their defaults and an
    /// optional root.
    #[test]
    fn interface_wrappers_parse_with_defaults_and_roots() {
        let config = parse(
            "version=1\n[[interfaces.wrappers]]\ncallee='apiFetch'\nprefix='/api'\nroot='web'\n\
             [[interfaces.wrappers]]\ncallee='client.send'\nmethod='argument'\nat=2\n",
        )
        .unwrap();
        assert_eq!(
            config.interface_wrappers(),
            [
                InterfaceWrapper {
                    callee: "apiFetch".into(),
                    method: WrapperMethod::Options(1),
                    prefix: Some("/api".into()),
                    root: "web".into(),
                    app: None,
                },
                InterfaceWrapper {
                    callee: "client.send".into(),
                    method: WrapperMethod::Argument(2),
                    prefix: None,
                    root: ".".into(),
                    app: None,
                },
            ]
        );
        // The same callee may be authored once per root.
        parse("version=1\n[[interfaces.wrappers]]\ncallee='api2'\n[[interfaces.wrappers]]\ncallee='api2'\nroot='web'\n").unwrap();
        assert!(
            parse("version=1\n")
                .unwrap()
                .interface_wrappers()
                .is_empty()
        );
        assert!(
            parse("version=1\n[interfaces]\n")
                .unwrap()
                .interface_wrappers()
                .is_empty()
        );
    }

    /// #855: an invalid wrapper is a located rules error, never a silent omission.
    #[test]
    fn interface_wrapper_errors_are_located() {
        for (body, kind, line, needle) in [
            (
                "[[interfaces.wrappers]]\nprefix='/api'\n",
                "schema",
                2,
                "requires callee",
            ),
            (
                "[[interfaces.wrappers]]\ncallee='a.b.c'\n",
                "schema",
                3,
                "identifier",
            ),
            (
                "[[interfaces.wrappers]]\ncallee='9a'\n",
                "schema",
                3,
                "identifier",
            ),
            (
                "[[interfaces.wrappers]]\ncallee='a'\nmethod='body'\n",
                "schema",
                4,
                "options",
            ),
            (
                "[[interfaces.wrappers]]\ncallee='a'\nat=0\n",
                "schema",
                4,
                "1 to 8",
            ),
            (
                "[[interfaces.wrappers]]\ncallee='a'\nat=9\n",
                "schema",
                4,
                "1 to 8",
            ),
            (
                "[[interfaces.wrappers]]\ncallee='a'\nprefix='api'\n",
                "schema",
                4,
                "prefix",
            ),
            (
                "[[interfaces.wrappers]]\ncallee='a'\nprefix='/a?b'\n",
                "schema",
                4,
                "prefix",
            ),
            (
                "[[interfaces.wrappers]]\ncallee='a'\nroot='../web'\n",
                "schema",
                4,
                "wrapper root",
            ),
            (
                "[[interfaces.wrappers]]\ncallee='a'\nkind='x'\n",
                "schema",
                4,
                "kind",
            ),
            ("[interfaces]\ncallees=['a']\n", "schema", 3, "callees"),
            (
                "[[interfaces.wrappers]]\ncallee='a'\n[[interfaces.wrappers]]\ncallee='a'\n",
                "ambiguity",
                5,
                "twice",
            ),
        ] {
            let d = located(&format!("version=1\n{body}"));
            assert_eq!(d["kind"], kind, "{body}: {d}");
            assert_eq!(d["line"], line, "{body}: {d}");
            assert!(
                d["message"].as_str().unwrap().contains(needle),
                "{body}: {d}"
            );
        }
        let many: String = (0..65)
            .map(|at| format!("[[interfaces.wrappers]]\ncallee='w{at}'\n"))
            .collect();
        assert_eq!(located(&format!("version=1\n{many}"))["kind"], "too_large");
    }

    /// #794: `[apps.<name>]` tables parse in name order, each with its roots.
    #[test]
    fn authored_apps_parse_in_name_order() {
        let config = parse(
            "version=1\n[apps.web]\nroots=['client', 'shared/ui']\n[apps.backend]\nroots=['.']\n",
        )
        .unwrap();
        assert_eq!(config.authored_app_count(), 2);
        let apps = config.authored_apps();
        assert_eq!(apps[0].name, "backend");
        assert_eq!(apps[0].roots, ["."]);
        assert_eq!(apps[1].name, "web");
        assert_eq!(apps[1].roots, ["client", "shared/ui"]);
        assert_eq!(apps[1].source, crate::apps::AppSource::Override);
        assert_eq!(parse("version=1\n").unwrap().authored_app_count(), 0);
    }

    /// #794: each schema error of an authored app is located at its token.
    #[test]
    fn authored_app_schema_errors_are_located() {
        for (body, line, needle) in [
            ("[apps.api]\n", 2, "requires roots"),
            ("[apps.api]\nroots=[]\n", 3, "at least one root"),
            ("[apps.api]\nroots='server'\n", 3, ""),
            ("[apps.api]\nroots=[\n1\n]\n", 4, ""),
            ("[apps.api]\nroots=['server']\nkind='npm'\n", 4, "kind"),
            (
                "[apps.api]\nroots=['server']\nentries=['main']\n",
                4,
                "entries",
            ),
            ("[apps.\" \"]\nroots=['server']\n", 2, "blank"),
            ("[apps.api]\nroots=[\n'server',\n'server'\n]\n", 5, "twice"),
        ] {
            let d = located(&format!("version=1\n{body}"));
            assert_eq!(d["kind"], "schema", "{body}");
            assert_eq!(d["line"], line, "{body}: {d}");
            assert!(
                d["message"].as_str().unwrap().contains(needle),
                "{body}: {d}"
            );
        }
    }

    /// #794: a root must be `.` or a literal checkout-relative directory; one outside the
    /// checkout, absolute, globbed or unnormalised is a located schema error.
    #[test]
    fn an_invalid_root_is_a_schema_error_at_the_root() {
        for root in [
            "", "/srv", "srv/", "../srv", "srv/../x", "./srv", "srv/./x", "a//b", "srv/*",
            "srv/**", "s?v", "[s]rv", "{a,b}", "!srv", "srv\\\\x", "c:/srv", "..",
        ] {
            let text = format!(
                "version=1\n[apps.api]\nroots=[\n'ok',\n{}\n]\n",
                toml::Value::String(root.into())
            );
            let d = located(&text);
            assert_eq!(d["kind"], "schema", "{root:?}");
            assert_eq!(d["line"], 5, "{root:?}");
            assert!(
                d["message"].as_str().unwrap().contains("app root"),
                "{root:?}"
            );
        }
    }

    /// #794 D3: the same root in two authored apps is an ambiguity at its second occurrence;
    /// nested roots are allowed.
    #[test]
    fn an_identical_root_in_two_apps_is_an_ambiguity_and_nesting_is_not() {
        let d =
            located("version=1\n[apps.api]\nroots=['srv']\n[apps.web]\nroots=[\n'ui',\n'srv'\n]\n");
        assert_eq!(d["kind"], "ambiguity");
        assert_eq!(d["line"], 7);
        assert!(
            d["message"].as_str().unwrap().contains("api and web"),
            "{d}"
        );
        parse("version=1\n[apps.api]\nroots=['.']\n[apps.web]\nroots=['ui', 'ui/inner']\n[apps.inner]\nroots=['ui/inner/x']\n").unwrap();
    }

    #[test]
    fn too_many_authored_apps_or_roots_are_too_large() {
        let roots: String = (0..=COLLECTION_LIMIT)
            .map(|i| format!("'r{i}',\n"))
            .collect();
        let d = located(&format!("version=1\n[apps.api]\nroots=[\n{roots}]\n"));
        assert_eq!(d["kind"], "too_large");
        assert_eq!(d["line"], 4 + COLLECTION_LIMIT);
        let apps: String = (0..=COLLECTION_LIMIT)
            .map(|i| format!("[apps.a{i}]\nroots=['r{i}']\n"))
            .collect();
        let d = located(&format!("version=1\n{apps}"));
        assert_eq!(d["kind"], "too_large");
    }

    fn documents<'a>(paths: &[&'a str]) -> BTreeSet<&'a str> {
        paths.iter().copied().collect()
    }

    /// #794 D6: a root with no indexed document under it is a located reference error.
    #[test]
    fn an_authored_root_without_documents_is_a_reference_error() {
        let config = parse("version=1\n[apps.api]\nroots=[\n'server',\n'gone'\n]\n").unwrap();
        let d = diagnostic(
            &effective_apps(&config, vec![], &documents(&["server/a.js", "gonex/b.js"]))
                .unwrap_err(),
        )
        .unwrap();
        assert_eq!(d["kind"], "reference");
        assert_eq!(d["line"], 5);
        assert!(d["message"].as_str().unwrap().contains("gone"), "{d}");
        let apps =
            effective_apps(&config, vec![], &documents(&["server/a.js", "gone/b.js"])).unwrap();
        assert_eq!(apps.len(), 1);
        let root = parse("version=1\n[apps.all]\nroots=['.']\n").unwrap();
        assert!(effective_apps(&root, vec![], &documents(&[])).is_err());
        effective_apps(&root, vec![], &documents(&["a.rs"])).unwrap();
    }

    /// #794 D4: an authored name equal to a detected app that survives is an ambiguity at the
    /// authored table; the same name on a shadowed app is a rename.
    #[test]
    fn an_authored_name_clashing_with_a_surviving_detected_app_is_an_ambiguity() {
        let detected = vec![
            app("svc", AppKind::Cargo, "svc"),
            app("ui", AppKind::Npm, "ui"),
        ];
        let docs = documents(&["svc/main.rs", "ui/a.js", "cli/a.rs"]);
        let clash = parse("version=1\n[apps.svc]\nroots=['cli']\n").unwrap();
        let d = diagnostic(&effective_apps(&clash, detected.clone(), &docs).unwrap_err()).unwrap();
        assert_eq!(d["kind"], "ambiguity");
        assert_eq!(d["line"], 2);
        assert!(
            d["message"].as_str().unwrap().contains("svc/manifest"),
            "{d}"
        );
        let rename =
            parse("version=1\n[apps.service]\nroots=['svc']\n[apps.ui]\nroots=['ui']\n").unwrap();
        let apps = effective_apps(&rename, detected, &docs).unwrap();
        let names: Vec<&str> = apps.iter().map(|app| app.name.as_str()).collect();
        assert_eq!(names, ["service", "ui"]);
        assert!(
            apps.iter()
                .all(|app| app.source == crate::apps::AppSource::Override)
        );
    }

    /// #794: a layer names an effective app; a detected app an authored root shadows can no
    /// longer be named.
    #[test]
    fn layers_name_effective_apps_only() {
        let text = "version=1\n[apps.service]\nroots=['svc']\n[layers.up]\napp='svc'\norder=1\npaths=['svc/**']\n[layers.web]\napp='ui'\norder=1\npaths=['ui/**']\n";
        let config = parse(text).unwrap();
        let detected = vec![
            app("svc", AppKind::Cargo, "svc"),
            app("ui", AppKind::Npm, "ui"),
        ];
        let apps =
            effective_apps(&config, detected, &documents(&["svc/main.rs", "ui/a.js"])).unwrap();
        let d = diagnostic(&validate_apps(&config, &apps).unwrap_err()).unwrap();
        assert_eq!(d["kind"], "reference");
        assert_eq!(d["line"], 5);
        assert!(
            d["message"].as_str().unwrap().contains("service, ui"),
            "{d}"
        );
    }

    /// #794: with a rules file the prompt lists the effective inventory, unchecked against
    /// the snapshot.
    #[test]
    fn prompt_apps_list_authored_apps_first_and_drop_shadowed_ones() {
        let candidates = Candidates {
            manifests: vec![
                candidate("svc", AppKind::Cargo, "svc/Cargo.toml"),
                candidate("ui", AppKind::Npm, "ui/package.json"),
            ],
        };
        let config = parse("version=1\n[apps.web]\nroots=['ui', 'gone']\n").unwrap();
        let rows = prompt_apps(Some(&config), &candidates, &mut || Ok(())).unwrap();
        assert_eq!(
            rows,
            vec![
                json!({"name":"web","source":"override","kind":null,"manifest":null,"roots":["ui","gone"],"entries":[]}),
                json!({"name":"svc","source":"manifest","kind":"cargo","manifest":"svc/Cargo.toml","roots":["svc"],"entries":["src/main.rs"]}),
            ]
        );
    }

    fn candidate(name: &str, kind: AppKind, manifest: &str) -> (String, Outcome) {
        let app = App::detected(name.into(), kind, manifest, vec!["src/main.rs".into()]);
        (manifest.into(), Outcome::App(app))
    }

    #[test]
    fn prompt_apps_list_every_manifest_without_rules() {
        let candidates = Candidates {
            manifests: vec![
                candidate("svc", AppKind::Cargo, "svc/Cargo.toml"),
                ("tools/package.json".into(), Outcome::NotApp),
                candidate("ui", AppKind::Npm, "ui/package.json"),
                ("bad/package.json".into(), Outcome::Unread),
            ],
        };
        let rows = prompt_apps(None, &candidates, &mut || Ok(())).unwrap();
        assert_eq!(
            rows,
            vec![
                json!({"name":"svc","source":"manifest","kind":"cargo","manifest":"svc/Cargo.toml","roots":["svc"],"entries":["src/main.rs"]}),
                json!({"name":"ui","source":"manifest","kind":"npm","manifest":"ui/package.json","roots":["ui"],"entries":["src/main.rs"]}),
            ]
        );
    }

    #[test]
    fn prompt_apps_leave_out_a_manifest_a_hidden_layer_selects() {
        let candidates = Candidates {
            manifests: vec![
                candidate("svc", AppKind::Cargo, "svc/Cargo.toml"),
                candidate("fixture", AppKind::Npm, "fixtures/package.json"),
            ],
        };
        let config =
            parse("version=1\n[layers.fix]\norder=1\ndisplay='hidden'\npaths=['fixtures/**']\n")
                .unwrap();
        let rows = prompt_apps(Some(&config), &candidates, &mut || Ok(())).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "svc");
    }
}
