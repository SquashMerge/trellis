//! Bounded, read-only architecture rules over one admitted source snapshot.
use crate::native_evidence::{Declaration, DeclarationRole, Range};
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

pub const RULES_PATH: &str = ".trellis/rules.toml";
const CONFIG_BYTES: usize = 2 * 1024 * 1024;
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
    paths: Vec<Spanned<String>>,
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawException {
    rule: Spanned<String>,
    target: Spanned<ConfigTarget>,
    destination: Option<Spanned<ConfigTarget>>,
    reason: Spanned<String>,
}
struct Layer {
    order: Option<i64>,
    patterns: Vec<GlobSet>,
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
struct Exception {
    rule: String,
    target: Value,
    destination: Option<Value>,
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
    let mut layers = BTreeMap::new();
    for (name, raw) in raw.layers {
        let span = raw.span();
        let layer = raw.into_inner();
        if name.trim().is_empty()
            || layer.paths.is_empty()
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
                "layer requires paths and exactly one integer order or shared=true",
            ));
        }
        let globs = layer
            .paths
            .iter()
            .map(|v| pattern(text, v, true, &mut patterns))
            .collect::<Result<Vec<_>>>()?;
        layers.insert(
            name,
            Layer {
                order: layer.order,
                patterns: globs,
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
        if id.trim().is_empty() || id == "$layers" || !ids.insert(id.clone()) {
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
        if !target_valid(e.target.get_ref())
            || e.destination
                .as_ref()
                .is_some_and(|d| !target_valid(d.get_ref()))
        {
            return Err(error(
                text,
                e.target.span(),
                "reference",
                "token",
                "malformed exception target identity",
            ));
        }
        if (e.rule.get_ref() == "$layers") != e.destination.is_some() {
            return Err(error(
                text,
                raw.span(),
                "reference",
                "token",
                "destination is required only for a layer edge exception",
            ));
        }
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
            target: e.target.get_ref().wire(),
            destination: e.destination.as_ref().map(|d| d.get_ref().wire()),
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
        json!({"name": name, "order": layer.order, "shared": layer.order.is_none()})
    }).collect()
}

pub(crate) fn layer_for<'a>(config: &'a Config, path: &str) -> Result<Option<&'a str>> {
    let mut selected = None;
    for (name, layer) in &config.layers {
        if layer.patterns.iter().any(|p| p.is_match(path)) {
            if selected.is_some() {
                return Err(error(
                    &config.text,
                    layer.span.clone(),
                    "ambiguity",
                    "token",
                    format!("path {path} matches multiple layers"),
                ));
            }
            selected = Some(name.as_str());
        }
    }
    Ok(selected)
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
fn decorate(config: &Config, row: &mut Value) {
    if let Some(e) = config.exceptions.iter().find(|e| {
        row["rule"] == e.rule
            && row["target"] == e.target
            && e.destination
                .as_ref()
                .map_or(row["destination"].is_null(), |d| row["destination"] == *d)
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
                    true
                } else {
                    false
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
pub fn evaluate(
    config: &Config,
    snapshot: &Snapshot,
    refs: &Projection,
    paths: &[String],
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Value> {
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
    let modules = refs.data["modules"]
        .as_array()
        .context("store_unavailable: references modules absent")?;
    let layer_patterns: usize = config
        .layers
        .values()
        .map(|layer| layer.patterns.len())
        .sum();
    ensure!(
        modules.len().saturating_mul(layer_patterns) <= 5_000_000,
        "bounded_work: layer assignment pattern budget"
    );
    let mut unassigned = Vec::new();
    let mut assignments = BTreeMap::new();
    let mut module_by_id = BTreeMap::new();
    for module in modules {
        check()?;
        let path = module["identity"]["path"]
            .as_str()
            .context("store_unavailable: module path absent")?;
        let layer = layer_for(config, path)?;
        assignments.insert(path, layer);
        module_by_id.insert(module["identity"].to_string(), module);
        if layer.is_none() {
            unassigned.push(module["identity"].clone());
        }
    }
    let mut work = 0usize;
    let mut unresolved_positions = BTreeMap::new();
    let mut instances = Vec::new();
    let mut test_only = Vec::new();
    if !config.layers.is_empty() {
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
                let target_layer = assignments
                    .get(
                        target["path"]
                            .as_str()
                            .context("store_unavailable: edge target path absent")?,
                    )
                    .copied()
                    .flatten();
                let (status, mut unresolved) = layer_status(config, source_layer, target_layer);
                if test {
                    unresolved.push("test_only".into());
                }
                let module = module_by_id
                    .get(&source.to_string())
                    .context("store_unavailable: edge source module absent")?;
                let evidence = edge["sites"]
                    .as_array()
                    .context("store_unavailable: edge sites absent")?
                    .iter()
                    .map(|s| reference_evidence(module, s))
                    .collect::<Vec<_>>();
                let mut row = json!({"rule":"$layers","target":source,"destination":target,"sourceLayer":source_layer,"destinationLayer":target_layer,"status":status,
                "excepted":false,"exceptionReason":null,"constraints":[],"evidence":evidence,"unresolved":unresolved,"referenceCount":edge["count"]});
                work = work.saturating_add(config.exceptions.len());
                ensure!(work <= 5_000_000, "bounded_work: exception match budget");
                decorate(config, &mut row);
                if test {
                    test_only.push(row)
                } else {
                    instances.push(row)
                }
            }
        }
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
        let bases = if inheritance {
            parse_bases(doc, check)?
        } else {
            BTreeMap::new()
        };
        for d in &doc.declarations {
            check()?;
            for rule in &config.rules {
                work = work.saturating_add(1 + config.roles[&rule.role].paths.len());
                ensure!(work <= 5_000_000, "bounded_work: rules selection budget");
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
                    work = work.saturating_add(
                        1 + if c.kind == "calls" {
                            doc.calls.len()
                        } else {
                            0
                        },
                    );
                    ensure!(work <= 5_000_000, "bounded_work: rule constraint budget");
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
                            work = work.saturating_add(1);
                            ensure!(work <= 5_000_000, "bounded_work: rule evidence budget");
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
                work = work.saturating_add(config.exceptions.len());
                ensure!(work <= 5_000_000, "bounded_work: exception match budget");
                decorate(config, &mut row);
                if test {
                    test_only.push(row)
                } else {
                    instances.push(row)
                }
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
    Ok(
        json!({"rulesVersion":"rules:v1","indexGeneration":snapshot.pin.index_generation.to_string(),"indexRevision":snapshot.pin.index_revision,"rulesFile":descriptor(config),
        "counts":counts(&instances),"testCounts":counts(&test_only),"instances":instances,"testOnly":test_only,"unassigned":unassigned,
        "limitations":["Layer results describe observed structurally mapped syntax references, not absence of semantic dependencies.","Calls and type relationships have no admitted bindings and remain unknown.","testOnly policy statuses are hypothetical and never production violations.","Structural reference provenance attests captured source bytes, not a semantic target association."]}),
    )
}
pub fn explain(
    mut payload: Value,
    rule: &str,
    target: &Value,
    destination: Option<&Value>,
) -> Result<Value> {
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
    Ok(payload)
}
