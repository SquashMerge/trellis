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
const AST_VISITS: usize = 1_000_000;
const AST_DEPTH: usize = 128;

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
}

impl Config {
    /// Authored `[[exceptions]]` entries, as the rules inventory lists them.
    pub fn exception_count(&self) -> usize {
        self.exceptions.len()
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
fn languages(s: &str) -> bool {
    matches!(s, "java" | "rust" | "python" | "javascript")
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
                patterns: globs,
                external,
                span,
            },
        );
    }
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
    })
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
        json!({"name": name, "order": layer.order, "shared": layer.order.is_none(), "display": layer.display})
    }).collect()
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
            && config.layers[layer].display.as_deref() == Some("hidden")
        {
            hidden.insert((*path).to_owned());
        }
    }
    Ok(hidden)
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

fn layer_status(
    config: &Config,
    source: Option<&str>,
    target: Option<&str>,
) -> (&'static str, Vec<String>) {
    let (Some(s), Some(t)) = (source, target) else {
        return ("unknown", vec!["layer_unassigned".into()]);
    };
    let from = &config.layers[s];
    let to = &config.layers[t];
    if s == t || to.order.is_none() {
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
fn counts(rows: &[Value]) -> Value {
    let mut conforms = 0;
    let mut deviates = 0;
    let mut unknown = 0;
    let mut excepted = 0;
    for row in rows {
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
    json!({"comparable":rows.len(),"conforms":conforms,"deviates":deviates,"unknown":unknown,"excepted":excepted})
}
fn reference_evidence(module: &Value, site: &Value) -> Value {
    json!({"document":{"sourceSetId":module["identity"]["sourceSetId"],"language":module["identity"]["language"],"path":module["identity"]["path"]},
        "revisionId":module["revisionId"],"range":site["range"],"kind":"structuralReference","syntaxId":site["ownerSyntaxId"],"callId":site["callId"],"provenanceId":module["provenanceId"]})
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
    let mut stack = vec![(tree.root_node(), 0usize)];
    let mut visits = 0;
    let mut types = BTreeMap::new();
    while let Some((node, depth)) = stack.pop() {
        check()?;
        visits += 1;
        ensure!(
            visits <= AST_VISITS && depth <= AST_DEPTH,
            "bounded_work: selector AST budget"
        );
        ensure!(
            !node.is_error() && !node.is_missing(),
            "invalid_source_syntax"
        );
        if matches!(
            node.kind(),
            "class_declaration"
                | "class_definition"
                | "interface_declaration"
                | "impl_item"
                | "trait_item"
                | "enum_declaration"
                | "record_declaration"
        ) {
            types.insert((node.start_byte(), node.end_byte()), node);
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor).map(|n| (n, depth + 1)));
    }
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
        let node = types.get(&(d.range.start, d.range.end)).copied();
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
struct DocumentEvaluation<'a> {
    config: &'a Config,
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
    doc: &crate::references::DocumentInput,
    inheritance: bool,
    source_set: Option<&str>,
    limit: usize,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<DocumentRows> {
    let mut work = Work::new(limit);
    let mut instances = Vec::new();
    let mut test_only = Vec::new();
    let bases = if inheritance {
        parse_bases(doc, check)?
    } else {
        BTreeMap::new()
    };
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
            for c in &rule.constraints {
                ensure!(
                    work.charge(
                        1 + if c.kind == "calls" {
                            doc.calls.len()
                        } else {
                            0
                        },
                    ),
                    "bounded_work: rule constraint budget"
                );
                check()?;
                let reason = if c.kind == "calls" {
                    "call_binding_unavailable"
                } else {
                    "type_relationship_unavailable"
                };
                let mut evidence = vec![native_evidence(d)];
                if c.kind == "calls" {
                    for call in doc
                        .calls
                        .iter()
                        .filter(|call| call.owner_syntax_id == d.syntax_id)
                    {
                        ensure!(work.charge(1), "bounded_work: rule evidence budget");
                        check()?;
                        evidence.push(json!({"document":call.document,"revisionId":call.revision_id,"range":call.range,"kind":"call","syntaxId":call.owner_syntax_id,"callId":call.id,"provenanceId":call.provenance_id}));
                    }
                }
                constraints.push(json!({"mode":c.mode,"kind":c.kind,"targetRole":c.role,"status":"unknown","evidence":evidence,"unresolved":[reason]}));
                if !unresolved.iter().any(|r| r == reason) {
                    unresolved.push(reason.to_owned());
                }
            }
            let test = d.role == DeclarationRole::Test;
            if test {
                unresolved.push("test_only".into());
            }
            let mut row = json!({"rule":rule.id,"target":identity(d),"destination":null,"sourceLayer":null,"destinationLayer":null,"status":"unknown","excepted":false,"exceptionReason":null,
                    "constraints":constraints,"evidence":[native_evidence(d)],"unresolved":unresolved,"referenceCount":0});
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
    let limit = work::rules_limit(
        snapshot.documents.len(),
        work::snapshot_bytes(snapshot, check)?,
    );
    let path_values: Vec<_> = paths
        .iter()
        .map(|p| Spanned::new(0..0, p.clone()))
        .collect();
    let mut budget = 0;
    let filter = path_values
        .iter()
        .map(|p| pattern(&config.text, p, true, &mut budget))
        .collect::<Result<Vec<_>>>()
        .map_err(|_| anyhow::anyhow!("invalid_request: invalid rules source path glob"))?;
    let modules = &refs.modules;
    let layer_patterns: usize = config
        .layers
        .values()
        .map(|layer| layer.patterns.len())
        .sum();
    let mut work = Work::new(limit);
    ensure!(
        work.charge(modules.len().saturating_mul(layer_patterns)),
        "bounded_work: layer assignment pattern budget"
    );
    let source_set = portable_source_set(config, snapshot, &mut work, check)?;
    let mut matcher = LayerMatcher::new(config);
    let mut unassigned = Vec::new();
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
        if layer.is_none() {
            unassigned.push(module["identity"].clone());
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
                let (status, mut unresolved) = layer_status(config, source_layer, target_layer);
                if test {
                    unresolved.push("test_only".into());
                }
                let mut row = json!({"rule":"$layers","target":source,"destination":target,"sourceLayer":source_layer,"destinationLayer":target_layer,"status":status,
                    "excepted":false,"exceptionReason":null,"constraints":[],"evidence":evidence,"unresolved":unresolved,"referenceCount":edge["count"]});
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
                    "excepted":false,"exceptionReason":null,"constraints":[],"evidence":evidence,"unresolved":unresolved,"referenceCount":edge["count"]}));
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
            "excepted":false,"exceptionReason":null,"constraints":[],"evidence":[reference_evidence(module,site)],"unresolved":reasons,"referenceCount":1});
            // One unresolved source instance keeps every unresolved observed site.
            let rows = if test { &mut test_only } else { &mut instances };
            let key = (test, source.to_string());
            if let Some(index) = unresolved_positions.get(&key).copied() {
                let existing: &mut Value = &mut rows[index];
                existing["referenceCount"] =
                    json!(existing["referenceCount"].as_u64().unwrap_or(0) + 1);
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
    let (declarations, test_declarations) = DocumentEvaluation {
        config,
        inheritance,
        source_set,
        #[cfg(test)]
        before_document: None,
    }
    .run(&documents, &mut work, workers, check)?;
    instances.extend(declarations);
    test_only.extend(test_declarations);
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
    Ok(Evaluation {
        data: json!({"rulesVersion":"rules:v2","indexGeneration":snapshot.pin.index_generation.to_string(),"indexRevision":snapshot.pin.index_revision,"rulesFile":descriptor(config),
        "counts":counts(&instances),"testCounts":counts(&test_only),"instances":instances,"testOnly":test_only,"unassigned":unassigned,"externalPackages":external_packages,
        "limitations":["Layer results describe observed structurally mapped syntax references, not absence of semantic dependencies.","Calls and type relationships have no admitted bindings and remain unknown.","testOnly policy statuses are hypothetical and never production violations.","Structural reference provenance attests captured source bytes, not a semantic target association."]}),
        layers: assignments,
    })
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
        let count = counts(rows);
        payload[count_field] = count;
    }
    ensure!(found, "not_found: rule instance");
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
    fn document_work_overlaps_but_rows_and_counts_are_admitted_in_order() {
        let (config, snapshot) = fixture();
        let documents: Vec<_> = snapshot.documents.iter().collect();
        let evaluation = DocumentEvaluation {
            config: &config,
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
        let evaluation = DocumentEvaluation {
            config: &config,
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
