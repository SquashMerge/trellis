//! View-time, source-witnessed module references. Structural mappings are not bindings.
use crate::model::{IndexPin, SourceFile};
use crate::native_evidence::{Call, Declaration, DeclarationRole, DocumentKey};
use crate::syntax_bounds::{self, DEPTH, VISITS};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::{Node, Parser, Tree};
use work::Work;

pub(crate) mod callers;
pub mod external;
mod java;
mod javascript;
pub(crate) mod parallel_check;
mod python;
mod rust;
pub mod tsconfig;
pub(crate) mod work;

const FOLLOW: usize = 4;

const SOURCE_BYTES: usize = 2 * 1024 * 1024;
const TOTAL_BYTES: usize = 256 * 1024 * 1024;
const SITES: usize = 250_000;

pub struct DocumentInput {
    pub key: DocumentKey,
    pub file: SourceFile,
    pub declarations: Vec<Declaration>,
    pub calls: Vec<Call>,
    /// Selected current or explicitly labelled historical producer evidence.
    pub semantic: DocumentSemantic,
}
pub struct Snapshot {
    pub pin: IndexPin,
    pub documents: Vec<DocumentInput>,
    pub manifests: Vec<external::ManifestInput>,
    /// The root `tsconfig.json`, else `jsconfig.json`, when the revision captured one.
    pub root_tsconfig: Option<tsconfig::RootTsconfigInput>,
    /// Documents published with no evidence but their file module, and their `partialFiles`
    /// reason: over the evidence budget (#564), or not read (#576).
    pub evidence_gaps: BTreeMap<DocumentKey, &'static str>,
}

/// Selected producer evidence: current fresh bindings or an authorized historical overlay.
/// Historical outcomes retain P authority and cannot be re-resolved against native N.
#[derive(Debug, Default, Clone)]
pub struct DocumentSemantic {
    /// The selected producer, when the document has a usable coverage tuple.
    pub producer_id: Option<String>,
    /// The selected coverage tuple is complete, not partial.
    pub complete: bool,
    /// Call bindings of measured calls and resolved semantic references, by source range.
    pub anchors: Vec<SemanticAnchor>,
    /// Fresh `extends`/`implements` relationships whose source is declared here.
    pub relationships: Vec<SemanticRelationship>,
    /// Native-N selection of a validated historical semantic basis, or explicit pending.
    pub projection: Option<crate::model::SemanticProjection>,
    /// The original coverage status, including unavailable/not-requested evidence.
    pub coverage_state: Option<String>,
    pub boundary: bool,
}

/// One bound name or call in a document.
#[derive(Debug, Clone)]
pub struct SemanticAnchor {
    /// The producer's joined source range: a callee or invocation, or a referenced name.
    pub range: crate::native_evidence::Range,
    /// The measured call of a call binding; `None` for a semantic reference.
    pub call_id: Option<String>,
    /// A semantic reference with the `type` role.
    pub type_role: bool,
    /// A call binding whose dispatch is direct or a constructor.
    pub direct: bool,
    pub outcome: SemanticOutcome,
    pub provenance_id: String,
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticOutcome {
    /// A declared internal target: its document's index in `Snapshot::documents` and its
    /// syntax ID.
    Internal(usize, String),
    /// Bound outside the checkout.
    External,
    Ambiguous,
    Unresolved,
    /// An endpoint is not eligible against native N; never resolve it by spelling.
    Boundary,
}

/// A fresh semantic type relationship from a declaration of this document.
#[derive(Debug, Clone)]
pub struct SemanticRelationship {
    /// `extends` or `implements`.
    pub kind: String,
    pub source_syntax_id: String,
    /// The internal target, or `None` for an external one.
    pub target: Option<(usize, String)>,
    pub provenance_id: String,
    pub metadata: BTreeMap<String, Value>,
}
#[derive(Debug)]
pub struct Projection {
    pub data: Value,
    // Rules assign layers across the full snapshot, whatever the source filter selects.
    pub(crate) modules: Vec<Value>,
    /// Exact reference-site coverage per source path and role (test = true):
    /// scanned sites, sites with at least one mapped target, and unresolved sites.
    /// Architecture sums it per layer; it is bounded by the scanned documents.
    pub(crate) coverage: BTreeMap<(String, bool), SiteCoverage>,
}
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SiteCoverage {
    pub sites: usize,
    pub mapped: usize,
    pub unresolved: usize,
    /// Sites mapped or bound outside the checkout by a fresh semantic binding (#523).
    pub semantic: usize,
}
/// Every resolved reference of one revision, by source document, before path selection and
/// sampling (#556). It keeps no source text, so a checkout can hold it between reads.
/// `select` turns it into a `Projection`; `project` is exactly these two steps.
pub struct ReferenceBase {
    pin: IndexPin,
    /// Every snapshot document feeds resolution, so path selection does not narrow these.
    partial_files: Vec<syntax_bounds::PartialFile>,
    identities: Vec<Identity>,
    modules: Vec<Value>,
    /// The sources in `identities` order; a source outside the scan stays `None`.
    sources: Vec<Option<SourceReferences>>,
    /// Language to selected producer.
    producers: BTreeMap<String, String>,
    semantic_documents: Vec<Value>,
    /// The most sites each edge keeps; `select` samples at most this many.
    sample_limit: usize,
    work_used: usize,
}
/// One source document's resolved sites.
struct SourceReferences {
    projection: Option<crate::model::SemanticProjection>,
    edges: BTreeMap<(Target, bool), EdgeSites>,
    unresolved: Vec<(Site, &'static str)>,
    /// Modules that prove this source's mapped sites.
    proof: BTreeSet<usize>,
    packages: BTreeMap<external::ExternalIdentity, external::ExternalPackage>,
    /// Coverage per role (test = true).
    coverage: BTreeMap<bool, SiteCoverage>,
    mapped: usize,
    semantic: usize,
}
/// The sites of one (target, role) edge: exact counts, and call-first samples (R5).
#[derive(Default)]
struct EdgeSites {
    count: usize,
    semantic: usize,
    kinds: KindCounts,
    calls: Vec<Site>,
    others: Vec<Site>,
}
/// What a site is: the closed `relationKind` vocabulary.
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum RelationKind {
    Reference,
    Import,
    Call,
    Type,
}
impl RelationKind {
    fn from_raw(kind: &'static str) -> Self {
        match kind {
            "import" => Self::Import,
            "call" => Self::Call,
            "type" => Self::Type,
            _ => Self::Reference,
        }
    }
}
/// Exact site counts per relation kind of one edge or aggregated row.
#[derive(Clone, Copy, Default, Serialize)]
struct KindCounts {
    call: usize,
    r#type: usize,
    import: usize,
    reference: usize,
}
impl KindCounts {
    fn add(&mut self, kind: RelationKind) {
        match kind {
            RelationKind::Call => self.call += 1,
            RelationKind::Type => self.r#type += 1,
            RelationKind::Import => self.import += 1,
            RelationKind::Reference => self.reference += 1,
        }
    }
}
/// The declaration that encloses a site.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SiteOwner {
    syntax_id: String,
    name: Option<String>,
    stable_lookup_key: Option<String>,
}
/// The declaration a semantic binding resolves a site to.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReferenceTarget {
    syntax_id: String,
    display_key: String,
    document: DocumentKey,
    lines: Lines,
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
    relation_kind: RelationKind,
    spelling: String,
    call_id: Option<String>,
    caller: Option<callers::Caller>,
    caller_reason: Option<&'static str>,
    owner: Option<SiteOwner>,
    /// Set only when a fresh semantic binding resolves the site to an indexed declaration.
    target: Option<ReferenceTarget>,
    /// Why `target` is null: `structural_only`, `unresolved`, `external` or `ambiguous`.
    target_reason: Option<&'static str>,
    /// `structural` (captured syntax conventions) or `semantic` (a fresh producer binding).
    mapping: &'static str,
    /// The semantic binding's provenance; null for structural sites.
    provenance_id: Option<String>,
    #[serde(flatten)]
    semantic_metadata: BTreeMap<String, Value>,
}
#[derive(Clone, Copy, Serialize)]
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

/// A references `prefix` (#837): a checkout-relative directory of at most 4096 bytes with
/// `/` separators and an optional trailing `/`, and no empty, `.` or `..` segment,
/// backslash, `:` or NUL. Returns it normalized, without the trailing `/`.
pub fn directory_prefix(prefix: &str) -> Option<&str> {
    let normalized = prefix.strip_suffix('/').unwrap_or(prefix);
    (prefix.len() <= 4096
        && !normalized.is_empty()
        && !normalized.starts_with('/')
        && !normalized.contains(['\\', ':', '\0'])
        && normalized
            .split('/')
            .all(|part| !matches!(part, "" | "." | "..")))
    .then_some(normalized)
}

pub fn project(
    snapshot: &Snapshot,
    paths: &[String],
    sample_limit: usize,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Projection> {
    project_under(snapshot, paths, None, sample_limit, check)
}

/// [`project`] of only the sources under the directory `prefix` (#837): a normalized
/// checkout-relative directory without a trailing `/`.
pub fn project_under(
    snapshot: &Snapshot,
    paths: &[String],
    prefix: Option<&str>,
    sample_limit: usize,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Projection> {
    project_configured(
        snapshot,
        Sources { paths, prefix },
        sample_limit,
        crate::parallel::default_workers(),
        check,
    )
    .map(|(projection, _)| projection)
}

/// The sources a read selects: those any `paths` glob matches (every source without one),
/// and only under the directory `prefix` when it is given (#837).
#[derive(Clone, Copy)]
struct Sources<'a> {
    paths: &'a [String],
    prefix: Option<&'a str>,
}

impl Sources<'_> {
    fn all() -> Self {
        Sources {
            paths: &[],
            prefix: None,
        }
    }

    fn selects(&self, globs: &globset::GlobSet, path: &str) -> bool {
        (self.paths.is_empty() || globs.is_match(path))
            && self.prefix.is_none_or(|prefix| {
                path.strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
            })
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn project_with_workers(
    snapshot: &Snapshot,
    paths: &[String],
    sample_limit: usize,
    workers: usize,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Projection> {
    project_configured(
        snapshot,
        Sources {
            paths,
            prefix: None,
        },
        sample_limit,
        workers,
        check,
    )
    .map(|(projection, _)| projection)
}

#[cfg(feature = "test-support")]
static TEST_SITE_LIMIT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(SITES);

/// Lower the site budget, so a test's small revision can exceed it.
#[cfg(feature = "test-support")]
pub fn set_site_limit_for_tests(limit: usize) {
    TEST_SITE_LIMIT.store(limit, std::sync::atomic::Ordering::Release);
}

fn site_limit() -> usize {
    #[cfg(feature = "test-support")]
    return TEST_SITE_LIMIT.load(std::sync::atomic::Ordering::Acquire);
    #[cfg(not(feature = "test-support"))]
    SITES
}

/// The most sites a reference read may sample per edge.
pub const MAX_SAMPLE_LIMIT: usize = 50;

/// Resolve every document of `snapshot` for later selection by any paths and sample limit.
pub fn resolve_all(
    snapshot: &Snapshot,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<ReferenceBase> {
    let request = parallel_check::RequestCheck::new(check);
    let workers = crate::parallel::default_workers().clamp(1, crate::parallel::MAX_WORKERS);
    let result = resolve(
        snapshot,
        Sources::all(),
        MAX_SAMPLE_LIMIT,
        workers,
        &request,
    );
    request.result(result)
}

fn project_configured(
    snapshot: &Snapshot,
    sources: Sources<'_>,
    sample_limit: usize,
    workers: usize,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<(Projection, usize)> {
    let request = parallel_check::RequestCheck::new(check);
    let workers = workers.clamp(1, crate::parallel::MAX_WORKERS);
    let result = resolve(snapshot, sources, sample_limit, workers, &request).and_then(|base| {
        let projection = base.select_inner(sources, sample_limit, &mut request.worker())?;
        Ok((projection, base.work_used))
    });
    request.result(result)
}

fn path_globs(
    paths: &[String],
    check: &mut impl FnMut() -> Result<()>,
) -> Result<globset::GlobSet> {
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
    Ok(globs.build()?)
}

fn resolve(
    snapshot: &Snapshot,
    sources: Sources<'_>,
    sample_limit: usize,
    workers: usize,
    request: &parallel_check::RequestCheck<'_>,
) -> Result<ReferenceBase> {
    let check = &mut request.worker();
    ensure!(
        (1..=MAX_SAMPLE_LIMIT).contains(&sample_limit),
        "invalid_request: reference sample limit"
    );
    let globs = path_globs(sources.paths, check)?;
    let mut order: Vec<usize> = (0..snapshot.documents.len()).collect();
    order.sort_by(|&a, &b| snapshot.documents[a].key.cmp(&snapshot.documents[b].key));
    let docs: Vec<_> = order.iter().map(|&i| &snapshot.documents[i]).collect();
    // The sorted position of each snapshot document: semantic targets name the latter.
    let mut sorted = vec![0; order.len()];
    for (position, &i) in order.iter().enumerate() {
        sorted[i] = position;
    }
    let mut identities = Vec::new();
    let mut modules = Vec::new();
    let mut total_bytes = 0usize;
    let mut document_map = BTreeMap::new();
    for manifest in &snapshot.manifests {
        check()?;
        let bytes = manifest.bytes.as_ref().map_or(0, Vec::len);
        total_bytes = total_bytes
            .checked_add(bytes)
            .context("bounded_work: reference bytes")?;
        // Nested manifests (#784) count toward the total only: the cap guards the root inputs.
        let root = !manifest.path.contains('/');
        ensure!(
            (!root || bytes <= SOURCE_BYTES) && total_bytes <= TOTAL_BYTES,
            "bounded_work: reference source bytes"
        );
    }
    if let Some(config) = &snapshot.root_tsconfig {
        check()?;
        total_bytes = total_bytes
            .checked_add(config.bytes.len())
            .context("bounded_work: reference bytes")?;
        ensure!(
            config.bytes.len() <= SOURCE_BYTES && total_bytes <= TOTAL_BYTES,
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
    // Parsing is unbudgeted, so one pass over every document keeps all workers busy.
    let (trees, gaps): (Vec<_>, Vec<_>) = map_all(&docs, workers, request, |doc| {
        let check = &mut request.worker();
        let tree = parse(doc, check)?;
        let gap = syntax_bounds::scan(tree.root_node(), check)?;
        Ok((tree, gap))
    })?
    .into_iter()
    .unzip();
    // Every snapshot document feeds resolution, so path filters do not narrow this list.
    let partial_files: Vec<_> = docs
        .iter()
        .zip(&gaps)
        .filter_map(|(doc, gap)| {
            snapshot
                .evidence_gaps
                .get(&doc.key)
                .map(|&reason| syntax_bounds::SyntaxGap::module_only(reason, doc.file.text.len()))
                .as_ref()
                .or(gap.as_ref())
                .map(|gap| gap.file(&doc.key))
        })
        .collect();
    let selected: Vec<_> = docs
        .iter()
        .enumerate()
        .filter_map(|(i, doc)| {
            // A document with no evidence has no sites either: only `partialFiles` names it.
            (sources.selects(&globs, &doc.key.path)
                && !snapshot.evidence_gaps.contains_key(&doc.key))
            .then_some(i)
        })
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
                &snapshot.manifests,
                &selected,
                &mut work,
                &mut check,
            )?)),
            3 => {
                let catalog = if selected
                    .iter()
                    .any(|&i| script_family(&docs[i].key.language))
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
                        snapshot.root_tsconfig.as_ref(),
                        &mut work,
                        request,
                        workers,
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
    // Exact internal targets of semantic bindings: every declaration of the snapshot by
    // document, so a binding whose target is not in this snapshot is not used (#523).
    let mut declared: BTreeSet<(usize, &str)> = BTreeSet::new();
    let mut producers: BTreeMap<String, String> = BTreeMap::new();
    for doc in &docs {
        if let Some(producer) = &doc.semantic.producer_id {
            producers.insert(doc.key.language.clone(), producer.clone());
        }
    }
    if !producers.is_empty() {
        for (index, doc) in docs.iter().enumerate() {
            for declaration in &doc.declarations {
                owner_step(&mut rust.work, check)?;
                declared.insert((index, declaration.syntax_id.as_str()));
            }
        }
    }
    let locate = |document: usize, sid: &str| {
        sorted
            .get(document)
            .copied()
            .filter(|&index| declared.contains(&(index, sid)))
    };
    let mut sources: Vec<Option<SourceReferences>> = Vec::new();
    sources.resize_with(docs.len(), || None);
    let mut site_count = 0usize;
    let mut target_documents: BTreeMap<usize, TargetDocument<'_>> = BTreeMap::new();
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
            let mut found = SourceReferences {
                projection: doc.semantic.projection.clone(),
                edges: BTreeMap::new(),
                unresolved: Vec::new(),
                proof: BTreeSet::new(),
                packages: BTreeMap::new(),
                coverage: BTreeMap::new(),
                mapped: 0,
                semantic: 0,
            };
            for PreparedSite {
                raw,
                mut site,
                anchor,
            } in sites
            {
                check()?;
                let anchor = anchor.map(|index| &doc.semantic.anchors[index]);
                let historical = doc.semantic.projection.is_some();
                let structural = match &raw {
                    // Historical claims cannot be resolved against N by spelling. Any
                    // independent structural fallback must remain explicitly native-N.
                    Some(_) if historical && anchor.is_some() => None,
                    None => None,
                    Some(_) if site.owner_syntax_id.is_none() => {
                        Some(SiteResolution::unknown("reference_owner_ambiguous"))
                    }
                    Some(item) if doc.key.language == "rust" => Some(rust.resolve(
                        source,
                        item,
                        &packages,
                        &mut found.packages,
                        &docs,
                        check,
                    )?),
                    Some(item) if doc.key.language == "java" => Some(java.resolve(
                        source,
                        item,
                        &mut found.packages,
                        &mut rust.work,
                        check,
                    )?),
                    Some(item) if doc.key.language == "python" => Some(python.resolve(
                        source,
                        item,
                        &mut found.packages,
                        &mut rust.work,
                        check,
                    )?),
                    Some(item) if script_family(&doc.key.language) => Some(
                        javascript
                            .as_ref()
                            .expect("selected JavaScript catalog")
                            .resolve(source, item, &mut found.packages, &mut rust.work, check)?,
                    ),
                    Some(_) => Some(SiteResolution::unknown("module_mapping_unavailable")),
                };
                let semantic = anchor
                    .and_then(|anchor| semantic_resolution(anchor, structural.as_ref(), &locate));
                let resolution = match (semantic, structural) {
                    // A semantic-only site inside its own module adds no module edge.
                    (Some(resolution), None)
                        if resolution.targets.contains(&Target::Native(source)) =>
                    {
                        continue;
                    }
                    (Some(resolution), _) => {
                        let anchor = anchor.expect("semantic resolution has an anchor");
                        site.mapping = "semantic";
                        site.provenance_id = Some(anchor.provenance_id.clone());
                        site.semantic_metadata = anchor.metadata.clone();
                        if anchor.metadata.get("semanticBoundary") == Some(&json!(true)) {
                            site.caller = None;
                            site.caller_reason = Some("semantic_boundary");
                        }
                        if site.relation_kind == RelationKind::Reference {
                            if anchor.call_id.is_some() {
                                site.relation_kind = RelationKind::Call;
                            } else if anchor.type_role {
                                site.relation_kind = RelationKind::Type;
                            }
                        }
                        resolution
                    }
                    (None, Some(resolution)) => resolution,
                    // A semantic-only site whose binding is not usable here is not a site.
                    (None, None) => continue,
                };
                site_count += 1;
                ensure!(
                    site_count <= site_limit(),
                    "bounded_work: reference site count"
                );
                if let (true, Some(SemanticOutcome::Internal(document, sid))) =
                    (site.mapping == "semantic", anchor.map(|a| &a.outcome))
                    && let Some(target) = locate(*document, sid)
                    && resolution.targets.contains(&Target::Native(target))
                {
                    owner_step(&mut rust.work, check)?;
                    let entry = target_documents
                        .entry(target)
                        .or_insert_with(|| TargetDocument::new(docs[target]));
                    site.target = entry.by_sid.get(sid.as_str()).map(|d| ReferenceTarget {
                        syntax_id: sid.clone(),
                        display_key: display_key(d),
                        document: d.document.clone(),
                        lines: lines_of(&entry.newlines, d.range.start, d.range.end),
                    });
                }
                if site.target.is_none() {
                    site.target_reason = Some(target_reason(
                        site.mapping == "structural",
                        doc.semantic.producer_id.is_some(),
                        anchor.map(|a| &a.outcome),
                    ));
                }
                let role = site.role;
                let test = role == Some("test");
                let bound = site.mapping == "semantic";
                let semantic = bound && resolution.residual.is_none();
                let mapped = !resolution.targets.is_empty();
                let covered = found.coverage.entry(test).or_default();
                covered.sites += 1;
                covered.semantic += usize::from(semantic);
                covered.mapped += usize::from(mapped);
                covered.unresolved += usize::from(resolution.residual.is_some());
                found.semantic += usize::from(semantic);
                found.mapped += usize::from(mapped);
                found.proof.extend(resolution.proof);
                for target in resolution.targets {
                    owner_step(&mut rust.work, check)?;
                    let edge = found.edges.entry((target, test)).or_default();
                    edge.count += 1;
                    edge.semantic += usize::from(bound);
                    edge.kinds.add(site.relation_kind);
                    let sample = if site.relation_kind == RelationKind::Call {
                        &mut edge.calls
                    } else {
                        &mut edge.others
                    };
                    if sample.len() < sample_limit {
                        sample.push(site.clone());
                    }
                }
                if let Some(reason) = resolution.residual {
                    found.unresolved.push((site, reason));
                }
            }
            sources[source] = Some(found);
        }
    }
    // Freeing every syntax tree is serial native work; spread it across workers.
    let trees: Vec<_> = trees
        .into_iter()
        .map(|tree| std::sync::Mutex::new(Some(tree)))
        .collect();
    crate::parallel::map_ordered(
        &trees,
        workers,
        &std::sync::atomic::AtomicBool::new(false),
        |_, tree| drop(tree.lock().unwrap_or_else(|e| e.into_inner()).take()),
    );
    Ok(ReferenceBase {
        pin: snapshot.pin,
        partial_files,
        identities,
        modules,
        sources,
        producers,
        semantic_documents: docs
            .iter()
            .filter_map(|doc| {
                doc.semantic.projection.as_ref().map(|projection| {
                    json!({
                        "producerId":doc.semantic.producer_id,"language":doc.key.language,
                        "path":doc.key.path,"projection":projection,
                        "coverageState":doc.semantic.coverage_state,"boundary":doc.semantic.boundary
                    })
                })
            })
            .collect(),
        sample_limit,
        work_used: rust.work.used,
    })
}

impl ReferenceBase {
    /// The references of the sources `paths` selects, with at most `sample_limit` sites per
    /// edge: what `project` returns for the same arguments.
    pub fn select(
        &self,
        paths: &[String],
        sample_limit: usize,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<Projection> {
        self.select_under(paths, None, sample_limit, check)
    }

    /// [`Self::select`] of only the sources under the directory `prefix` (#837): what
    /// [`project_under`] returns for the same arguments.
    pub fn select_under(
        &self,
        paths: &[String],
        prefix: Option<&str>,
        sample_limit: usize,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<Projection> {
        self.select_inner(Sources { paths, prefix }, sample_limit, check)
    }

    fn select_inner(
        &self,
        sources: Sources<'_>,
        sample_limit: usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Projection> {
        ensure!(
            (1..=self.sample_limit).contains(&sample_limit),
            "invalid_request: reference sample limit"
        );
        let globs = path_globs(sources.paths, check)?;
        let identities = &self.identities;
        let mut emitted_modules = BTreeSet::new();
        let mut external_packages: BTreeMap<_, external::ExternalPackage> = BTreeMap::new();
        let mut coverage: BTreeMap<(String, bool), SiteCoverage> = BTreeMap::new();
        let mut production = Vec::new();
        let mut tests = Vec::new();
        let mut unresolved = Vec::new();
        let (mut scanned, mut mapped, mut semantic_sites) = (0usize, 0usize, 0usize);
        for (source, found) in self.sources.iter().enumerate() {
            check()?;
            let Some(found) = found else { continue };
            let path = &identities[source].path;
            if !sources.selects(&globs, path) {
                continue;
            }
            scanned += 1;
            mapped += found.mapped;
            semantic_sites += found.semantic;
            emitted_modules.insert(source);
            emitted_modules.extend(found.proof.iter().copied());
            for (test, row) in &found.coverage {
                let total = coverage.entry((path.clone(), *test)).or_default();
                total.sites += row.sites;
                total.mapped += row.mapped;
                total.unresolved += row.unresolved;
                total.semantic += row.semantic;
            }
            for (identity, package) in &found.packages {
                match external_packages.get_mut(identity) {
                    Some(existing) if existing.evidence != package.evidence => {
                        existing.evidence.extend(package.evidence.iter().cloned());
                        existing.evidence.sort();
                        existing.evidence.dedup();
                    }
                    Some(_) => {}
                    None => {
                        external_packages.insert(identity.clone(), package.clone());
                    }
                }
            }
            for ((target, test), edge) in &found.edges {
                let (count, semantic) = (&edge.count, &edge.semantic);
                check()?;
                let target = match target {
                    Target::Native(target) => {
                        emitted_modules.insert(*target);
                        json!(identities[*target])
                    }
                    Target::External(identity) => json!(identity),
                };
                let sampled: Vec<_> = edge
                    .calls
                    .iter()
                    .chain(&edge.others)
                    .take(sample_limit)
                    .collect();
                let omitted = count - sampled.len();
                let tier = match semantic {
                    0 => "syntax",
                    n if n == count => "semantic",
                    _ => "mixed",
                };
                let mut edge = json!({"source":identities[source],"target":target,"count":count,
                    "kindCounts":edge.kinds,"sites":sampled,"omittedSites":omitted,"evidenceTier":tier,"semanticSites":semantic});
                // The edge itself is detachable even when sampling omits other sites.
                if *semantic > 0
                    && let Some(projection) = &found.projection
                {
                    edge["projection"] = json!(projection);
                }
                if *test {
                    tests.push(edge);
                } else {
                    production.push(edge);
                }
            }
            for (site, reason) in &found.unresolved {
                check()?;
                unresolved.push(json!({"source":identities[source],"site":site,"reason":reason}));
            }
        }
        let emitted_modules: Vec<_> = emitted_modules
            .into_iter()
            .map(|i| &self.modules[i])
            .collect();
        let mut limitations = vec![
            "Structural file conventions are not compiler, package, type or call resolution; missing sites do not prove absence.",
            "Rust expands captured public names through proved lexical modules and bounded import/reexport chains; cfg, macros, unproved ownership and unknown in-repo names remain uncertain. Declared external roots identify packages only.",
            "JavaScript expands captured named ESM exports through bounded relative forwarding; stars exclude default. Ambiguous names, dynamic namespaces and unproved bindings remain uncertain. Static sources retain module evidence; declared npm roots and the partial pinned Node catalog identify packages only.",
            "Python expands proved checkout-root and declared pyproject source-root modules and package exports with bounded import forwarding and literal-list __all__; dynamic namespaces, ambiguous bindings and unproved package layouts remain uncertain. The pinned CPython 3.12 stdlib catalog labels roots only; third-party distribution mappings remain unknown.",
            "Java expands captured public top-level types from declared packages, with explicit and lexical shadowing; static/member types and unproved third-party prefixes remain unresolved. Java labels identify package families only; javax requires an exact package in the pinned OpenJDK 25 catalog, not proof of members or installed-JDK availability.",
        ];
        let bounds_limitation = syntax_bounds::limitation(self.partial_files.len());
        if !self.partial_files.is_empty() {
            limitations.push(&bounds_limitation);
        }
        if self.producers.is_empty() {
            limitations.push("No selected semantic producer has fresh bindings for these documents; every site is structural syntax evidence.");
        } else {
            limitations.push("Semantic sites use selected producer bindings. Historical basis-P sites retain their original claims and proof basis, never fresh or current absence. Pending endpoints stop semantic traversal; independent structural sites describe captured native-N syntax only.");
        }
        let semantic_producers: Vec<_> = self
            .producers
            .iter()
            .map(|(language, producer)| json!({"language":language,"producerId":producer}))
            .collect();
        check()?;
        Ok(Projection {
            data: json!({
                "referencesVersion":"references:v2","indexGeneration":self.pin.index_generation.to_string(),
                "indexRevision":self.pin.index_revision,"evidenceTier":if semantic_sites > 0 { "mixed" } else { "syntax" },"semanticComplete":false,
                "semanticProducers":semantic_producers,
                "semanticDocuments":self.semantic_documents,
                "partial":!self.semantic_documents.is_empty(),
                "modules":emitted_modules,"externalPackages":external_packages.into_values().collect::<Vec<_>>(),"production":production,"tests":tests,"unresolved":unresolved,
                "counts":{"scannedDocuments":scanned,"mappedSites":mapped,"unresolvedSites":unresolved.len(),"semanticSites":semantic_sites},
                "partialFiles":self.partial_files,
                "limitations":limitations
            }),
            modules: self.modules.clone(),
            coverage,
        })
    }
}

/// A semantic target's document: its line starts and declarations by syntax ID, filled on
/// first use.
struct TargetDocument<'a> {
    newlines: Vec<usize>,
    by_sid: BTreeMap<&'a str, &'a Declaration>,
}
impl<'a> TargetDocument<'a> {
    fn new(doc: &'a DocumentInput) -> Self {
        Self {
            newlines: newlines(&doc.file.text),
            by_sid: doc
                .declarations
                .iter()
                .map(|d| (d.syntax_id.as_str(), d))
                .collect(),
        }
    }
}

fn newlines(text: &str) -> Vec<usize> {
    text.bytes()
        .enumerate()
        .filter_map(|(i, b)| (b == b'\n').then_some(i))
        .collect()
}

/// The 1-based inclusive lines of a byte range, given the document's newline offsets.
fn lines_of(newlines: &[usize], start: usize, end: usize) -> Lines {
    Lines {
        start: newlines.partition_point(|&p| p < start) + 1,
        end: newlines.partition_point(|&p| p < end.saturating_sub(1)) + 1,
    }
}

/// Why a site has no `target`, by first match. `outcome` is the site's binding outcome.
fn target_reason(
    structural: bool,
    has_producer: bool,
    outcome: Option<&SemanticOutcome>,
) -> &'static str {
    match outcome {
        _ if structural && !has_producer => "structural_only",
        Some(SemanticOutcome::External) => "external",
        Some(SemanticOutcome::Ambiguous) => "ambiguous",
        _ => "unresolved",
    }
}

/// The written path of a declaration: its document and dotted ancestors.
pub(crate) fn display_key(d: &Declaration) -> String {
    let mut segments: Vec<String> = d
        .ancestors
        .iter()
        .map(|key| {
            key.name
                .clone()
                .unwrap_or_else(|| format!("{}@{}", key.kind, key.ordinal))
        })
        .collect();
    segments.push(
        d.name
            .clone()
            .unwrap_or_else(|| format!("{}@{}", d.kind, d.range.start)),
    );
    format!("{}#{}", d.document.path, segments.join("."))
}

/// How a fresh semantic binding resolves a site, or `None` to keep its structural result
/// (`structural` is `None` for a semantic-only site, which is then dropped).
fn semantic_resolution(
    anchor: &SemanticAnchor,
    structural: Option<&SiteResolution>,
    locate: &impl Fn(usize, &str) -> Option<usize>,
) -> Option<SiteResolution> {
    let historical = anchor.metadata.contains_key("projection");
    match &anchor.outcome {
        SemanticOutcome::Boundary => Some(SiteResolution::unknown("semantic_endpoint_catching_up")),
        SemanticOutcome::External if historical => {
            Some(SiteResolution::unknown("historical_external_claim"))
        }
        SemanticOutcome::Ambiguous | SemanticOutcome::Unresolved if historical => {
            Some(SiteResolution::unknown("historical_resolution_unproven"))
        }
        // A target outside this snapshot is stale: the structural result stands.
        SemanticOutcome::Internal(document, sid) => locate(*document, sid)
            .map(|target| SiteResolution {
                targets: BTreeSet::from([Target::Native(target)]),
                ..SiteResolution::default()
            })
            .or_else(|| {
                historical.then(|| SiteResolution::unknown("semantic_endpoint_catching_up"))
            }),
        // A proved package target says more than a binding outside the checkout.
        SemanticOutcome::External => structural
            .is_none_or(|resolution| resolution.targets.is_empty())
            .then(SiteResolution::default),
        // An unknown target keeps the structural result; it never makes a site of its own.
        SemanticOutcome::Ambiguous | SemanticOutcome::Unresolved => None,
    }
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
    sites: Vec<PreparedSite>,
}

/// One site of a document: a structural syntax site (`raw`), possibly bound by a
/// semantic anchor, or a semantic-only call or type site (#523).
struct PreparedSite {
    raw: Option<RawSite>,
    site: Site,
    anchor: Option<usize>,
}

/// The declaration, enclosing call and lines of one site range.
struct SiteContext<'a> {
    doc: &'a DocumentInput,
    declarations: RangeIndex<'a>,
    calls: RangeIndex<'a>,
    newlines: Vec<usize>,
    callers: callers::Callers<'a>,
    keys: BTreeMap<String, String>,
}
impl SiteContext<'_> {
    #[allow(clippy::too_many_arguments)]
    fn site(
        &self,
        start: usize,
        end: usize,
        kind: &'static str,
        spelling: String,
        call_id: Option<String>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Site> {
        let doc = self.doc;
        let owner = owner(doc, self.declarations.enclosing(start, end, work, check)?);
        let site_owner = owner.map(|d| {
            let file_module = d.kind == "module" && d.ancestors.is_empty();
            SiteOwner {
                syntax_id: d.syntax_id.clone(),
                name: if d.kind == "module" {
                    None
                } else {
                    d.name.clone()
                },
                stable_lookup_key: if file_module {
                    None
                } else {
                    self.keys.get(&d.syntax_id).cloned()
                },
            }
        });
        let role = owner.map(|d| match d.role {
            DeclarationRole::Test => "test",
            DeclarationRole::Production => "production",
        });
        let range = crate::native_evidence::Range { start, end };
        let call_id = match call_id {
            Some(id) => Some(id),
            None => {
                let candidates = self.calls.enclosing(start, end, work, check)?;
                let mut exact = None;
                for &index in &candidates {
                    owner_step(work, check)?;
                    if callers::matches_site(&doc.calls[index], &range)
                        && exact.is_none_or(|previous| index < previous)
                    {
                        exact = Some(index);
                    }
                }
                exact
                    .or_else(|| candidates.into_iter().min())
                    .map(|index| doc.calls[index].id.clone())
            }
        };
        let (mut caller, mut caller_reason) = self.callers.get(call_id.as_deref(), &range);
        // R1: an import stays an import and a structural callee path is a call.
        let relation_kind = match RelationKind::from_raw(kind) {
            RelationKind::Reference if self.callers.measured(call_id.as_deref(), &range) => {
                RelationKind::Call
            }
            RelationKind::Type => {
                // A caller belongs to a call or an import, never a type site.
                (caller, caller_reason) = (None, Some("syntax_only"));
                RelationKind::Type
            }
            kind => kind,
        };
        Ok(Site {
            owner: site_owner,
            target: None,
            target_reason: None,
            caller,
            caller_reason,
            owner_syntax_id: owner.map(|d| d.syntax_id.clone()),
            role,
            range: crate::native_evidence::Range { start, end },
            lines: (!doc.file.text.is_empty()).then(|| lines_of(&self.newlines, start, end)),
            relation_kind,
            spelling,
            call_id,
            mapping: "structural",
            provenance_id: None,
            semantic_metadata: BTreeMap::new(),
        })
    }
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
    let keys = if raw.is_empty() && doc.semantic.anchors.is_empty() {
        BTreeMap::new()
    } else {
        callers::lookup_keys(doc)
    };
    let context = SiteContext {
        doc,
        callers: callers::Callers::with_keys(doc, &keys),
        keys,
        declarations: RangeIndex::new(doc.declarations.iter().map(|d| &d.range), &mut work, check)?,
        calls: RangeIndex::new(doc.calls.iter().map(|c| &c.range), &mut work, check)?,
        newlines: newlines(&doc.file.text),
    };
    // Semantic anchors by start offset (#523). A structural site consumes every anchor
    // inside it, and a call joined on its whole invocation that starts with it; it is
    // bound by the anchor that ends where it ends (its final name or callee, a call
    // first), else by such an invocation.
    let anchors = &doc.semantic.anchors;
    let usable = |anchor: &SemanticAnchor| {
        anchor.range.start < anchor.range.end
            && doc
                .file
                .text
                .get(anchor.range.start..anchor.range.end)
                .is_some()
    };
    let mut anchors_by_start: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (index, anchor) in anchors.iter().enumerate() {
        owner_step(&mut work, check)?;
        if usable(anchor) {
            anchors_by_start
                .entry(anchor.range.start)
                .or_default()
                .push(index);
        }
    }
    let mut consumed = vec![false; anchors.len()];
    let mut seen = BTreeSet::new();
    let mut sites = Vec::new();
    for item in raw {
        check()?;
        if !seen.insert((item.start, item.end, item.kind, item.spelling.clone())) {
            continue;
        }
        let site = context.site(
            item.start,
            item.end,
            item.kind,
            item.spelling.clone(),
            None,
            &mut work,
            check,
        )?;
        let mut anchor: Option<usize> = None;
        let mut invocation: Option<usize> = None;
        let mut historical_anchor: Option<usize> = None;
        for (_, indexes) in anchors_by_start.range(item.start..item.end) {
            for &index in indexes {
                owner_step(&mut work, check)?;
                let candidate = &anchors[index];
                if candidate.range.end <= item.end {
                    consumed[index] = true;
                    if candidate.metadata.contains_key("projection") {
                        historical_anchor = historical_anchor.or(Some(index));
                    }
                } else if candidate.call_id.is_some() && candidate.range.start == item.start {
                    consumed[index] = true;
                    invocation = invocation.or(Some(index));
                    continue;
                } else {
                    continue;
                }
                if candidate.range.end == item.end
                    && anchor.is_none_or(|best| {
                        let best = &anchors[best];
                        (candidate.call_id.is_some(), candidate.range.start)
                            > (best.call_id.is_some(), best.range.start)
                    })
                {
                    anchor = Some(index);
                }
            }
        }
        sites.push(PreparedSite {
            raw: Some(item),
            site,
            anchor: anchor.or(invocation).or(historical_anchor),
        });
    }
    // Calls and type references with an internal target that no structural site covers
    // are sites of their own, one per range (a call first). Unbound, ambiguous,
    // unresolved and external bindings add no site.
    let mut covered = BTreeSet::new();
    for calls_first in [true, false] {
        for (index, anchor) in anchors.iter().enumerate() {
            check()?;
            if anchor.call_id.is_some() != calls_first
                || consumed[index]
                || !usable(anchor)
                || (!matches!(anchor.outcome, SemanticOutcome::Internal(..))
                    && !anchor.metadata.contains_key("projection"))
                || (anchor.call_id.is_none()
                    && !anchor.type_role
                    && !anchor.metadata.contains_key("projection"))
                || !covered.insert((anchor.range.start, anchor.range.end))
            {
                continue;
            }
            let site = context.site(
                anchor.range.start,
                anchor.range.end,
                if calls_first { "call" } else { "type" },
                doc.file.text[anchor.range.start..anchor.range.end].to_owned(),
                anchor.call_id.clone(),
                &mut work,
                check,
            )?;
            sites.push(PreparedSite {
                raw: None,
                site,
                anchor: Some(index),
            });
        }
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

/// Map every item in one parallel pass. After a failure no later item starts;
/// every earlier item still runs, so the error returned is the first in input
/// order whatever the schedule.
fn map_all<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    request: &parallel_check::RequestCheck<'_>,
    f: impl Fn(&T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    map_all_hooked(items, workers, request, &|_| {}, &|_| {}, f)
}

/// `map_all` with test hooks: `entered` runs after an index is claimed and
/// before it may be skipped; `failed` runs after a failure is recorded.
fn map_all_hooked<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    request: &parallel_check::RequestCheck<'_>,
    entered: &(dyn Fn(usize) + Sync),
    failed: &(dyn Fn(usize) + Sync),
    f: impl Fn(&T) -> Result<R> + Sync,
) -> Result<Vec<R>> {
    request.check()?;
    use std::sync::atomic::{AtomicUsize, Ordering};
    // Lowest failed index so far. Only items after it are skipped, so a skipped
    // slot always follows an actual error in input order.
    let first_failure = AtomicUsize::new(usize::MAX);
    let results = crate::parallel::map_ordered(items, workers, &request.stopped, |index, item| {
        entered(index);
        if index > first_failure.load(Ordering::Acquire) {
            return None;
        }
        let result = f(item);
        if result.is_err() {
            first_failure.fetch_min(index, Ordering::AcqRel);
            failed(index);
        }
        Some(result)
    });
    results
        .into_iter()
        .map(|slot| slot.flatten().context("reference request stopped")?)
        .collect()
}

/// JavaScript and TypeScript share one import-resolution family.
fn script_family(language: &str) -> bool {
    matches!(language, "javascript" | "typescript")
}
fn grammar(key: &DocumentKey) -> Result<tree_sitter::Language> {
    Ok(match key.language.as_str() {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => crate::indexer_ts::grammar(&key.path),
        _ => anyhow::bail!("store_unavailable: reference language"),
    })
}
fn parse(doc: &DocumentInput, check: &mut impl FnMut() -> Result<()>) -> Result<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&grammar(&doc.key)?)?;
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
    tree.context("bounded_work: reference parser stopped")
}
/// Depth-first over named syntax. What lies past the bounds and inside a parser error is
/// skipped: the projection's scan of this tree reported it.
pub(crate) fn walk<'a, C: FnMut() -> Result<()>>(
    root: Node<'a>,
    check: &mut C,
    visit: &mut impl FnMut(Node<'a>, &mut C) -> Result<bool>,
) -> Result<()> {
    let mut stack = vec![(root, 0)];
    let mut visits = 0;
    while let Some((node, depth)) = stack.pop() {
        check()?;
        visits += 1;
        if visits > VISITS {
            break;
        }
        if depth > DEPTH || node.is_error() {
            continue;
        }
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
    if script_family(&doc.key.language) {
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
mod historical_projection_tests {
    use super::*;

    fn anchor(outcome: SemanticOutcome) -> SemanticAnchor {
        SemanticAnchor {
            range: crate::native_evidence::Range { start: 0, end: 1 },
            call_id: None,
            type_role: true,
            direct: false,
            outcome,
            provenance_id: "proof".into(),
            metadata: BTreeMap::from([(
                "projection".into(),
                json!({"semanticStatus":"olderBasis"}),
            )]),
        }
    }

    #[test]
    fn historical_unknown_and_external_claims_never_use_current_structural_targets() {
        let structural = SiteResolution {
            targets: BTreeSet::from([Target::Native(4)]),
            ..SiteResolution::default()
        };
        for outcome in [
            SemanticOutcome::Unresolved,
            SemanticOutcome::Ambiguous,
            SemanticOutcome::External,
            SemanticOutcome::Boundary,
        ] {
            let result =
                semantic_resolution(&anchor(outcome), Some(&structural), &|_, _| Some(4)).unwrap();
            assert!(result.targets.is_empty());
            assert!(result.residual.is_some());
        }
    }

    #[test]
    fn historical_internal_target_missing_from_native_is_a_boundary() {
        let result = semantic_resolution(
            &anchor(SemanticOutcome::Internal(1, "target".into())),
            None,
            &|_, _| None,
        )
        .unwrap();
        assert!(result.targets.is_empty());
        assert_eq!(result.residual, Some("semantic_endpoint_catching_up"));
    }
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
        snapshot_of(&[
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
        ])
    }

    pub(super) fn snapshot_of(files: &[(&str, &str)]) -> Snapshot {
        let root = tempfile::tempdir().unwrap();
        for (path, text) in files.iter().copied() {
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
                    semantic: Default::default(),
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
            root_tsconfig: None,
            evidence_gaps: Default::default(),
        }
    }

    /// #556: a read selects from one whole-revision base instead of resolving its own paths.
    #[test]
    fn a_resolved_base_selects_what_a_scoped_projection_returns() {
        let snapshot = snapshot();
        let base = resolve_all(&snapshot, &mut || Ok(())).unwrap();
        for paths in [
            vec![],
            vec!["src/**".to_owned()],
            vec!["use.js".to_owned()],
            vec!["*.py".to_owned(), "Use.java".to_owned()],
            vec!["missing/**".to_owned()],
        ] {
            for limit in [1, 5, MAX_SAMPLE_LIMIT] {
                let scoped = project(&snapshot, &paths, limit, &mut || Ok(())).unwrap();
                let selected = base.select(&paths, limit, &mut || Ok(())).unwrap();
                assert_eq!(scoped.data, selected.data, "{paths:?} {limit}");
                assert_eq!(
                    format!("{:?}", scoped.coverage),
                    format!("{:?}", selected.coverage),
                    "{paths:?} {limit}"
                );
            }
        }
    }

    /// #837: a `prefix` selects the sources under a directory, with or without `paths`, and
    /// the scoped projection and the base's selection agree.
    #[test]
    fn a_prefix_selects_sources_under_a_directory() {
        let snapshot = snapshot();
        let base = resolve_all(&snapshot, &mut || Ok(())).unwrap();
        let sources = |data: &Value| -> BTreeSet<String> {
            ["production", "tests", "unresolved"]
                .iter()
                .flat_map(|field| data[*field].as_array().unwrap().clone())
                .map(|row| row["source"]["path"].as_str().unwrap().to_owned())
                .collect()
        };
        for (paths, prefix, scanned) in [
            (vec![], "src", 2),
            (vec!["**/lib.rs".to_owned()], "src", 1),
            (vec!["use.js".to_owned()], "src", 0),
            (vec![], "sr", 0),
            (vec![], "src/lib.rs", 0),
        ] {
            let scoped = project_under(&snapshot, &paths, Some(prefix), 5, &mut || Ok(())).unwrap();
            let selected = base
                .select_under(&paths, Some(prefix), 5, &mut || Ok(()))
                .unwrap();
            assert_eq!(scoped.data, selected.data, "{paths:?} {prefix}");
            assert_eq!(
                scoped.data["counts"]["scannedDocuments"], scanned,
                "{paths:?} {prefix}"
            );
            assert!(
                sources(&scoped.data)
                    .iter()
                    .all(|path| path.starts_with("src/")),
                "{paths:?} {prefix}"
            );
        }
        assert!(
            !sources(
                &project_under(&snapshot, &[], Some("src"), 5, &mut || Ok(()))
                    .unwrap()
                    .data
            )
            .is_empty()
        );
        for (prefix, normalized) in [
            ("src", Some("src")),
            ("src/", Some("src")),
            ("a/b c/", Some("a/b c")),
            ("src//", None),
            ("/", None),
            ("", None),
            ("..", None),
            ("a/./b", None),
            ("a\\b", None),
            ("a:b", None),
            ("a\0", None),
        ] {
            assert_eq!(directory_prefix(prefix), normalized, "{prefix:?}");
        }
    }

    #[test]
    fn serial_and_parallel_keep_payload_and_exact_work_counts() {
        let snapshot = snapshot();
        for paths in [vec![], vec!["src/lib.rs".into(), "use.*".into()]] {
            let (serial, used) = project_configured(
                &snapshot,
                Sources {
                    paths: &paths,
                    prefix: None,
                },
                1,
                1,
                &mut || Ok(()),
            )
            .unwrap();
            assert!(used > 0);
            for workers in [2, 8] {
                let (parallel, parallel_used) = project_configured(
                    &snapshot,
                    Sources {
                        paths: &paths,
                        prefix: None,
                    },
                    1,
                    workers,
                    &mut || Ok(()),
                )
                .unwrap();
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

    /// One pass over every item: after a failure no later item starts, and the
    /// reported error is the first in input order, not the first in time.
    #[test]
    fn single_pass_reports_first_failure_in_input_order_and_starts_nothing_later() {
        let mut check = || Ok(());
        let request = parallel_check::RequestCheck::new(&mut check);
        let (failed_tx, failed_rx) = std::sync::mpsc::channel();
        let failed_rx = Mutex::new(failed_rx);
        let started = Mutex::new(Vec::new());
        let result = map_all(&[0usize, 1, 2, 3], 2, &request, |&index| -> Result<usize> {
            started.lock().unwrap().push(index);
            match index {
                // Fails only after item 1 has failed, and after every later item
                // had the chance to start on the other worker.
                0 => {
                    failed_rx.lock().unwrap().recv().unwrap();
                    anyhow::bail!("first source failure")
                }
                1 => {
                    failed_tx.send(()).unwrap();
                    anyhow::bail!("later source failure")
                }
                _ => Ok(index),
            }
        });
        assert_eq!(result.unwrap_err().to_string(), "first source failure");
        let mut started = started.into_inner().unwrap();
        started.sort();
        assert_eq!(started, vec![0, 1]);
        let all = map_all(&[0usize, 1, 2, 3], 2, &request, |&index| Ok(index * 10));
        assert_eq!(all.unwrap(), vec![0, 10, 20, 30]);
    }

    /// An earlier index claimed before a later failure, but not yet past the
    /// skip check, still runs: the later typed failure is never masked.
    #[test]
    fn a_claimed_earlier_item_runs_after_a_later_typed_failure() {
        for message in [
            "bounded_work: reference site count",
            "bounded_work: reference owner lookup",
        ] {
            let mut check = || Ok(());
            let request = parallel_check::RequestCheck::new(&mut check);
            let (failed_tx, failed_rx) = std::sync::mpsc::channel();
            let failed_rx = Mutex::new(failed_rx);
            let entered = |index: usize| {
                if index == 0 {
                    failed_rx.lock().unwrap().recv().unwrap();
                }
            };
            let failed = |index: usize| {
                if index == 1 {
                    failed_tx.send(()).unwrap();
                }
            };
            let ran = Mutex::new(Vec::new());
            let result = map_all_hooked(
                &[0usize, 1, 2],
                2,
                &request,
                &entered,
                &failed,
                |&index| -> Result<usize> {
                    ran.lock().unwrap().push(index);
                    anyhow::ensure!(index != 1, "{message}");
                    Ok(index)
                },
            );
            assert_eq!(result.unwrap_err().to_string(), message);
            let mut ran = ran.into_inner().unwrap();
            ran.sort();
            assert_eq!(ran, vec![0, 1]);
            assert!(request.result(Ok(())).is_ok());
        }
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

#[cfg(test)]
mod classification_tests {
    use super::parallel_tests::snapshot_of;
    use super::*;

    fn edge<'a>(data: &'a Value, source: &str, target: &str) -> &'a Value {
        data["production"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["source"]["path"] == source && e["target"]["path"] == target)
            .unwrap()
    }

    /// R1 and R2: a structural callee path is a call, and a site names its owner.
    #[test]
    fn structural_callee_paths_are_calls() {
        let snapshot = snapshot_of(&[
            (
                "src/lib.rs",
                "mod high;\nuse crate::high::T;\nfn run(_: crate::high::T) { crate::high::f(); }\n",
            ),
            ("src/high.rs", "pub struct T;\npub fn f() -> u8 { 0 }\n"),
        ]);
        let data = project(&snapshot, &[], 20, &mut || Ok(())).unwrap().data;
        let pair = edge(&data, "src/lib.rs", "src/high.rs");
        let sites = pair["sites"].as_array().unwrap();
        let kind = |spelling: &str, line: u64| {
            sites
                .iter()
                .find(|s| s["spelling"] == spelling && s["lines"]["start"] == line)
                .unwrap_or_else(|| panic!("{spelling} on line {line}"))
        };
        assert_eq!(kind("crate::high::T", 2)["relationKind"], "import");
        assert_eq!(kind("crate::high::T", 3)["relationKind"], "reference");
        let call = kind("crate::high::f", 3);
        assert_eq!(call["relationKind"], "call");
        assert_eq!(call["caller"]["name"], "run");
        assert_eq!(call["owner"]["name"], "run");
        assert_eq!(call["owner"]["syntaxId"], call["ownerSyntaxId"]);
        assert_eq!(call["owner"]["stableLookupKey"], "src/lib.rs::run");
        assert_eq!(call["target"], Value::Null);
        assert_eq!(call["targetReason"], "structural_only");
        // Calls come first, and the counts add up to the edge.
        assert_eq!(sites[0]["relationKind"], "call");
        let counts = &pair["kindCounts"];
        assert_eq!(counts["call"], 1);
        assert_eq!(counts["import"], 1);
        assert_eq!(counts["reference"], 1);
        assert_eq!(counts["type"], 0);
        // A site outside any declaration is owned by the file module.
        let module = pair["source"]["sid"].clone();
        let import = kind("crate::high::T", 2);
        assert_eq!(import["owner"]["name"], Value::Null);
        assert_eq!(import["owner"]["stableLookupKey"], Value::Null);
        assert_eq!(import["owner"]["syntaxId"], module);
    }

    /// R3: the four reasons, and the order in which they apply.
    #[test]
    fn target_reason_vocabulary() {
        use SemanticOutcome::*;
        assert_eq!(target_reason(true, false, None), "structural_only");
        // No producer wins over any outcome.
        assert_eq!(
            target_reason(true, false, Some(&External)),
            "structural_only"
        );
        assert_eq!(target_reason(true, true, Some(&External)), "external");
        assert_eq!(target_reason(false, true, Some(&External)), "external");
        assert_eq!(target_reason(true, true, Some(&Ambiguous)), "ambiguous");
        assert_eq!(target_reason(true, true, None), "unresolved");
        assert_eq!(target_reason(true, true, Some(&Unresolved)), "unresolved");
        assert_eq!(target_reason(false, true, Some(&Boundary)), "unresolved");
        assert_eq!(
            target_reason(false, true, Some(&Internal(0, "sid".into()))),
            "unresolved"
        );
        assert_eq!(target_reason(false, true, None), "unresolved");
    }
}
