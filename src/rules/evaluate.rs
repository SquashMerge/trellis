//! Rule evaluation: preparation, the policy join over references, and `explain`.
//! `RulesBase` stays in `rules.rs`.
use super::{
    Config, DocumentEvaluation, ExceptionKind, ExternalSelector, RulesBase, Targets,
    apps::{AppScope, LayerMatcher, effective_apps, hidden_layer_paths, validate_apps},
    declarations::package_closure,
    error, glob_matches,
    parse::{descriptor, pattern},
};
use crate::references::{
    Projection, Snapshot,
    work::{self, Work},
};
use anyhow::{Context, Result, ensure};
use globset::GlobSet;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use toml::Spanned;

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
pub(super) fn portable_source_set<'a>(
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
pub(super) fn decorate(config: &Config, source_set: Option<&str>, row: &mut Value) {
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

pub(super) fn semantic_evidence(mut evidence: Value, metadata: &BTreeMap<String, Value>) -> Value {
    for (key, value) in metadata {
        evidence[key] = value.clone();
    }
    if let Some(revision) = evidence["projection"]["basisNativeRevisionId"].as_str() {
        evidence["revisionId"] = json!(revision);
    }
    evidence
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
    let mut unassigned = Vec::new();
    let ModuleAssignments {
        assignments,
        module_by_id,
        unassigned_files,
    } = assign_modules(config, modules, &filter, &mut unassigned, &mut work, check)?;
    let external_packages = refs.data["externalPackages"]
        .as_array()
        .context("store_unavailable: external package descriptors absent")?;
    let external_assignments =
        assign_external(config, external_packages, &mut unassigned, &mut work, check)?;
    let join = PolicyJoin {
        config,
        filter: &filter,
        scope: &scope,
        source_set,
        assignments: &assignments,
        external_assignments: &external_assignments,
        module_by_id: &module_by_id,
    };
    let mut instances = Vec::new();
    let mut test_only = Vec::new();
    for (field, test) in [("production", false), ("tests", true)] {
        for edge in refs.data[field]
            .as_array()
            .context("store_unavailable: references edges absent")?
        {
            check()?;
            let rows = if test { &mut test_only } else { &mut instances };
            join.edge_rows(edge, test, rows, &mut work, check)?;
        }
    }
    if !config.layers.is_empty() {
        join.unresolved_rows(refs, &mut instances, &mut test_only, check)?;
    }
    select_prepared(base, &filter, &mut instances, &mut test_only, check)?;
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
        "partialFiles":partial_files,"limitations":limitations,"semanticDocumentStatus":refs.data["semanticDocumentStatus"],"partial":refs.data["partial"]}),
        layers: assignments,
    })
}

/// The layer of every module by path, every module by identity, and the count of modules with
/// no layer.
struct ModuleAssignments<'a> {
    assignments: BTreeMap<&'a str, Option<&'a str>>,
    module_by_id: BTreeMap<String, &'a Value>,
    unassigned_files: usize,
}

/// Assigns every module its layer. Modules with no layer that the filter selects are added to
/// `unassigned`.
fn assign_modules<'a>(
    config: &'a Config,
    modules: &'a [Value],
    filter: &[GlobSet],
    unassigned: &mut Vec<Value>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<ModuleAssignments<'a>> {
    let mut matcher = LayerMatcher::new(config);
    let mut unassigned_files = 0usize;
    let mut assignments = BTreeMap::new();
    let mut module_by_id = BTreeMap::new();
    for module in modules {
        check()?;
        let path = module["identity"]["path"]
            .as_str()
            .context("store_unavailable: module path absent")?;
        let layer = matcher.select(path, work, check)?;
        assignments.insert(path, layer);
        module_by_id.insert(module["identity"].to_string(), module);
        // Owner ruling 2026-10-31 (#559): the list follows the source filter; the total does not.
        // Destinations still resolve across the snapshot.
        if layer.is_none() {
            unassigned_files += 1;
            if glob_matches(filter, path) {
                unassigned.push(module["identity"].clone());
            }
        }
    }
    Ok(ModuleAssignments {
        assignments,
        module_by_id,
        unassigned_files,
    })
}

/// The layer of every external package by identity. Packages with no layer are added to
/// `unassigned`.
fn assign_external<'a>(
    config: &'a Config,
    external_packages: &[Value],
    unassigned: &mut Vec<Value>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<BTreeMap<String, Option<&'a str>>> {
    let mut external_assignments = BTreeMap::new();
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
    Ok(external_assignments)
}

/// What the reference passes of `evaluate_prepared` read: the source filter and the layer of
/// every module and external package.
struct PolicyJoin<'a, 'e> {
    config: &'a Config,
    filter: &'e [GlobSet],
    scope: &'e AppScope<'e>,
    source_set: Option<&'e str>,
    assignments: &'e BTreeMap<&'a str, Option<&'a str>>,
    external_assignments: &'e BTreeMap<String, Option<&'a str>>,
    module_by_id: &'e BTreeMap<String, &'a Value>,
}

impl PolicyJoin<'_, '_> {
    /// The `$layers` and `$external` rows of one reference edge, pushed to `rows`.
    fn edge_rows(
        &self,
        edge: &Value,
        test: bool,
        rows: &mut Vec<Value>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let Self {
            config,
            filter,
            scope,
            source_set,
            assignments,
            external_assignments,
            module_by_id,
        } = *self;
        let source = &edge["source"];
        let target = &edge["target"];
        let path = source["path"]
            .as_str()
            .context("store_unavailable: edge source path absent")?;
        if !glob_matches(filter, path) {
            return Ok(());
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
            return Ok(());
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
        if !config.layers.is_empty() {
            let (status, mut unresolved) = layer_status(config, scope, source_layer, target_layer);
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
        Ok(())
    }

    /// The `$layers` rows of the unresolved references: one row a source and role, which keeps
    /// every unresolved site of that source.
    fn unresolved_rows(
        &self,
        refs: &Projection,
        instances: &mut Vec<Value>,
        test_only: &mut Vec<Value>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let Self {
            filter,
            assignments,
            module_by_id,
            ..
        } = *self;
        let mut unresolved_positions = BTreeMap::new();
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
            if !glob_matches(filter, path) {
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
            let rows = if test {
                &mut *test_only
            } else {
                &mut *instances
            };
            let key = (test, source.to_string());
            if let Some(index) = unresolved_positions.get(&key).copied() {
                merge_unresolved(&mut rows[index], &row, module, site);
            } else {
                unresolved_positions.insert(key, rows.len());
                rows.push(row);
            }
        }
        Ok(())
    }
}

/// One more unresolved site of a source that already has its row.
fn merge_unresolved(existing: &mut Value, row: &Value, module: &Value, site: &Value) {
    existing["referenceCount"] = json!(existing["referenceCount"].as_u64().unwrap_or(0) + 1);
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
}

/// The prepared declaration rows of the documents the filter selects.
fn select_prepared(
    base: &RulesBase,
    filter: &[GlobSet],
    instances: &mut Vec<Value>,
    test_only: &mut Vec<Value>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    // Prepared rows carry their document in `target`.
    for (prepared, rows) in [
        (&base.declarations, instances),
        (&base.test_declarations, test_only),
    ] {
        for row in prepared {
            check()?;
            let path = row["target"]["path"]
                .as_str()
                .context("store_unavailable: rule target path absent")?;
            if glob_matches(filter, path) {
                rows.push(row.clone());
            }
        }
    }
    Ok(())
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
