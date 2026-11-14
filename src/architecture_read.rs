//! The Architecture read service: references, rule checks, apps, endpoints, the browser
//! summary and its details, over one admitted response's snapshot.
//!
//! Moved up from `store.rs` (S5, #1045). The methods are still `impl EvidenceResponse`
//! blocks, so they run in the same read transaction and behind the same fence as before.
//! Storage keeps the one real read, `architecture_snapshot_regions`.
use crate::architecture_core::{
    self, ArchitectureArguments, ArchitectureCore, ArchitectureProgress, ArchitectureView,
};
use crate::metrics_read::MetricsOptions;
use crate::model::*;
use crate::store::EvidenceResponse;
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

/// How one architecture operation evaluates rules over its references.
type EvaluateRules<'a> = dyn FnMut(
        &crate::rules::Config,
        &crate::references::Projection,
        &mut (dyn FnMut() -> Result<()> + Send),
    ) -> Result<serde_json::Value>
    + 'a;

/// What every `trellis_endpoints` answer says of its rows (#851).
const ENDPOINTS_LIMITATION: &str = "Endpoints are structural matches of registered routes and client call sites by method and path template, not proof of what runs: a route with no call site, or a call with no route, does not prove it unused or missing.";
/// The note of a checkout whose interfaces were not extracted (#851).
const ENDPOINTS_NOT_EXTRACTED: &str = "not extracted: fewer than two apps";

impl EvidenceResponse {
    /// The checked arguments of one architecture operation.
    pub(crate) fn architecture_arguments(
        kind: &str,
        arguments: &serde_json::Value,
    ) -> Result<ArchitectureArguments> {
        ensure!(
            matches!(
                kind,
                "references" | "check" | "explain" | "prompt" | "endpoints" | "apps"
            ),
            "invalid_request"
        );
        let pin = arguments
            .get("expectedBasis")
            .map(|value| {
                serde_json::from_value::<IndexPin>(value.clone())
                    .map_err(|_| anyhow::anyhow!("invalid_request: expectedBasis"))
            })
            .transpose()?;
        let paths: Vec<String> = arguments
            .get("paths")
            .map(|value| {
                serde_json::from_value(value.clone())
                    .map_err(|_| anyhow::anyhow!("invalid_request: paths"))
            })
            .transpose()?
            .unwrap_or_default();
        ensure!(
            paths
                .iter()
                .all(|path| crate::references::valid_path_glob(path)),
            "invalid_request: paths"
        );
        let sample_limit = arguments
            .get("limit")
            .map(|value| {
                value
                    .as_u64()
                    .filter(|limit| (1..=50).contains(limit))
                    .map(|limit| limit as usize)
                    .ok_or_else(|| anyhow::anyhow!("invalid_request: limit"))
            })
            .transpose()?
            .unwrap_or(5);
        // #837: a references read may keep only the sources under a directory, and group
        // by app, which needs each file's app.
        let references = kind == "references";
        let prefix = match arguments.get("prefix").filter(|_| references) {
            None => None,
            Some(value) => Some(
                value
                    .as_str()
                    .and_then(crate::references::directory_prefix)
                    .context("invalid_request: prefix")?
                    .to_owned(),
            ),
        };
        let endpoints = kind == "endpoints";
        let apps = (references
            && arguments.get("groupBy").and_then(|by| by.as_str()) == Some("package"))
            || endpoints
            || kind == "apps";
        // #851: an app name or manifest path, and the endpoint a read keeps.
        let text = |field: &str, kinds: &[&str]| -> Result<Option<String>> {
            match arguments.get(field).filter(|_| kinds.contains(&kind)) {
                None => Ok(None),
                Some(value) => value
                    .as_str()
                    .filter(|text| !text.is_empty() && text.len() <= 4096)
                    .map(|text| Some(text.to_owned()))
                    .with_context(|| format!("invalid_request: {field}")),
            }
        };
        Ok(ArchitectureArguments {
            pin,
            paths,
            prefix,
            apps,
            app: text("app", &["references", "check", "endpoints", "apps"])?,
            method: text("method", &["endpoints"])?,
            path_template: text("pathTemplate", &["endpoints"])?,
            sample_limit,
        })
    }

    /// Materialize one captured-source architecture view in this admitted snapshot.
    pub fn architecture_response(
        &self,
        kind: &str,
        arguments: &serde_json::Value,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<serde_json::Value> {
        let parsed = Self::architecture_arguments(kind, arguments)?;
        check()?;
        let config = self.architecture_config(kind, &parsed)?;
        let selected = self.selected(parsed.pin)?;
        let core = self.architecture_core_at(
            selected.pin,
            config.as_ref(),
            false,
            &mut *check,
            &|_: ArchitectureProgress| {},
        )?;
        self.architecture_response_at(kind, arguments, &parsed, config.as_ref(), &core, check)
    }

    /// The rules this operation reads; a check or explanation fails without them. References
    /// read them only to group by app (#837), and work without them.
    pub(crate) fn architecture_config(
        &self,
        kind: &str,
        parsed: &ArchitectureArguments,
    ) -> Result<Option<crate::rules::Config>> {
        let config = if kind == "references" && !parsed.apps() {
            None
        } else {
            crate::rules::load(Path::new(self.store.workspace_root()))?
        };
        if matches!(kind, "check" | "explain") && config.is_none() {
            anyhow::bail!("rules_not_found");
        }
        Ok(config)
    }

    /// One architecture operation over a computed core.
    pub(crate) fn architecture_response_at(
        &self,
        kind: &str,
        arguments: &serde_json::Value,
        parsed: &ArchitectureArguments,
        config: Option<&crate::rules::Config>,
        core: &ArchitectureCore,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<serde_json::Value> {
        if kind == "endpoints" {
            return Self::endpoints_at(parsed, config, core, check);
        }
        // #851: an `app` filter assigns files as an unfiltered check does.
        let assigned = match &parsed.app {
            Some(app) => {
                let (assignment, whole) = Self::app_assignment_at(config, core, check)?;
                let named = assignment.named(app)?;
                Some((assignment, whole, named))
            }
            None => None,
        };
        let app_files = match (&assigned, kind) {
            (Some((assignment, whole, named)), "references") => {
                Some(Self::app_files(assignment, whole, named, check)?)
            }
            _ => None,
        };
        let references = core.references_in(parsed, app_files.as_ref(), check)?;
        let apps = if kind == "prompt" {
            Some(crate::rules::prompt_apps(
                config,
                core.app_candidates()?,
                check,
            )?)
        } else {
            None
        };
        // #837: app groups follow the whole checkout's evaluation, so they fail exactly when
        // an unfiltered `trellis_rules_check` does, whatever sources the read selects.
        let path_apps = match (parsed.apps, config) {
            (false, _) => None,
            (true, Some(config)) => {
                let base = core.rules(config)?;
                let whole = core.references(&[], 5, check)?;
                let evaluated = crate::rules::evaluate_prepared(config, base, &whole, &[], check)?;
                Some(crate::rules::path_apps(
                    Some(config),
                    base.apps(),
                    &evaluated.layers,
                    &references,
                    check,
                )?)
            }
            (true, None) => Some(crate::rules::path_apps(
                None,
                &crate::rules::unruled_apps(core.app_candidates()?),
                &Default::default(),
                &references,
                check,
            )?),
        };
        let mut evaluate =
            |config: &crate::rules::Config,
             references: &crate::references::Projection,
             check: &mut (dyn FnMut() -> Result<()> + Send)| {
                let mut check = || check();
                Ok(crate::rules::evaluate_prepared(
                    config,
                    core.rules(config)?,
                    references,
                    &parsed.paths,
                    &mut check,
                )?
                .data)
            };
        let mut payload = Self::architecture_payload(
            kind,
            arguments,
            config,
            references,
            core.pin,
            &mut evaluate,
            check,
        )?;
        if let Some(apps) = apps {
            payload["apps"] = serde_json::Value::Array(apps);
        }
        if let Some(path_apps) = path_apps {
            payload[crate::rules::PATH_APPS] = path_apps;
        }
        if kind == "check"
            && let Some((assignment, _, named)) = &assigned
        {
            Self::check_rows_in(&mut payload, assignment, named, check)?;
        }
        Ok(payload)
    }

    /// #851: each file's app at a core, from the same whole-checkout evaluation an unfiltered
    /// `trellis_rules_check` runs, and that whole projection. Without a rules file every
    /// detected manifest is an app and files go by root.
    pub(crate) fn app_assignment_at<'c>(
        config: Option<&'c crate::rules::Config>,
        core: &'c ArchitectureCore,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<(
        crate::rules::AppAssignment<'c>,
        crate::references::Projection,
    )> {
        let whole = core.references(&[], 5, check)?;
        let assignment = match config {
            Some(config) => {
                let base = core.rules(config)?;
                let evaluated = crate::rules::evaluate_prepared(config, base, &whole, &[], check)?;
                crate::rules::AppAssignment::new(
                    Some(config),
                    base.apps().to_vec(),
                    &evaluated.layers,
                )
            }
            None => crate::rules::AppAssignment::new(
                None,
                crate::rules::unruled_apps(core.app_candidates()?),
                &BTreeMap::new(),
            ),
        };
        Ok((assignment, whole))
    }

    /// The module paths of `whole` whose app is one of `named` (#851).
    pub(crate) fn app_files(
        assignment: &crate::rules::AppAssignment<'_>,
        whole: &crate::references::Projection,
        named: &BTreeSet<usize>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<BTreeSet<String>> {
        let mut files = BTreeSet::new();
        for module in &whole.modules {
            check()?;
            let path = module["identity"]["path"]
                .as_str()
                .context("store_unavailable: module path absent")?;
            if assignment
                .file(path)
                .is_some_and(|app| named.contains(&app))
            {
                files.insert(path.to_owned());
            }
        }
        Ok(files)
    }

    /// #851: the effective app inventory for `trellis_workspace_describe`, each app with its
    /// languages and file count, and the files in no app. A file in an authored hidden layer
    /// counts but adds no language (#909).
    pub(crate) fn app_inventory(
        assignment: &crate::rules::AppAssignment<'_>,
        whole: &crate::references::Projection,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<serde_json::Value> {
        let apps = assignment.apps();
        let mut files = vec![(0usize, BTreeSet::new()); apps.len()];
        let mut no_app = 0usize;
        for module in &whole.modules {
            check()?;
            let identity = &module["identity"];
            let path = identity["path"]
                .as_str()
                .context("store_unavailable: module path absent")?;
            match assignment.file(path) {
                Some(app) => {
                    files[app].0 += 1;
                    let language = identity["language"]
                        .as_str()
                        .context("store_unavailable: module language")?;
                    if assignment.counts_language(path) {
                        files[app].1.insert(language.to_owned());
                    }
                }
                None => no_app += 1,
            }
        }
        let items: Vec<_> = apps
            .iter()
            .zip(files)
            .map(|(app, (count, languages))| {
                serde_json::json!({
                    "name": app.name, "source": app.source, "kind": app.kind,
                    "manifest": app.manifest, "roots": app.roots, "entries": app.entries,
                    "languages": crate::semantic_records::LANGUAGE_ORDER
                        .iter()
                        .filter(|language| languages.contains(**language))
                        .collect::<Vec<_>>(),
                    "fileCount": count,
                })
            })
            .collect();
        Ok(serde_json::json!({"state": "ready", "items": items, "noApp": {"fileCount": no_app}}))
    }

    /// #851: a check's rows in the `named` apps. Instances and test-only rows are kept when
    /// they count under one of them (`apps[].counts`), unassigned native files when they lie
    /// in one; external packages, which are in no app, leave `unassigned`. Counts stay whole.
    fn check_rows_in(
        payload: &mut serde_json::Value,
        assignment: &crate::rules::AppAssignment<'_>,
        named: &BTreeSet<usize>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        for field in ["instances", "testOnly"] {
            let rows = payload[field]
                .as_array_mut()
                .context("store_unavailable: rule instances absent")?;
            check()?;
            rows.retain(|row| {
                assignment
                    .bucket(row)
                    .is_some_and(|app| named.contains(&app))
            });
        }
        let unassigned = payload["unassigned"]
            .as_array_mut()
            .context("store_unavailable: unassigned catalog absent")?;
        unassigned.retain(|identity| {
            identity["path"]
                .as_str()
                .and_then(|path| assignment.file(path))
                .is_some_and(|app| named.contains(&app))
        });
        check()
    }

    /// #851: one `trellis_endpoints` answer at a core, every row before paging.
    fn endpoints_at(
        parsed: &ArchitectureArguments,
        config: Option<&crate::rules::Config>,
        core: &ArchitectureCore,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<serde_json::Value> {
        // Whether a core extracts depends on its rules file's authored apps, so its extraction
        // answers only the file it was built for, or no file (#851). The owner never hands a
        // read another file's core; a core of another file here is refused, never projected.
        ensure!(
            core.rules_hash() == config.map(|config| config.content_hash.as_str()),
            "store_unavailable: architecture core of another rules file"
        );
        let extraction = core.interfaces()?;
        let (assignment, _) = Self::app_assignment_at(config, core, check)?;
        let named = parsed
            .app
            .as_deref()
            .map(|app| assignment.named(app))
            .transpose()?;
        let extracted = architecture_core::interfaces_extracted(config, core.app_candidates()?);
        let rows = crate::interfaces::endpoints(
            extraction,
            &assignment,
            &crate::interfaces::EndpointQuery {
                apps: named.as_ref(),
                method: parsed.method.as_deref(),
                path_template: parsed.path_template.as_deref(),
            },
            check,
        )?;
        let mut limitations = vec![ENDPOINTS_LIMITATION];
        if !extracted {
            limitations.push(ENDPOINTS_NOT_EXTRACTED);
        }
        check()?;
        Ok(serde_json::json!({
            "evidence": "structural",
            "extracted": extracted,
            "limitations": limitations,
            "endpoints": rows,
            "indexGeneration": core.pin.index_generation.to_string(),
            "indexRevision": core.pin.index_revision,
        }))
    }

    /// One path-scoped operation computed from its own scope, under the budgets of that
    /// scope. For a revision too large for a core.
    pub(crate) fn architecture_response_scoped(
        &self,
        kind: &str,
        arguments: &serde_json::Value,
        parsed: &ArchitectureArguments,
        config: Option<&crate::rules::Config>,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<serde_json::Value> {
        let snapshot = self
            .architecture_snapshot(parsed.pin, check)
            .map_err(|error| self.store.report_index_failure(error))?;
        // #851: an `app` filter assigns every document of the snapshot.
        let assigned = match &parsed.app {
            Some(app) => {
                let candidates = crate::rules::app_candidates(&snapshot, check)?;
                let paths: Vec<&str> = snapshot
                    .documents
                    .iter()
                    .map(|document| document.key.path.as_str())
                    .collect();
                let assignment = match config {
                    Some(config) => {
                        let base = crate::rules::prepare(config, &snapshot, &mut *check)?;
                        crate::rules::AppAssignment::for_paths(
                            config,
                            base.apps().to_vec(),
                            &paths,
                            check,
                        )?
                    }
                    None => crate::rules::AppAssignment::new(
                        None,
                        crate::rules::unruled_apps(&candidates),
                        &BTreeMap::new(),
                    ),
                };
                let named = assignment.named(app)?;
                let files: BTreeSet<String> = paths
                    .iter()
                    .filter(|path| {
                        assignment
                            .file(path)
                            .is_some_and(|app| named.contains(&app))
                    })
                    .map(|path| (*path).to_owned())
                    .collect();
                Some((assignment, named, files))
            }
            None => None,
        };
        let references = crate::references::project_where(
            &snapshot,
            &parsed.paths,
            parsed.prefix.as_deref(),
            assigned
                .as_ref()
                .filter(|_| kind == "references")
                .map(|(_, _, files)| files),
            parsed.sample_limit,
            check,
        )?;
        let apps = if kind == "prompt" {
            let candidates = crate::rules::app_candidates(&snapshot, check)?;
            Some(crate::rules::prompt_apps(config, &candidates, check)?)
        } else {
            None
        };
        // #837: as in `architecture_response_at`, check's whole-checkout validity. Layer
        // assignment covers every module of the projection, whatever its sources.
        let path_apps = match (parsed.apps, config) {
            (false, _) => None,
            (true, Some(config)) => {
                let base = crate::rules::prepare(config, &snapshot, &mut *check)?;
                let evaluated =
                    crate::rules::evaluate_prepared(config, &base, &references, &[], check)?;
                Some(crate::rules::path_apps(
                    Some(config),
                    base.apps(),
                    &evaluated.layers,
                    &references,
                    check,
                )?)
            }
            (true, None) => {
                let candidates = crate::rules::app_candidates(&snapshot, check)?;
                Some(crate::rules::path_apps(
                    None,
                    &crate::rules::unruled_apps(&candidates),
                    &Default::default(),
                    &references,
                    check,
                )?)
            }
        };
        let mut evaluate =
            |config: &crate::rules::Config,
             references: &crate::references::Projection,
             check: &mut (dyn FnMut() -> Result<()> + Send)| {
                let mut check = || check();
                crate::rules::evaluate(config, &snapshot, references, &parsed.paths, &mut check)
            };
        let mut payload = Self::architecture_payload(
            kind,
            arguments,
            config,
            references,
            snapshot.pin,
            &mut evaluate,
            check,
        )?;
        if let Some(apps) = apps {
            payload["apps"] = serde_json::Value::Array(apps);
        }
        if let Some(path_apps) = path_apps {
            payload[crate::rules::PATH_APPS] = path_apps;
        }
        if kind == "check"
            && let Some((assignment, named, _)) = &assigned
        {
            Self::check_rows_in(&mut payload, assignment, named, check)?;
        }
        Ok(payload)
    }

    /// One operation's answer from its references and its way of evaluating the rules.
    fn architecture_payload(
        kind: &str,
        arguments: &serde_json::Value,
        config: Option<&crate::rules::Config>,
        references: crate::references::Projection,
        pin: IndexPin,
        evaluate: &mut EvaluateRules<'_>,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<serde_json::Value> {
        let mut payload = match kind {
            "references" => references.data,
            "prompt" => serde_json::json!({
                "rulesText": config.map(|config| &config.text),
                "rulesFile": config.map(crate::rules::descriptor),
                "references": references.data,
            }),
            "check" | "explain" => {
                let config = config.expect("checked rules presence");
                let result = evaluate(config, &references, &mut *check)?;
                if kind == "explain" {
                    crate::rules::explain(
                        result,
                        arguments["rule"]
                            .as_str()
                            .context("invalid_request: rule")?,
                        arguments.get("target").context("invalid_request: target")?,
                        arguments.get("destination"),
                        check,
                    )?
                } else {
                    result
                }
            }
            _ => unreachable!(),
        };
        payload["indexGeneration"] = serde_json::json!(pin.index_generation.to_string());
        payload["indexRevision"] = serde_json::json!(pin.index_revision);
        check()?;
        Ok(payload)
    }

    /// Browser-only layer summary, references and metrics from this admitted read.
    pub fn architecture_view(
        &self,
        options: &MetricsOptions,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<serde_json::Value> {
        options.validate()?;
        check()?;
        let config = crate::rules::load(Path::new(self.store.workspace_root()))?;
        let view = self.architecture_view_head(options, config.as_ref())?;
        let pin = match &view {
            ArchitectureView::Ready { pin, .. } => *pin,
            ArchitectureView::Done(payload) => {
                check()?;
                return Ok(payload.clone());
            }
        };
        let core = self.architecture_core_at(
            pin,
            config.as_ref(),
            true,
            &mut *check,
            &|_: ArchitectureProgress| {},
        )?;
        let summary = self.architecture_view_at(
            options,
            view,
            config.as_ref().expect("ready config"),
            &core,
            check,
        )?;
        Ok(summary.as_ref().clone())
    }

    /// The summary's revision and rules state. Only a ready summary needs a core.
    pub(crate) fn architecture_view_head(
        &self,
        options: &MetricsOptions,
        config: Option<&crate::rules::Config>,
    ) -> Result<ArchitectureView> {
        let selected = self.selected(options.pin)?;
        let layers = config.map(crate::rules::view_layers).unwrap_or_default();
        let payload = serde_json::json!({
            "indexGeneration": selected.pin.index_generation.to_string(),
            "indexRevision": selected.pin.index_revision,
            "state": if config.is_none() { "missing_rules" } else if layers.is_empty() { "no_layers" } else { "ready" },
            "rulesFile": config.map(crate::rules::descriptor),
            "exceptionCount": config.map(crate::rules::Config::exception_count),
            "aggregate": options.aggregate, "metricsVersion": "metrics:v1",
            "role": options.role, "workbench": options.workbench, "evidenceTier": "syntax", "semanticComplete": false,
            "layers": layers, "modules": [], "edges": [], "references": null, "rules": null,
            "apps": [], "noApp": null, "interfaces": []
        });
        Ok(if payload["state"] == "ready" {
            ArchitectureView::Ready {
                pin: selected.pin,
                payload,
            }
        } else {
            ArchitectureView::Done(payload)
        })
    }

    /// The summary at a computed core. A core serves each role and aggregate once.
    pub(crate) fn architecture_view_at(
        &self,
        options: &MetricsOptions,
        view: ArchitectureView,
        config: &crate::rules::Config,
        core: &ArchitectureCore,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<Arc<serde_json::Value>> {
        let key = (
            options.role.clone(),
            options.aggregate.clone(),
            options.workbench,
        );
        if let Some(summary) = core.summary(&key) {
            return Ok(summary);
        }
        let ArchitectureView::Ready { mut payload, .. } = view else {
            anyhow::bail!("store_unavailable: architecture summary state");
        };
        let references = core.references(&[], 5, check)?;
        let evaluated =
            crate::rules::evaluate_prepared(config, core.rules(config)?, &references, &[], check)?;
        payload["evidenceTier"] = references.data["evidenceTier"].clone();
        payload["partialFiles"] = references.data["partialFiles"].clone();
        let references_coverage = &references.coverage;
        let rules = evaluated.data;
        let assignments = evaluated.layers;
        let base = core.rules(config)?;
        let scope = crate::rules::AppScope::new(config, base.apps());
        let no_app_counts = rules["noApp"]["counts"].clone();
        let app_rules = rules["apps"]
            .as_array()
            .context("store_unavailable: per-app counts")?;
        ensure!(
            app_rules.len() == base.apps().len(),
            "store_unavailable: per-app counts"
        );
        let metrics = self.metrics_at_with_config(
            &MetricsOptions {
                pin: Some(core.pin),
                role: options.role.clone(),
                workbench: options.workbench,
                aggregate: options.aggregate.clone(),
                ..MetricsOptions::default()
            },
            Some(config),
            true,
            Some(core.metrics()?),
            check,
        )?;
        let files: BTreeMap<_, _> = metrics
            .files
            .iter()
            .map(|file| {
                (
                    (
                        file.source_set_id.as_str(),
                        file.language.as_str(),
                        file.path.as_str(),
                    ),
                    file,
                )
            })
            .collect();
        let mut modules = Vec::new();
        let mut app_files = vec![(0usize, 0usize, BTreeSet::new()); base.apps().len()];
        let mut unassigned_files = 0usize;
        for module in &references.modules {
            check()?;
            let target = &module["identity"];
            let key = (
                target["sourceSetId"]
                    .as_str()
                    .context("store_unavailable: module source set")?,
                target["language"]
                    .as_str()
                    .context("store_unavailable: module language")?,
                target["path"]
                    .as_str()
                    .context("store_unavailable: module path")?,
            );
            let file = files
                .get(&key)
                .context("store_unavailable: architecture metrics join")?;
            let mut row = module.clone();
            row["layer"] = serde_json::json!(
                assignments
                    .get(key.2)
                    .context("store_unavailable: architecture layer assignment absent")?
            );
            let layer = assignments.get(key.2).copied().flatten();
            let app = scope.file(key.2, layer);
            row["app"] = serde_json::json!(app.and_then(|index| scope.name(index)));
            match app {
                Some(index) => {
                    let entry = &mut app_files[index];
                    entry.0 += 1;
                    entry.1 += usize::from(layer.is_none());
                    // #909: hidden tooling, test-harness and fixture layers add no language.
                    if !layer.is_some_and(|layer| crate::rules::hidden_layer(config, layer)) {
                        entry.2.insert(key.1);
                    }
                }
                None => unassigned_files += usize::from(layer.is_none()),
            }
            row["physicalLines"] = serde_json::json!(file.physical_lines);
            row["declarationCount"] = serde_json::json!(file.declaration_count);
            row["roleCounts"] = serde_json::json!(file.role_counts);
            row["metrics"] = file.metrics.clone();
            if let Some(reason) = file.excluded_reason {
                row["excludedReason"] = serde_json::json!(reason);
            }
            modules.push(row);
        }
        // Keep policy and exception contributions separate even on the same pair. An edge's
        // pairs and their sampled sites are paged by the details route's `edge` kind (#917).
        type SummaryKey = (String, String, String, String, bool, Option<String>);
        let mut summaries: BTreeMap<SummaryKey, serde_json::Value> = BTreeMap::new();
        crate::architecture_details::edge_pairs(
            &references.data,
            &rules,
            &mut *check,
            |role, edge, pair| {
                let summary = summaries
                    .entry((
                        edge.source.into(),
                        edge.destination.into(),
                        role.into(),
                        edge.status.into(),
                        edge.excepted,
                        edge.reason.map(str::to_owned),
                    ))
                    .or_insert_with(|| {
                        serde_json::json!({
                            "key": edge.key(role),
                            "sourceLayer": edge.source, "destinationLayer": edge.destination,
                            "role": role, "status": edge.status, "excepted": edge.excepted,
                            "exceptionReason": edge.reason, "count": 0, "semanticSites": 0
                        })
                    });
                summary["count"] = serde_json::json!(
                    summary["count"].as_u64().unwrap_or(0)
                        + pair["count"]
                            .as_u64()
                            .context("store_unavailable: reference count")?
                );
                summary["semanticSites"] = serde_json::json!(
                    summary["semanticSites"].as_u64().unwrap_or(0)
                        + pair["semanticSites"]
                            .as_u64()
                            .context("store_unavailable: reference semantic count")?
                );
                Ok(())
            },
        )?;
        // Undrawn detail (sites, pairs, instances) is served by paged detail reads (#448).
        let (mut references, mut rules) = crate::architecture_details::summary(
            &references.data,
            &rules,
            &crate::architecture_details::Layers::new(&assignments, &rules),
        )?;
        // Exact site coverage per source layer and role, so drawn counts can be read as
        // lower bounds next to their arrows (#449). One row per (layer, role).
        let mut coverage: BTreeMap<(Option<&str>, bool), crate::references::SiteCoverage> =
            BTreeMap::new();
        for ((path, test), counted) in references_coverage {
            check()?;
            let layer = assignments.get(path.as_str()).copied().flatten();
            let row = coverage.entry((layer, *test)).or_default();
            row.sites += counted.sites;
            row.mapped += counted.mapped;
            row.unresolved += counted.unresolved;
            row.semantic += counted.semantic;
        }
        let coverage_row = |row: &crate::references::SiteCoverage| {
            serde_json::json!({
                "sites": row.sites, "mappedSites": row.mapped, "unresolvedSites": row.unresolved,
                "semanticSites": row.semantic
            })
        };
        // Each arrow carries its evidence tier and its source layer's coverage: its count is
        // exact (`complete`) only when no site of that layer and role stayed unresolved (#523).
        for summary in summaries.values_mut() {
            check()?;
            let test = summary["role"] == "test";
            let row = coverage
                .get(&(summary["sourceLayer"].as_str(), test))
                .copied()
                .unwrap_or_default();
            let count = summary["count"].as_u64().unwrap_or(0);
            let semantic = summary["semanticSites"].as_u64().unwrap_or(0);
            summary["evidenceTier"] = serde_json::json!(match semantic {
                0 => "syntax",
                n if n == count => "semantic",
                _ => "mixed",
            });
            let mut covered = coverage_row(&row);
            covered["complete"] = serde_json::json!(row.unresolved == 0);
            summary["coverage"] = covered;
        }
        references["coverage"] = serde_json::json!(
            coverage
                .into_iter()
                .map(|((layer, test), row)| {
                    let mut value = coverage_row(&row);
                    value["layer"] = serde_json::json!(layer);
                    value["role"] = serde_json::json!(if test { "test" } else { "production" });
                    value
                })
                .collect::<Vec<_>>()
        );
        let apps: Vec<_> = base
            .apps()
            .iter()
            .zip(app_rules)
            .zip(&app_files)
            .map(|((app, counted), (file_count, unassigned, languages))| {
                serde_json::json!({
                    "name": app.name, "kind": app.kind, "manifest": app.manifest,
                    "roots": app.roots, "entries": app.entries,
                    "languages": crate::semantic_records::LANGUAGE_ORDER
                        .iter()
                        .filter(|language| languages.contains(**language))
                        .collect::<Vec<_>>(),
                    "fileCount": file_count, "source": app.source,
                    "counts": counted["counts"], "unassignedFileCount": unassigned
                })
            })
            .collect();
        let no_app = serde_json::json!({
            "counts": no_app_counts, "unassignedFileCount": unassigned_files
        });
        let interfaces =
            crate::interfaces::project(core.interfaces()?, &scope, &assignments, check)?.summary();
        drop(assignments);
        for value in [&mut references, &mut rules] {
            value["indexGeneration"] = serde_json::json!(core.pin.index_generation.to_string());
            value["indexRevision"] = serde_json::json!(core.pin.index_revision);
        }
        payload["modules"] = serde_json::json!(modules);
        payload["edges"] = serde_json::json!(summaries.into_values().collect::<Vec<_>>());
        payload["references"] = references;
        payload["rules"] = rules;
        payload["apps"] = serde_json::json!(apps);
        payload["noApp"] = no_app;
        payload["interfaces"] = serde_json::json!(interfaces);
        check()?;
        Ok(core.remember_summary(key, payload))
    }

    /// One bounded page of Architecture detail at the summary's pin and rules hash.
    /// A different rules file fails `rules_conflict`; a stale pin fails like the summary.
    pub fn architecture_details(
        &self,
        query: &crate::architecture_details::Query,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<serde_json::Value> {
        check()?;
        let config = self.architecture_details_config(query)?;
        let core = self.architecture_core_at(
            query.pin,
            Some(&config),
            false,
            &mut *check,
            &|_: ArchitectureProgress| {},
        )?;
        self.architecture_details_at(query, &config, &core, check)
    }

    /// The rules file a detail read pins, or `rules_conflict`.
    pub(crate) fn architecture_details_config(
        &self,
        query: &crate::architecture_details::Query,
    ) -> Result<crate::rules::Config> {
        let config = crate::rules::load(Path::new(self.store.workspace_root()))?
            .context("rules_conflict: rules file missing")?;
        ensure!(
            config.content_hash == query.rules_hash,
            "rules_conflict: rules changed since the summary"
        );
        self.selected(Some(query.pin))?;
        Ok(config)
    }

    /// One bounded detail page at a computed core.
    pub(crate) fn architecture_details_at(
        &self,
        query: &crate::architecture_details::Query,
        config: &crate::rules::Config,
        core: &ArchitectureCore,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<serde_json::Value> {
        let references = core.references(&[], 5, check)?;
        let evaluated =
            crate::rules::evaluate_prepared(config, core.rules(config)?, &references, &[], check)?;
        let layers =
            crate::architecture_details::Layers::new(&evaluated.layers, &evaluated.data).with_apps(
                crate::rules::AppScope::new(config, core.rules(config)?.apps()),
            );
        let interfaces = if query.kind == crate::architecture_details::Kind::Interfaces {
            Some(crate::interfaces::project(
                core.interfaces()?,
                &crate::rules::AppScope::new(config, core.rules(config)?.apps()),
                &evaluated.layers,
                check,
            )?)
        } else {
            None
        };
        let page = crate::architecture_details::page(
            query,
            &references.data,
            &evaluated.data,
            &layers,
            interfaces.as_ref(),
            check,
        )?;
        check()?;
        Ok(page)
    }

    /// Browser rule authoring validates the entire proposal against one admitted
    /// snapshot. Nothing here publishes or changes index state.
    pub(crate) fn validate_rules_edit(
        &self,
        pin: IndexPin,
        base: &crate::rules::Config,
        proposed: &crate::rules::Config,
        request: &crate::rules_edit::Request,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<()> {
        let snapshot = self
            .architecture_snapshot(Some(pin), check)
            .map_err(|error| self.store.report_index_failure(error))?;
        let references = crate::references::project(&snapshot, &[], 5, check)?;
        let evaluated =
            crate::rules::evaluate_with_layers(base, &snapshot, &references, &[], check)?;
        crate::rules_edit::validate_selection(request, &evaluated.data, &evaluated.layers, check)?;
        crate::rules::evaluate(proposed, &snapshot, &references, &[], check)?;
        Ok(())
    }

    pub(crate) fn rules_edit_inventory(&self, pin: IndexPin) -> Result<serde_json::Value> {
        self.selected(Some(pin))?;
        let config = crate::rules::load(Path::new(self.store.workspace_root()))?
            .context("not_found: rules file missing")?;
        crate::rules_edit::inventory(&config, pin)
    }
}

#[cfg(test)]
#[path = "architecture_read_apps_tests.rs"]
mod apps_tests;
