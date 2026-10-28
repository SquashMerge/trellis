//! View-time, source-witnessed module references. Structural mappings are not bindings.
use crate::model::{IndexPin, SourceFile};
use crate::native_evidence::{Call, Declaration, DeclarationRole, DocumentKey};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::{Node, Parser, Tree};
use work::Work;

pub mod external;
mod java;
mod javascript;
pub(crate) mod parallel_check;
mod python;
mod rust;
pub(crate) mod work;

const FOLLOW: usize = 4;

const SOURCE_BYTES: usize = 2 * 1024 * 1024;
const TOTAL_BYTES: usize = 256 * 1024 * 1024;
const VISITS: usize = 1_000_000;
const DEPTH: usize = 128;
const SITES: usize = 250_000;

pub struct DocumentInput {
    pub key: DocumentKey,
    pub file: SourceFile,
    pub declarations: Vec<Declaration>,
    pub calls: Vec<Call>,
}
pub struct Snapshot {
    pub pin: IndexPin,
    pub documents: Vec<DocumentInput>,
    pub manifests: Vec<external::ManifestInput>,
}
#[derive(Debug)]
pub struct Projection {
    pub data: Value,
    // Rules assign layers and report unassigned modules across the full snapshot.
    pub(crate) modules: Vec<Value>,
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
struct Identity {
    source_set_id: String,
    language: String,
    path: String,
    sid: String,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Site {
    owner_syntax_id: Option<String>,
    role: Option<&'static str>,
    range: crate::native_evidence::Range,
    lines: Option<Lines>,
    relation_kind: &'static str,
    spelling: String,
    call_id: Option<String>,
}
#[derive(Clone, Serialize)]
struct Lines {
    start: usize,
    end: usize,
}
struct RawSite {
    start: usize,
    end: usize,
    kind: &'static str,
    spelling: String,
    destination: std::result::Result<Vec<String>, &'static str>,
    namespace: Option<u8>,
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    Native(usize),
    External(external::ExternalIdentity),
}
#[derive(Default)]
struct SiteResolution {
    targets: BTreeSet<Target>,
    proof: BTreeSet<usize>,
    residual: Option<&'static str>,
}
impl SiteResolution {
    fn unknown(reason: &'static str) -> Self {
        Self {
            residual: Some(reason),
            ..Self::default()
        }
    }
}

/// The closed path-glob grammar shared by architecture request surfaces.
pub fn valid_path_glob(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['\\', '\0', ':', '[', ']', '{', '}', '!'])
        && !path
            .split('/')
            .any(|part| matches!(part, "" | "." | "..") || (part.contains("**") && part != "**"))
        && globset::GlobBuilder::new(path)
            .literal_separator(true)
            .backslash_escape(false)
            .build()
            .is_ok()
}

pub fn project(
    snapshot: &Snapshot,
    paths: &[String],
    sample_limit: usize,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Projection> {
    project_configured(
        snapshot,
        paths,
        sample_limit,
        crate::parallel::default_workers(),
        check,
    )
    .map(|(projection, _)| projection)
}

#[cfg(any(test, feature = "test-support"))]
pub fn project_with_workers(
    snapshot: &Snapshot,
    paths: &[String],
    sample_limit: usize,
    workers: usize,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Projection> {
    project_configured(snapshot, paths, sample_limit, workers, check)
        .map(|(projection, _)| projection)
}

fn project_configured(
    snapshot: &Snapshot,
    paths: &[String],
    sample_limit: usize,
    workers: usize,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<(Projection, usize)> {
    let request = parallel_check::RequestCheck::new(check);
    let workers = workers.clamp(1, crate::parallel::MAX_WORKERS);
    let result = project_inner(snapshot, paths, sample_limit, workers, &request);
    request.result(result)
}

fn project_inner(
    snapshot: &Snapshot,
    paths: &[String],
    sample_limit: usize,
    workers: usize,
    request: &parallel_check::RequestCheck<'_>,
) -> Result<(Projection, usize)> {
    let check = &mut request.worker();
    ensure!(
        (1..=50).contains(&sample_limit),
        "invalid_request: reference sample limit"
    );
    let mut globs = globset::GlobSetBuilder::new();
    for path in paths {
        check()?;
        ensure!(
            valid_path_glob(path),
            "invalid_request: reference path glob"
        );
        globs.add(
            globset::GlobBuilder::new(path)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .context("invalid_request: reference path glob")?,
        );
    }
    let globs = globs.build()?;
    let mut docs: Vec<_> = snapshot.documents.iter().collect();
    docs.sort_by(|a, b| a.key.cmp(&b.key));
    let mut identities = Vec::new();
    let mut modules = Vec::new();
    let mut trees = Vec::new();
    let mut total_bytes = 0usize;
    let mut document_map = BTreeMap::new();
    for manifest in &snapshot.manifests {
        check()?;
        let bytes = manifest.bytes.as_ref().map_or(0, Vec::len);
        total_bytes = total_bytes
            .checked_add(bytes)
            .context("bounded_work: reference bytes")?;
        ensure!(
            bytes <= SOURCE_BYTES && total_bytes <= TOTAL_BYTES,
            "bounded_work: reference source bytes"
        );
    }
    for (i, doc) in docs.iter().enumerate() {
        check()?;
        ensure!(
            doc.key.path == doc.file.path && doc.key.language == doc.file.language,
            "store_unavailable: reference document identity"
        );
        ensure!(
            document_map.insert(doc.key.clone(), i).is_none(),
            "store_unavailable: duplicate reference document"
        );
        let roots: Vec<_> = doc
            .declarations
            .iter()
            .filter(|d| d.kind == "module" && d.ancestors.is_empty())
            .collect();
        ensure!(roots.len() == 1, "store_unavailable: reference file module");
        let root = roots[0];
        let identity = Identity {
            source_set_id: doc.key.source_set_id.clone(),
            language: doc.key.language.clone(),
            path: doc.key.path.clone(),
            sid: root.syntax_id.clone(),
        };
        modules.push(json!({"identity":identity,"revisionId":root.revision_id,
            "contentHash":doc.file.hash,"provenanceId":root.provenance_id}));
        identities.push(identity);
        total_bytes = total_bytes
            .checked_add(doc.file.text.len())
            .context("bounded_work: reference bytes")?;
        ensure!(
            doc.file.text.len() <= SOURCE_BYTES && total_bytes <= TOTAL_BYTES,
            "bounded_work: reference source bytes"
        );
    }
    for batch in docs.chunks(workers) {
        trees.extend(map_batch(batch, workers, request, |doc| {
            parse(doc, &mut request.worker())
        })?);
    }
    let selected: Vec<_> = docs
        .iter()
        .enumerate()
        .filter_map(|(i, doc)| (paths.is_empty() || globs.is_match(&doc.key.path)).then_some(i))
        .collect();
    let limit = work::reference_limit(docs.len(), total_bytes);
    let catalogs = map_batch(&[0u8, 1, 2, 3], workers, request, |language| {
        let mut check = request.worker();
        let mut work = Work::new(limit);
        let catalog = match language {
            0 => {
                let packages =
                    external::RustPackages::new(&snapshot.manifests, &mut work, &mut check)?;
                let catalog = rust::Catalog::new(&docs, &trees, &document_map, work, &mut check)?;
                let used = catalog.work.used;
                return Ok((CatalogJob::Rust(Box::new((catalog, packages))), used));
            }
            1 => CatalogJob::Java(Box::new(java::Catalog::new(
                &docs, &trees, &selected, &mut work, &mut check,
            )?)),
            2 => CatalogJob::Python(Box::new(python::Catalog::new(
                &docs,
                &trees,
                &document_map,
                &selected,
                &mut work,
                &mut check,
            )?)),
            3 => {
                let catalog = if selected
                    .iter()
                    .any(|&i| docs[i].key.language == "javascript")
                {
                    let packages = external::JavascriptPackages::new(
                        &snapshot.manifests,
                        &mut work,
                        &mut check,
                    )?;
                    Some(javascript::Catalog::new(
                        &docs,
                        &trees,
                        &document_map,
                        &selected,
                        &packages,
                        &mut work,
                        &mut check,
                    )?)
                } else {
                    None
                };
                CatalogJob::Javascript(Box::new(catalog))
            }
            _ => unreachable!(),
        };
        Ok((catalog, work.used))
    })?;
    let mut work = Work::new(limit);
    let mut catalogs = catalogs.into_iter();
    let mut admit = |(catalog, used)| -> Result<CatalogJob> {
        merge_work(&mut work, used, check)?;
        Ok(catalog)
    };
    let CatalogJob::Rust(rust_job) = admit(catalogs.next().expect("Rust catalog"))? else {
        unreachable!()
    };
    let (mut rust, packages) = *rust_job;
    let CatalogJob::Java(java) = admit(catalogs.next().expect("Java catalog"))? else {
        unreachable!()
    };
    let CatalogJob::Python(python) = admit(catalogs.next().expect("Python catalog"))? else {
        unreachable!()
    };
    let CatalogJob::Javascript(javascript) = admit(catalogs.next().expect("JavaScript catalog"))?
    else {
        unreachable!()
    };
    let javascript = *javascript;
    rust.work = work;
    let mut external_packages = BTreeMap::new();
    let mut edges: BTreeMap<(usize, Target, bool), (usize, Vec<Site>)> = BTreeMap::new();
    let mut emitted_modules = BTreeSet::new();
    let mut unresolved = Vec::new();
    let mut site_count = 0usize;
    let mut scanned = 0usize;
    let mut mapped = 0usize;
    for batch in selected.chunks(workers) {
        let remaining = rust.work.remaining();
        let prepared = map_batch(batch, workers, request, |&source| {
            prepare_file(
                source,
                docs[source],
                &trees[source],
                remaining,
                &mut request.worker(),
            )
        })?;
        for prepared in prepared {
            let PreparedFile {
                source,
                work,
                sites,
            } = prepared;
            let doc = docs[source];
            request.check()?;
            merge_work(&mut rust.work, work.used, check)?;
            site_count = site_count.saturating_add(sites.len());
            ensure!(site_count <= SITES, "bounded_work: reference site count");
            emitted_modules.insert(source);
            scanned += 1;
            for (item, site) in sites {
                check()?;
                let role = site.role;
                let resolution = if site.owner_syntax_id.is_none() {
                    SiteResolution::unknown("reference_owner_ambiguous")
                } else if doc.key.language == "rust" {
                    rust.resolve(
                        source,
                        &item,
                        &packages,
                        &mut external_packages,
                        &docs,
                        check,
                    )?
                } else if doc.key.language == "java" {
                    java.resolve(source, &item, &mut external_packages, &mut rust.work, check)?
                } else if doc.key.language == "python" {
                    python.resolve(source, &item, &mut external_packages, &mut rust.work, check)?
                } else if doc.key.language == "javascript" {
                    javascript
                        .as_ref()
                        .expect("selected JavaScript catalog")
                        .resolve(source, &item, &mut external_packages, &mut rust.work, check)?
                } else {
                    SiteResolution::unknown("module_mapping_unavailable")
                };
                if !resolution.targets.is_empty() {
                    mapped += 1;
                    emitted_modules.extend(resolution.proof);
                }
                for target in resolution.targets {
                    owner_step(&mut rust.work, check)?;
                    let edge = edges
                        .entry((source, target, role == Some("test")))
                        .or_default();
                    edge.0 += 1;
                    if edge.1.len() < sample_limit {
                        edge.1.push(site.clone());
                    }
                }
                if let Some(reason) = resolution.residual {
                    unresolved
                        .push(json!({"source":identities[source],"site":site,"reason":reason}));
                }
            }
        }
    }
    let mut production = Vec::new();
    let mut tests = Vec::new();
    for ((source, target, test), (count, sites)) in edges {
        check()?;
        let target = match target {
            Target::Native(target) => {
                emitted_modules.insert(target);
                json!(identities[target])
            }
            Target::External(identity) => json!(identity),
        };
        let omitted = count - sites.len();
        let edge = json!({"source":identities[source],"target":target,"count":count,
            "sites":sites,"omittedSites":omitted,"mapping":"structural"});
        if test {
            tests.push(edge);
        } else {
            production.push(edge);
        }
    }
    let emitted_modules: Vec<_> = emitted_modules.into_iter().map(|i| &modules[i]).collect();
    request.check()?;
    Ok((
        Projection {
            data: json!({
                "referencesVersion":"references:v2","indexGeneration":snapshot.pin.index_generation.to_string(),
                "indexRevision":snapshot.pin.index_revision,"evidenceTier":"syntax","semanticComplete":false,
                "modules":emitted_modules,"externalPackages":external_packages.into_values().collect::<Vec<_>>(),"production":production,"tests":tests,"unresolved":unresolved,
                "counts":{"scannedDocuments":scanned,"mappedSites":mapped,"unresolvedSites":unresolved.len()},
                "limitations":[
                    "Structural file conventions are not compiler, package, type or call resolution; missing sites do not prove absence.",
                    "Rust expands captured public names through proved lexical modules and bounded import/reexport chains; cfg, macros, unproved ownership and unknown in-repo names remain uncertain. Declared external roots identify packages only.",
                    "JavaScript expands captured named ESM exports through bounded relative forwarding; stars exclude default. Ambiguous names, dynamic namespaces and unproved bindings remain uncertain. Static sources retain module evidence; declared npm roots and the partial pinned Node catalog identify packages only.",
                    "Python expands proved checkout-root modules and package exports with bounded import forwarding and literal-list __all__; dynamic namespaces, ambiguous bindings and unproved package layouts remain uncertain. The pinned CPython 3.12 stdlib catalog labels roots only; third-party distribution mappings remain unknown.",
                    "Java expands captured public top-level types from declared packages, with explicit and lexical shadowing; static/member types and unproved third-party prefixes remain unresolved. Java labels identify package families only; javax requires an exact package in the pinned OpenJDK 25 catalog, not proof of members or installed-JDK availability."
                ]
            }),
            modules,
        },
        rust.work.used,
    ))
}

enum CatalogJob {
    Rust(Box<(rust::Catalog, external::RustPackages)>),
    Java(Box<java::Catalog>),
    Python(Box<python::Catalog>),
    Javascript(Box<Option<javascript::Catalog>>),
}

struct PreparedFile {
    source: usize,
    work: Work,
    sites: Vec<(RawSite, Site)>,
}

fn merge_work(work: &mut Work, used: usize, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
    check()?;
    ensure!(work.charge(used), "bounded_work: reference owner lookup");
    Ok(())
}

fn prepare_file(
    source: usize,
    doc: &DocumentInput,
    tree: &Tree,
    limit: usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<PreparedFile> {
    let mut work = Work::new(limit);
    let raw = extract(doc, tree, &mut work, check)?;
    let declarations =
        RangeIndex::new(doc.declarations.iter().map(|d| &d.range), &mut work, check)?;
    let calls = RangeIndex::new(doc.calls.iter().map(|c| &c.range), &mut work, check)?;
    let newlines: Vec<_> = doc
        .file
        .text
        .bytes()
        .enumerate()
        .filter_map(|(i, b)| (b == b'\n').then_some(i))
        .collect();
    let mut seen = BTreeSet::new();
    let mut sites = Vec::new();
    for item in raw {
        check()?;
        if !seen.insert((item.start, item.end, item.kind, item.spelling.clone())) {
            continue;
        }
        let owner = owner(
            doc,
            declarations.enclosing(item.start, item.end, &mut work, check)?,
        );
        let role = owner.map(|d| match d.role {
            DeclarationRole::Test => "test",
            DeclarationRole::Production => "production",
        });
        let site = Site {
            owner_syntax_id: owner.map(|d| d.syntax_id.clone()),
            role,
            range: crate::native_evidence::Range {
                start: item.start,
                end: item.end,
            },
            lines: (!doc.file.text.is_empty()).then(|| Lines {
                start: newlines.partition_point(|&p| p < item.start) + 1,
                end: newlines.partition_point(|&p| p < item.end.saturating_sub(1)) + 1,
            }),
            relation_kind: item.kind,
            spelling: item.spelling.clone(),
            call_id: calls
                .enclosing(item.start, item.end, &mut work, check)?
                .into_iter()
                .min()
                .map(|index| doc.calls[index].id.clone()),
        };
        sites.push((item, site));
    }
    Ok(PreparedFile {
        source,
        work,
        sites,
    })
}

fn map_batch<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    request: &parallel_check::RequestCheck<'_>,
    prepare: impl Fn(&T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    request.check()?;
    let results =
        crate::parallel::map_ordered(items, workers, &request.stopped, |_, item| prepare(item));
    results
        .into_iter()
        .map(|slot| slot.context("reference request stopped")?)
        .collect()
}

fn grammar(language: &str) -> Result<tree_sitter::Language> {
    Ok(match language {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        _ => anyhow::bail!("store_unavailable: reference language"),
    })
}
fn parse(doc: &DocumentInput, check: &mut impl FnMut() -> Result<()>) -> Result<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&grammar(&doc.key.language)?)?;
    let mut aborted = None;
    let tree = parser.parse_with_options(
        &mut |offset, _| &doc.file.text.as_bytes()[offset..],
        None,
        Some(
            tree_sitter::ParseOptions::new().progress_callback(&mut |_| {
                if let Err(error) = check() {
                    aborted = Some(error);
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            }),
        ),
    );
    if let Some(error) = aborted {
        return Err(error);
    }
    let tree = tree.context("bounded_work: reference parser stopped")?;
    ensure!(!tree.root_node().has_error(), "invalid_source_syntax");
    walk(tree.root_node(), check, &mut |node, _check| {
        ensure!(
            !node.is_error() && !node.is_missing(),
            "invalid_source_syntax"
        );
        Ok(false)
    })?;
    Ok(tree)
}
fn walk<'a, C: FnMut() -> Result<()>>(
    root: Node<'a>,
    check: &mut C,
    visit: &mut impl FnMut(Node<'a>, &mut C) -> Result<bool>,
) -> Result<()> {
    let mut stack = vec![(root, 0)];
    let mut visits = 0;
    while let Some((node, depth)) = stack.pop() {
        check()?;
        visits += 1;
        ensure!(
            visits <= VISITS && depth <= DEPTH,
            "bounded_work: reference AST"
        );
        if !visit(node, check)? {
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                stack.push((child, depth + 1));
            }
        }
    }
    Ok(())
}
fn text<'a>(doc: &'a DocumentInput, node: Node<'_>) -> &'a str {
    &doc.file.text[node.byte_range()]
}
struct RangeIndex<'a> {
    ranges: Vec<(usize, &'a crate::native_evidence::Range)>,
    max_ends: Vec<usize>,
}
impl<'a> RangeIndex<'a> {
    fn new(
        ranges: impl Iterator<Item = &'a crate::native_evidence::Range>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut indexed = Vec::new();
        for (index, range) in ranges.enumerate() {
            owner_step(work, check)?;
            indexed.push((index, range));
        }
        indexed.sort_by_key(|(index, range)| (range.start, *index));
        check()?;
        let mut result = Self {
            max_ends: vec![0; indexed.len()],
            ranges: indexed,
        };
        result.build(0, result.ranges.len(), work, check)?;
        Ok(result)
    }

    fn build(
        &mut self,
        left: usize,
        right: usize,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<usize> {
        if left == right {
            return Ok(0);
        }
        owner_step(work, check)?;
        let mid = left + (right - left) / 2;
        let end = self.ranges[mid]
            .1
            .end
            .max(self.build(left, mid, work, check)?)
            .max(self.build(mid + 1, right, work, check)?);
        self.max_ends[mid] = end;
        Ok(end)
    }

    fn enclosing(
        &self,
        start: usize,
        end: usize,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Vec<usize>> {
        let mut found = Vec::new();
        let mut pending = vec![(0, self.ranges.len())];
        while let Some((left, right)) = pending.pop() {
            if left == right {
                continue;
            }
            owner_step(work, check)?;
            let mid = left + (right - left) / 2;
            // A spanning file module must not prevent pruning unrelated subtrees.
            if self.ranges[left].1.start > start || self.max_ends[mid] < end {
                continue;
            }
            let (index, range) = self.ranges[mid];
            if range.start <= start && range.end >= end {
                found.push(index);
            }
            pending.push((mid + 1, right));
            pending.push((left, mid));
        }
        Ok(found)
    }
}

fn owner_step(work: &mut Work, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
    check()?;
    ensure!(work.charge(1), "bounded_work: reference owner lookup");
    Ok(())
}

fn owner(doc: &DocumentInput, mut owners: Vec<usize>) -> Option<&Declaration> {
    owners.sort_by_key(|&index| {
        let d = &doc.declarations[index];
        (
            d.range.end - d.range.start,
            d.kind == "module",
            std::cmp::Reverse(d.ancestors.len()),
            index,
        )
    });
    let first = &doc.declarations[*owners.first()?];
    if owners.get(1).is_some_and(|&index| {
        let d = &doc.declarations[index];
        d.range == first.range && d.kind == first.kind && d.ancestors.len() == first.ancestors.len()
    }) {
        None
    } else {
        Some(first)
    }
}
fn opaque(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "macro_invocation"
            | "macro_definition"
            | "token_tree"
            | "attribute_item"
            | "inner_attribute_item"
    )
}
fn raw(
    doc: &DocumentInput,
    node: Node<'_>,
    kind: &'static str,
    destination: std::result::Result<Vec<String>, &'static str>,
) -> RawSite {
    RawSite {
        start: node.start_byte(),
        end: node.end_byte(),
        kind,
        spelling: text(doc, node).to_owned(),
        destination,
        namespace: None,
    }
}
fn extract(
    doc: &DocumentInput,
    tree: &Tree,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<RawSite>> {
    if doc.key.language == "rust" {
        return rust::extract(doc, tree, work, check);
    }
    if doc.key.language == "java" {
        return java::extract(doc, tree, work, check);
    }
    if doc.key.language == "python" {
        return python::extract(doc, tree, work, check);
    }
    if doc.key.language == "javascript" {
        return javascript::extract(doc, tree, work, check);
    }
    Ok(Vec::new())
}
fn find(
    doc: &DocumentInput,
    candidates: &[String],
    documents: &BTreeMap<DocumentKey, usize>,
) -> Vec<usize> {
    candidates
        .iter()
        .filter_map(|path| {
            documents
                .get(&DocumentKey {
                    source_set_id: doc.key.source_set_id.clone(),
                    language: doc.key.language.clone(),
                    path: path.clone(),
                })
                .copied()
        })
        .collect()
}
fn relative_path(source: &str, relative: &str) -> Option<String> {
    let mut parts: Vec<_> = source.split('/').collect();
    parts.pop();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod range_index_tests {
    use super::*;
    use crate::native_evidence::Range;

    #[test]
    fn indexed_enclosure_matches_linear_ranges_and_stored_order() {
        let ranges = [
            Range { start: 0, end: 100 },
            Range { start: 20, end: 40 },
            Range { start: 5, end: 30 },
            Range { start: 20, end: 40 },
            Range { start: 40, end: 40 },
            Range { start: 60, end: 80 },
        ];
        let mut work = Work::new(5_000_000);
        let index = RangeIndex::new(ranges.iter(), &mut work, &mut || Ok(())).unwrap();
        for start in 0..=101 {
            for end in start..=101 {
                let expected: Vec<_> = ranges
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.start <= start && r.end >= end)
                    .map(|(i, _)| i)
                    .collect();
                let mut found = index
                    .enclosing(start, end, &mut work, &mut || Ok(()))
                    .unwrap();
                // Call enclosure chooses the first stored call, not the narrowest.
                assert_eq!(found.iter().min(), expected.first());
                found.sort_unstable();
                assert_eq!(found, expected, "{start}..{end}");
            }
        }
    }

    #[test]
    fn range_index_work_is_bounded_and_cancellable() {
        let ranges = [Range { start: 0, end: 100 }];
        let mut work = Work::new(5_000_000);
        let index = RangeIndex::new(ranges.iter(), &mut work, &mut || Ok(())).unwrap();
        work.used = work.limit;
        let error = index
            .enclosing(5, 10, &mut work, &mut || Ok(()))
            .unwrap_err();
        assert_eq!(error.to_string(), "bounded_work: reference owner lookup");
        let mut checks = 0;
        let error = RangeIndex::new(ranges.iter(), &mut Work::new(5_000_000), &mut || {
            checks += 1;
            ensure!(checks < 3, "cancelled");
            Ok(())
        })
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "cancelled");
        let error = index
            .enclosing(5, 10, &mut Work::new(5_000_000), &mut || {
                anyhow::bail!("cancelled")
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
    }
}

#[cfg(test)]
mod parallel_tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    fn snapshot() -> Snapshot {
        let root = tempfile::tempdir().unwrap();
        for (path, text) in [
            (
                "src/lib.rs",
                "mod provider; pub use provider::value; pub fn run() { value(); }",
            ),
            ("src/provider.rs", "pub fn value() {}"),
            (
                "Use.java",
                "package app; import app.Provider; public class Use { Provider value; }",
            ),
            ("Provider.java", "package app; public class Provider {}"),
            ("use.py", "from provider import value\nvalue()\n"),
            ("provider.py", "def value():\n    return 1\n"),
            ("use.js", "import { value } from './provider.js'; value();"),
            ("provider.js", "export function value() {}"),
        ] {
            let file = root.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
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
                    .find(|doc| doc.key.path == file.path)
                    .unwrap()
                    .key
                    .clone();
                let declarations = native
                    .declarations
                    .iter()
                    .filter(|declaration| declaration.document == key)
                    .cloned()
                    .collect();
                let calls = native
                    .calls
                    .iter()
                    .filter(|call| call.document == key)
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
            pin: IndexPin {
                index_generation: uuid::Uuid::new_v4(),
                index_revision: 1,
            },
            documents,
            manifests: Vec::new(),
        }
    }

    #[test]
    fn serial_and_parallel_keep_payload_and_exact_work_counts() {
        let snapshot = snapshot();
        for paths in [vec![], vec!["src/lib.rs".into(), "use.*".into()]] {
            let (serial, used) =
                project_configured(&snapshot, &paths, 1, 1, &mut || Ok(())).unwrap();
            assert!(used > 0);
            for workers in [2, 8] {
                let (parallel, parallel_used) =
                    project_configured(&snapshot, &paths, 1, workers, &mut || Ok(())).unwrap();
                assert_eq!(parallel_used, used);
                assert_eq!(
                    serde_json::to_vec(&parallel.data).unwrap(),
                    serde_json::to_vec(&serial.data).unwrap()
                );
                assert_eq!(parallel.modules, serial.modules);
            }
        }
    }

    #[test]
    fn parallel_parse_and_preparation_overlap_and_return_in_source_order() {
        let snapshot = snapshot();
        let docs = &snapshot.documents[..2];
        let (tx, rx) = std::sync::mpsc::channel();
        let rx = Mutex::new(rx);
        let mut check = || Ok(());
        let request = parallel_check::RequestCheck::new(&mut check);
        let trees = map_batch(&[0usize, 1], 2, &request, |&index| {
            if index == 0 {
                rx.lock().unwrap().recv().unwrap();
            }
            let tree = parse(&docs[index], &mut request.worker())?;
            if index == 1 {
                tx.send(()).unwrap();
            }
            Ok(tree)
        })
        .unwrap();
        assert_eq!(trees[0].root_node().end_byte(), docs[0].file.text.len());
        assert_eq!(trees[1].root_node().end_byte(), docs[1].file.text.len());
        let prepared = map_batch(&[0usize, 1], 2, &request, |&index| {
            if index == 0 {
                rx.lock().unwrap().recv().unwrap();
            }
            let prepared = prepare_file(
                index,
                &docs[index],
                &trees[index],
                5_000_000,
                &mut request.worker(),
            )?;
            if index == 1 {
                tx.send(()).unwrap();
            }
            Ok(prepared)
        })
        .unwrap();
        assert_eq!(prepared[0].source, 0);
        assert_eq!(prepared[1].source, 1);
        assert!(prepared.iter().all(|file| file.work.used > 0));
    }

    #[test]
    fn independent_catalog_jobs_overlap() {
        let snapshot = snapshot();
        let mut docs: Vec<_> = snapshot.documents.iter().collect();
        docs.sort_by(|a, b| a.key.cmp(&b.key));
        let trees: Vec<_> = docs
            .iter()
            .map(|doc| parse(doc, &mut || Ok(())).unwrap())
            .collect();
        let documents = docs
            .iter()
            .enumerate()
            .map(|(i, doc)| (doc.key.clone(), i))
            .collect();
        let selected: Vec<_> = (0..docs.len()).collect();
        let (tx, rx) = std::sync::mpsc::channel();
        let rx = Mutex::new(rx);
        let mut check = || Ok(());
        let request = parallel_check::RequestCheck::new(&mut check);
        let catalogs = map_batch(&[0usize, 1], 2, &request, |&language| {
            let mut check = request.worker();
            let mut work = Work::new(5_000_000);
            if language == 0 {
                rx.lock().unwrap().recv().unwrap();
                let packages = external::RustPackages::new(&[], &mut work, &mut check)?;
                let catalog = rust::Catalog::new(&docs, &trees, &documents, work, &mut check)?;
                Ok(CatalogJob::Rust(Box::new((catalog, packages))))
            } else {
                let catalog = java::Catalog::new(&docs, &trees, &selected, &mut work, &mut check)?;
                tx.send(()).unwrap();
                Ok(CatalogJob::Java(Box::new(catalog)))
            }
        })
        .unwrap();
        assert!(matches!(&catalogs[0], CatalogJob::Rust(_)));
        assert!(matches!(&catalogs[1], CatalogJob::Java(_)));
    }

    #[test]
    fn ordered_failure_stops_before_next_batch_and_work_is_bounded() {
        let visited = AtomicUsize::new(0);
        let mut check = || Ok(());
        let request = parallel_check::RequestCheck::new(&mut check);
        let result: Result<()> = (|| {
            for batch in [0usize, 1, 2].chunks(2) {
                map_batch(batch, 2, &request, |&index| -> Result<()> {
                    visited.fetch_add(1, Ordering::Relaxed);
                    if index == 0 {
                        anyhow::bail!("first source failure")
                    }
                    anyhow::bail!("later source failure")
                })?;
            }
            Ok(())
        })();
        assert_eq!(result.unwrap_err().to_string(), "first source failure");
        assert_eq!(visited.load(Ordering::Relaxed), 2);
        let snapshot = snapshot();
        let doc = &snapshot.documents[0];
        let tree = parse(doc, &mut || Ok(())).unwrap();
        let error = prepare_file(0, doc, &tree, 0, &mut || Ok(()))
            .err()
            .unwrap();
        assert!(error.to_string().starts_with("bounded_work:"));
        let mut total = Work::new(3);
        merge_work(&mut total, 2, &mut || Ok(())).unwrap();
        assert!(merge_work(&mut total, 2, &mut || Ok(())).is_err());
        assert_eq!(total.used, 4);
    }
}
