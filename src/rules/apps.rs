//! Apps under the rules: the effective inventory, layer matching, `AppScope` and
//! `AppAssignment`, and the checks of authored app names.
use super::{Config, RulesBase, error, glob_language};
use crate::references::{
    Projection, Snapshot,
    work::{self, Work},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

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
pub(super) fn effective_apps(
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

pub(super) struct LayerMatcher<'a> {
    config: &'a Config,
    pub(super) relations: BTreeMap<(&'a str, &'a str), glob_language::Relation>,
}
impl<'a> LayerMatcher<'a> {
    pub(super) fn new(config: &'a Config) -> Self {
        Self {
            config,
            relations: BTreeMap::new(),
        }
    }

    pub(super) fn select(
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

    /// A reference from layer `a` into layer `b` of an app it shares none with (#1131):
    /// `None` when no app of `a` declares `may_use` of an app of `b`; otherwise whether one of
    /// those declarations lists `b`.
    pub(crate) fn declared_use(&self, a: &str, b: &str) -> Option<bool> {
        let targets = self.layer(b);
        let mut declared = false;
        for source in self.layer(a) {
            let Some(authored) = self
                .config
                .apps
                .iter()
                .find(|app| app.name == self.apps[source].name)
            else {
                continue;
            };
            for used in &authored.may_use {
                if !self
                    .index(&used.app)
                    .is_some_and(|app| targets.contains(&app))
                {
                    continue;
                }
                declared = true;
                if used.layers.iter().any(|(layer, _)| layer == b) {
                    return Some(true);
                }
            }
        }
        declared.then_some(false)
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
pub(super) fn validate_apps(config: &Config, apps: &[crate::apps::App]) -> Result<()> {
    validate_layer_apps(config, apps)?;
    validate_app_uses(config, apps)?;
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

/// Each authored app's `may_use` (#1131), in app name order then app order: the app named
/// must be exactly one app of the effective inventory, and each layer listed must exist and
/// belong to that app.
fn validate_app_uses(config: &Config, apps: &[crate::apps::App]) -> Result<()> {
    let scope = AppScope::new(config, apps);
    for authored in &config.apps {
        for used in &authored.may_use {
            let matching = apps.iter().filter(|app| app.name == used.app).count();
            if matching != 1 {
                return Err(error(
                    &config.text,
                    used.span.clone(),
                    if matching == 0 {
                        "reference"
                    } else {
                        "ambiguity"
                    },
                    "token",
                    format!(
                        "app {} may_use names app {}, which {}",
                        authored.name,
                        used.app,
                        if matching == 0 {
                            "is not detected or authored".to_owned()
                        } else {
                            format!("{matching} apps share")
                        }
                    ),
                ));
            }
            let app = scope.index(&used.app).expect("one matching app");
            for (layer, span) in &used.layers {
                if !config.layers.contains_key(layer) || !scope.layer(layer).contains(&app) {
                    return Err(error(
                        &config.text,
                        span.clone(),
                        "reference",
                        "token",
                        format!(
                            "app {} may_use {} lists layer {layer}, which is not a layer of app {}",
                            authored.name, used.app, used.app
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
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
