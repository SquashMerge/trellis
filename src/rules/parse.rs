//! Loading and parsing `.trellis/rules.toml` into a `Config`. The authored types and
//! `Config` stay in `rules.rs`.
use super::{
    AuthoredApp, COLLECTION_LIMIT, CONFIG_BYTES, Config, ConfigTarget, Constraint, Exception,
    ExceptionKind, ExceptionTarget, ExternalSelector, InterfaceWrapper, Layer, LayerPattern,
    PATTERN_LIMIT, RULES_PATH, Raw, RawApp, RawException, RawInterfaces, RawLayer, RawRole,
    RawRule, Rule, Selector, WrapperMethod, error, languages,
};
use anyhow::Result;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use toml::Spanned;

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
pub(super) fn pattern(
    text: &str,
    value: &Spanned<String>,
    path: bool,
    budget: &mut usize,
) -> Result<GlobSet> {
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
/// Parses `rules.toml`. Sections are parsed in a fixed order, which decides the first error
/// reported and how the one glob and selector budget (`PATTERN_LIMIT`) is consumed.
pub fn parse(text: &str) -> Result<Config> {
    let raw = decode(text)?;
    let peers = parse_policy(text, raw.layer_policy.peers)?;
    let shared_outbound = parse_policy(text, raw.layer_policy.shared_outbound)?;
    let mut patterns = 0;
    let external_deny = external_selectors(text, &raw.external_policy.deny, &mut patterns)?;
    let layers = parse_layers(text, raw.layers, &mut patterns)?;
    let apps = authored_apps(text, raw.apps)?;
    let wrappers = interface_wrappers(text, raw.interfaces)?;
    let test_features = test_features(text, raw.test_features)?;
    let roles = parse_roles(text, raw.roles, &mut patterns)?;
    let (rules, ids) = parse_rules(text, raw.rules, &roles)?;
    let exceptions = parse_exceptions(text, raw.exceptions, &ids)?;
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
        test_features,
    })
}
/// The size bound, the TOML and schema decode, the version and the named collection bounds.
fn decode(text: &str) -> Result<Raw> {
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
    Ok(raw)
}
/// One `[layer_policy]` value.
fn parse_policy(text: &str, value: Option<Spanned<String>>) -> Result<Option<String>> {
    value
        .map(|v| {
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
}
/// The `[layers.<name>]` tables. An external package belongs to at most one layer.
fn parse_layers(
    text: &str,
    raw: BTreeMap<String, Spanned<RawLayer>>,
    patterns: &mut usize,
) -> Result<BTreeMap<String, Layer>> {
    let mut layers = BTreeMap::new();
    let mut external_owners = BTreeMap::new();
    for (name, raw) in raw {
        let span = raw.span();
        let layer = parse_layer(text, &name, raw, patterns)?;
        for selector in &layer.external {
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
        layers.insert(name, layer);
    }
    Ok(layers)
}
/// One layer: its shape, display hint, app scope, path globs and external selectors.
fn parse_layer(
    text: &str,
    name: &str,
    raw: Spanned<RawLayer>,
    patterns: &mut usize,
) -> Result<Layer> {
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
                matcher: pattern(text, v, true, patterns)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    globs.sort_by(|a, b| a.text.cmp(&b.text));
    globs.dedup_by(|a, b| a.text == b.text);
    let external = external_selectors(text, &layer.external, patterns)?;
    Ok(Layer {
        order: layer.order,
        display,
        app,
        apps,
        patterns: globs,
        external,
        span,
    })
}
/// The `[roles.<name>]` tables.
fn parse_roles(
    text: &str,
    raw: BTreeMap<String, Spanned<RawRole>>,
    patterns: &mut usize,
) -> Result<BTreeMap<String, Selector>> {
    let mut roles = BTreeMap::new();
    for (name, raw) in raw {
        let selector = parse_role(text, &name, raw.get_ref(), patterns)?;
        roles.insert(name, selector);
    }
    Ok(roles)
}
/// One role selector: its shape first, then its globs.
fn parse_role(
    text: &str,
    name: &str,
    selector: &RawRole,
    patterns: &mut usize,
) -> Result<Selector> {
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
    if s.paths.as_ref().is_some_and(Vec::is_empty) || s.language.as_ref().is_some_and(Vec::is_empty)
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
    Ok(Selector {
        paths: s
            .paths
            .iter()
            .flatten()
            .map(|p| pattern(text, p, true, patterns))
            .collect::<Result<_>>()?,
        name: s
            .name
            .as_ref()
            .map(|p| pattern(text, p, false, patterns))
            .transpose()?,
        language: s
            .language
            .iter()
            .flatten()
            .map(|l| l.get_ref().clone())
            .collect(),
        extends: s.extends.as_ref().map(|s| s.get_ref().clone()),
        implements: s.implements.as_ref().map(|s| s.get_ref().clone()),
    })
}
/// The `[[rules]]` entries, and the ids they define.
fn parse_rules(
    text: &str,
    raw: Vec<Spanned<RawRule>>,
    roles: &BTreeMap<String, Selector>,
) -> Result<(Vec<Rule>, BTreeSet<String>)> {
    let mut rules = Vec::new();
    let mut ids = BTreeSet::new();
    for raw in raw {
        rules.push(parse_rule(text, &raw, roles, &mut ids)?);
    }
    Ok((rules, ids))
}
fn parse_rule(
    text: &str,
    raw: &Spanned<RawRule>,
    roles: &BTreeMap<String, Selector>,
    ids: &mut BTreeSet<String>,
) -> Result<Rule> {
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
            constraints.push(parse_constraint(text, mode, value, roles)?);
        }
    }
    Ok(Rule {
        id: id.clone(),
        role: r.for_each.get_ref().clone(),
        constraints,
    })
}
fn parse_constraint(
    text: &str,
    mode: &'static str,
    value: &Spanned<BTreeMap<String, String>>,
    roles: &BTreeMap<String, Selector>,
) -> Result<Constraint> {
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
    Ok(Constraint {
        mode,
        kind: kind.clone(),
        role: role.clone(),
    })
}
/// What makes an exception a duplicate: its rule, target and destination.
type ExceptionKey = (String, ExceptionTarget, Option<ExceptionTarget>);
/// The `[[exceptions]]` entries, each naming `$layers` or one of the rule `ids`.
fn parse_exceptions(
    text: &str,
    raw: Vec<Spanned<RawException>>,
    ids: &BTreeSet<String>,
) -> Result<Vec<Exception>> {
    let mut exceptions = Vec::new();
    let mut exception_keys = BTreeSet::new();
    for raw in raw {
        exceptions.push(parse_exception(text, &raw, ids, &mut exception_keys)?);
    }
    Ok(exceptions)
}
fn parse_exception(
    text: &str,
    raw: &Spanned<RawException>,
    ids: &BTreeSet<String>,
    exception_keys: &mut BTreeSet<ExceptionKey>,
) -> Result<Exception> {
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
    Ok(Exception {
        rule: e.rule.get_ref().clone(),
        kind,
        reason: e.reason.get_ref().clone(),
    })
}

/// The most features `test_features` may name.
const TEST_FEATURE_LIMIT: usize = 64;

/// The authored `test_features` (#988): distinct Cargo feature names, each at most 64 bytes of
/// ASCII letters, digits, `_`, `-`, `+` and `.`, starting with a letter, digit or `_`.
fn test_features(text: &str, raw: Option<Spanned<Vec<Spanned<String>>>>) -> Result<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let raw = raw.into_inner();
    if raw.len() > TEST_FEATURE_LIMIT {
        return Err(error(
            text,
            raw[TEST_FEATURE_LIMIT].span(),
            "too_large",
            "token",
            "test_features exceeds 64 entries",
        ));
    }
    let mut features = BTreeSet::new();
    for feature in raw {
        let span = feature.span();
        let feature = feature.into_inner();
        if !crate::references::feature_name(&feature) {
            return Err(error(
                text,
                span,
                "schema",
                "token",
                format!("test feature {feature:?} is not a Cargo feature name"),
            ));
        }
        if !features.insert(feature.clone()) {
            return Err(error(
                text,
                span,
                "schema",
                "token",
                format!("test feature {feature:?} is repeated"),
            ));
        }
    }
    Ok(features.into_iter().collect())
}

/// The test features of the checkout's rules file for a read that does not otherwise load it
/// (#988). A missing or invalid file names none, so such a read still answers.
pub(crate) fn load_test_features(root: &Path) -> Vec<String> {
    match load(root) {
        Ok(Some(config)) => config.test_features,
        _ => Vec::new(),
    }
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
