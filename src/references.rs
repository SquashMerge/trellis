//! View-time, source-witnessed module references. Structural mappings are not bindings.
use crate::model::IndexPin;
use crate::native_evidence::{Declaration, DeclarationRole, DocumentKey};
use crate::syntax_bounds::{self, DEPTH, VISITS};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tree_sitter::{Node, Tree};
use work::Work;

pub(crate) mod callers;
#[cfg(feature = "test-support")]
mod canonical;
mod cargo_workspace;
pub mod external;
mod java;
mod javascript;
mod js_workspace;
mod package_parts;
use package_parts::{ExternalPackageTable, SourcePackageDescriptors};
#[cfg(feature = "test-support")]
mod incremental;
#[cfg(all(test, feature = "test-support"))]
mod incremental_tests;
#[cfg(feature = "test-support")]
pub use incremental::ReferenceReplayTrace;
#[cfg(feature = "test-support")]
pub(crate) use incremental::{ReferencePreparation, finish_incremental, prepare_incremental};
#[cfg(feature = "test-support")]
pub(crate) use incremental::{SourceChargePlan, SourceFacts};
#[cfg(feature = "test-support")]
pub use work::{
    ReferenceWorkLimitForTests, ReferenceWorkLimitGuardForTests, set_reference_work_limit_for_tests,
};
pub(crate) mod parallel_check;
mod python;
mod rust;
pub(crate) use rust::feature_name;
mod shared;
pub mod tsconfig;
pub(crate) mod work;

const FOLLOW: usize = 4;

const SOURCE_BYTES: usize = 2 * 1024 * 1024;
const TOTAL_BYTES: usize = 256 * 1024 * 1024;
const SITES: usize = 250_000;

pub use crate::architecture_inputs::{
    DocumentInput, DocumentSemantic, SemanticAnchor, SemanticMetadata, SemanticOutcome,
    SemanticRelationship, Snapshot,
};
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
    /// Sites mapped or bound outside the checkout by a semantic binding (#523, #936).
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
    /// The sources in `identities` order; a source outside the scan stays `None`. Each is
    /// shared, so a later result can keep a source that did not change (#1301).
    sources: Vec<Option<Arc<SourceReferences>>>,
    external_packages: ExternalPackageTable,
    #[cfg(feature = "test-support")]
    rust_catalog_digest: [u8; 32],
    #[cfg(feature = "test-support")]
    source_facts: Vec<SourceFacts>,
    /// Language to selected producer.
    producers: BTreeMap<String, String>,
    /// How many projected documents (#620) are in each read status: older basis, catching
    /// up, needs re-import. Counts only (#932, follows owner ruling #930 2026-11-12), so an
    /// answer stays small however many documents are catching up.
    document_status: [usize; 3],
    /// The most sites each edge keeps; `select` samples at most this many.
    sample_limit: usize,
    work_used: usize,
}
/// One source document's resolved sites.
#[derive(Clone)]
struct SourceReferences {
    projection: Option<crate::model::SemanticProjection>,
    /// By target, stored role (test = true) and the sites' feature gates.
    edges: BTreeMap<(Target, bool, Vec<String>), EdgeSites>,
    unresolved: Vec<(Site, &'static str)>,
    /// Modules that prove this source's mapped sites.
    proof: BTreeSet<usize>,
    packages: BTreeSet<external::ExternalIdentity>,
    /// Coverage per stored role (test = true) and feature gates.
    coverage: BTreeMap<(bool, Vec<String>), SiteCoverage>,
    mapped: usize,
    semantic: usize,
}
/// The sites of one (target, role) edge: exact counts, and call-first samples (R5).
#[derive(Clone, Default)]
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
    fn merge(&mut self, other: &Self) {
        self.call += other.call;
        self.r#type += other.r#type;
        self.import += other.import;
        self.reference += other.reference;
    }
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
    /// Set only when a semantic binding resolves the site to an indexed declaration.
    target: Option<ReferenceTarget>,
    /// Why `target` is null: `structural_only`, `unresolved`, `external` or `ambiguous`.
    target_reason: Option<&'static str>,
    /// `structural` (captured syntax conventions) or `semantic` (a producer binding, labelled
    /// with its freshness).
    mapping: &'static str,
    /// The semantic binding's provenance; null for structural sites.
    provenance_id: Option<String>,
    #[serde(flatten)]
    semantic_metadata: SemanticMetadata,
    /// The features of the `cfg(feature = "..")` gates on a production Rust site, sorted and
    /// distinct (#988). A read that names one as a test feature reports the site as test.
    #[serde(skip)]
    gates: Vec<String>,
    /// The site's position among its source's sites: the order of merged samples.
    #[serde(skip)]
    ordinal: usize,
}
#[derive(Clone, Copy, Serialize)]
struct Lines {
    start: usize,
    end: usize,
}
#[derive(Clone, Serialize)]
pub(crate) struct RawSite {
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
    project_where(snapshot, paths, prefix, None, &[], sample_limit, check)
}

/// [`project_under`] of only the sources whose path is in `files`, when it is given: the
/// files of the apps an `app` argument names (#851). A production site under a `cfg` gate of
/// one of `test_features` is reported as test (#988).
pub fn project_where(
    snapshot: &Snapshot,
    paths: &[String],
    prefix: Option<&str>,
    files: Option<&BTreeSet<String>>,
    test_features: &[String],
    sample_limit: usize,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Projection> {
    project_configured(
        snapshot,
        Sources {
            paths,
            prefix,
            files,
            test_features,
        },
        sample_limit,
        crate::parallel::default_workers(),
        check,
    )
    .map(|(projection, _)| projection)
}

/// The sources a read selects: those any `paths` glob matches (every source without one),
/// and only under the directory `prefix` when it is given (#837), and only in `files` when it
/// is given (#851).
#[derive(Clone, Copy)]
struct Sources<'a> {
    paths: &'a [String],
    prefix: Option<&'a str>,
    files: Option<&'a BTreeSet<String>>,
    /// The Cargo features whose gates mark test code for this read (#988). Selection only:
    /// resolution records every gate.
    test_features: &'a [String],
}

impl Sources<'_> {
    fn all() -> Self {
        Sources {
            paths: &[],
            prefix: None,
            files: None,
            test_features: &[],
        }
    }

    fn selects(&self, globs: &globset::GlobSet, path: &str) -> bool {
        (self.paths.is_empty() || globs.is_match(path)) && self.under(path)
    }

    /// Whether `path` lies under the prefix and in the files (always, without them).
    fn under(&self, path: &str) -> bool {
        self.prefix.is_none_or(|prefix| {
            path.strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
        }) && self.files.is_none_or(|files| files.contains(path))
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
            files: None,
            test_features: &[],
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
    let Admitted {
        docs,
        sorted,
        identities,
        modules,
        document_map,
        total_bytes,
    } = admit_snapshot(snapshot, check)?;
    // Parsing is unbudgeted, so one pass over every document keeps all workers busy.
    let (trees, gaps): (Vec<_>, Vec<_>) = map_all(&docs, workers, request, |doc| {
        let check = &mut request.worker();
        let (tree, unread) = syntax(doc, snapshot.unparsed.contains(&doc.key), check)?;
        let gap = if unread {
            Some(syntax_bounds::SyntaxGap::past_parse_work(
                doc.file.text.len(),
            ))
        } else {
            syntax_bounds::scan(tree.root_node(), check)?
        };
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
    let read = ReadContext {
        snapshot,
        docs: &docs,
        trees: &trees,
        document_map: &document_map,
        selected: &selected,
        sample_limit,
        workers,
        request,
    };
    let mut catalogs = build_catalogs(read, limit, check)?;
    #[cfg(feature = "test-support")]
    let rust_catalog_digest = catalogs.rust.digest();
    let (declared, producers) = declared_targets(&docs, &mut catalogs.rust_execution.work, check)?;
    let locate = |document: usize, sid: &str| {
        sorted
            .get(document)
            .copied()
            .filter(|&index| declared.contains(&(index, sid)))
    };
    let SelectedReferences {
        sources,
        external_packages,
        #[cfg(feature = "test-support")]
            facts: source_facts,
    } = selected_references(read, &mut catalogs, &locate, check)?;
    external_packages.validate(&sources)?;
    drop_trees(trees, workers);
    Ok(ReferenceBase {
        pin: snapshot.pin,
        partial_files,
        identities,
        modules,
        sources,
        external_packages,
        #[cfg(feature = "test-support")]
        rust_catalog_digest,
        #[cfg(feature = "test-support")]
        source_facts,
        producers,
        document_status: docs
            .iter()
            .filter_map(|doc| doc.semantic.projection.as_ref())
            .fold([0; 3], |mut counts, projection| {
                use crate::model::SemanticStatus;
                counts[match projection.semantic_status {
                    SemanticStatus::OlderBasis => 0,
                    SemanticStatus::CatchingUp => 1,
                    SemanticStatus::NeedsReimport => 2,
                }] += 1;
                counts
            }),
        sample_limit,
        work_used: catalogs.rust_execution.work.used,
    })
}

/// What `admit_snapshot` hands the rest of a read.
struct Admitted<'a> {
    /// The snapshot's documents in key order: every later index is a position here.
    docs: Vec<&'a DocumentInput>,
    /// The sorted position of each snapshot document.
    sorted: Vec<usize>,
    identities: Vec<Identity>,
    modules: Vec<Value>,
    document_map: BTreeMap<DocumentKey, usize>,
    /// The bytes of every manifest, the root tsconfig and every document.
    total_bytes: usize,
}

/// What the stages of one read share: the snapshot, its admitted documents with their trees,
/// the selected sources and the request.
#[derive(Clone, Copy)]
struct ReadContext<'a, 'r> {
    snapshot: &'a Snapshot,
    docs: &'a [&'a DocumentInput],
    trees: &'a [Tree],
    document_map: &'a BTreeMap<DocumentKey, usize>,
    selected: &'a [usize],
    sample_limit: usize,
    workers: usize,
    request: &'a parallel_check::RequestCheck<'r>,
}

/// The language catalogs and transient Rust state of one read.
struct Catalogs {
    rust: rust::Catalog,
    rust_execution: rust::RustExecution,
    packages: external::RustPackages,
    java: Box<java::Catalog>,
    python: Box<python::Catalog>,
    javascript: Option<javascript::Catalog>,
}

/// Admission: the documents in key order, each with one file module and within the byte
/// budgets, after the manifests and the root tsconfig.
fn admit_snapshot<'a>(
    snapshot: &'a Snapshot,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Admitted<'a>> {
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
    Ok(Admitted {
        docs,
        sorted,
        identities,
        modules,
        document_map,
        total_bytes,
    })
}

/// The four catalogs, each built under its own count of the limit; the counts are then
/// admitted in catalog order into the one budget the Rust execution state keeps.
fn build_catalogs(
    read: ReadContext<'_, '_>,
    limit: usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Catalogs> {
    let ReadContext {
        snapshot,
        docs,
        trees,
        document_map,
        selected,
        workers,
        request,
        ..
    } = read;
    let catalogs = map_batch(&[0u8, 1, 2, 3], workers, request, |language| {
        let mut check = request.worker();
        let mut work = Work::new(limit);
        let catalog = match language {
            0 => {
                let packages =
                    external::RustPackages::new(&snapshot.manifests, &mut work, &mut check)?;
                let workspace =
                    cargo_workspace::Workspace::new(&snapshot.manifests, &mut work, &mut check)?;
                let catalog = rust::Catalog::new(
                    docs,
                    trees,
                    document_map,
                    &workspace,
                    &packages,
                    work,
                    &mut check,
                )?;
                let used = catalog.1.work.used;
                return Ok((CatalogJob::Rust(Box::new((catalog, packages))), used));
            }
            1 => CatalogJob::Java(Box::new(java::Catalog::new(
                docs, trees, selected, &mut work, &mut check,
            )?)),
            2 => CatalogJob::Python(Box::new(python::Catalog::new(
                docs,
                trees,
                &snapshot.manifests,
                selected,
                &mut work,
                &mut check,
            )?)),
            3 => {
                let catalog = if selected
                    .iter()
                    .any(|&i| script_family(&docs[i].key.language))
                {
                    let workspaces =
                        js_workspace::Workspaces::new(&snapshot.manifests, &mut work, &mut check)?;
                    let packages = external::JavascriptPackages::new(
                        &snapshot.manifests,
                        &workspaces,
                        &mut work,
                        &mut check,
                    )?;
                    Some(javascript::Catalog::new(
                        docs,
                        trees,
                        document_map,
                        selected,
                        &packages,
                        &workspaces,
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
    let ((rust, mut rust_execution), packages) = *rust_job;
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
    rust_execution.work = work;
    Ok(Catalogs {
        rust,
        rust_execution,
        packages,
        java,
        python,
        javascript,
    })
}

/// Every declaration of the snapshot, by sorted document and syntax ID.
type Declared<'a> = BTreeSet<(usize, &'a str)>;

/// The declarations a semantic binding may target, and the selected producer of each
/// language. Without a producer nothing is declared and nothing is charged.
fn declared_targets<'a>(
    docs: &[&'a DocumentInput],
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<(Declared<'a>, BTreeMap<String, String>)> {
    // Exact internal targets of semantic bindings: every declaration of the snapshot by
    // document, so a binding whose target is not in this snapshot is not used (#523).
    let mut declared: BTreeSet<(usize, &str)> = BTreeSet::new();
    let mut producers: BTreeMap<String, String> = BTreeMap::new();
    for doc in docs {
        if let Some(producer) = &doc.semantic.producer_id {
            producers.insert(doc.key.language.clone(), producer.clone());
        }
    }
    if !producers.is_empty() {
        for (index, doc) in docs.iter().enumerate() {
            for declaration in &doc.declarations {
                owner_step(work, check)?;
                declared.insert((index, declaration.syntax_id.as_str()));
            }
        }
    }
    Ok((declared, producers))
}

struct SelectedReferences {
    sources: Vec<Option<Arc<SourceReferences>>>,
    external_packages: ExternalPackageTable,
    #[cfg(feature = "test-support")]
    facts: Vec<SourceFacts>,
}

struct CompletedFile {
    source: usize,
    found: SourceReferences,
    packages: SourcePackageDescriptors,
    #[cfg(feature = "test-support")]
    facts: SourceFacts,
}

/// The references of every selected source, in batches of one prepared file per worker,
/// each admitted in source order.
fn selected_references(
    read: ReadContext<'_, '_>,
    catalogs: &mut Catalogs,
    locate: &impl Fn(usize, &str) -> Option<usize>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<SelectedReferences> {
    let ReadContext {
        docs,
        trees,
        selected,
        workers,
        request,
        ..
    } = read;
    let mut sources: Vec<Option<Arc<SourceReferences>>> = Vec::new();
    sources.resize_with(docs.len(), || None);
    let mut external_packages = ExternalPackageTable::new(docs.len());
    #[cfg(feature = "test-support")]
    let mut facts: Vec<SourceFacts> = (0..docs.len()).map(|_| SourceFacts::default()).collect();
    let mut site_count = 0usize;
    let mut target_documents: BTreeMap<usize, TargetDocument<'_>> = BTreeMap::new();
    for batch in selected.chunks(workers) {
        let remaining = catalogs.rust_execution.work.remaining();
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
            let CompletedFile {
                source,
                found,
                packages,
                #[cfg(feature = "test-support")]
                    facts: source_facts,
            } = file_references(
                read,
                catalogs,
                prepared,
                locate,
                &mut site_count,
                &mut target_documents,
                check,
            )?;
            ensure!(
                found.packages == external_packages.complete(source, packages),
                "store_unavailable: reference package completion"
            );
            sources[source] = Some(Arc::new(found));
            #[cfg(feature = "test-support")]
            {
                facts[source] = source_facts;
            }
        }
    }
    Ok(SelectedReferences {
        sources,
        external_packages,
        #[cfg(feature = "test-support")]
        facts,
    })
}

/// One prepared file's sites, in site order: each resolved structurally by its language's
/// catalog, then by its semantic binding, counted against the site limit and recorded.
fn file_references<'a>(
    read: ReadContext<'a, '_>,
    catalogs: &mut Catalogs,
    prepared: PreparedFile,
    locate: &impl Fn(usize, &str) -> Option<usize>,
    site_count: &mut usize,
    target_documents: &mut BTreeMap<usize, TargetDocument<'a>>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<CompletedFile> {
    let ReadContext {
        docs,
        sample_limit,
        request,
        ..
    } = read;
    let PreparedFile {
        source,
        work,
        sites,
    } = prepared;
    let doc = docs[source];
    request.check()?;
    #[cfg(feature = "test-support")]
    let (before_work, before_sites) = (catalogs.rust_execution.work.used, *site_count);
    #[cfg(feature = "test-support")]
    let mut plan = SourceChargePlan {
        prepared_work: work.used,
        sites: Vec::new(),
    };
    #[cfg(feature = "test-support")]
    let mut endpoints = incremental::semantic_endpoints(read, source)?;
    merge_work(&mut catalogs.rust_execution.work, work.used, check)?;
    let mut found = SourceReferences {
        projection: doc.semantic.projection.clone(),
        edges: BTreeMap::new(),
        unresolved: Vec::new(),
        proof: BTreeSet::new(),
        packages: BTreeSet::new(),
        coverage: BTreeMap::new(),
        mapped: 0,
        semantic: 0,
    };
    let mut source_packages = SourcePackageDescriptors::new();
    for (
        ordinal,
        PreparedSite {
            raw,
            mut site,
            anchor,
        },
    ) in sites.into_iter().enumerate()
    {
        check()?;
        site.ordinal = ordinal;
        let anchor = anchor.map(|index| &doc.semantic.anchors[index]);
        let historical = doc.semantic.projection.is_some();
        let charge = shared::ChargeSite {
            structural: shared::structural_charge(
                raw,
                historical && anchor.is_some(),
                site.owner_syntax_id.is_none(),
            ),
            semantic: anchor.map(shared::SemanticCharge::new).transpose()?,
        };
        let charged = shared::resolve_charge_site(
            read,
            catalogs,
            source,
            &charge,
            &mut source_packages,
            locate,
            check,
        )?;
        #[cfg(feature = "test-support")]
        {
            incremental::resolution_endpoints(&mut endpoints, &charged);
            plan.push(read, charge)?;
        }
        let Some((resolution, semantic)) = charged.effective(source) else {
            continue;
        };
        if semantic {
            apply_semantic(
                &mut site,
                anchor.expect("semantic resolution has an anchor"),
            );
        }
        *site_count += 1;
        ensure!(
            *site_count <= site_limit(),
            "bounded_work: reference site count"
        );
        if let Some((target, sid)) = internal_target_charge(
            semantic,
            anchor.map(|a| &a.outcome),
            &resolution,
            locate,
            &mut catalogs.rust_execution.work,
            check,
        )? {
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
        record_site(
            &mut found,
            site,
            resolution,
            sample_limit,
            &mut catalogs.rust_execution.work,
            check,
        )?;
    }
    found.packages = source_packages.keys().cloned().collect();
    Ok(CompletedFile {
        source,
        found,
        packages: source_packages,
        #[cfg(feature = "test-support")]
        facts: SourceFacts {
            plan: Some(Arc::new(plan)),
            work: catalogs.rust_execution.work.used - before_work,
            sites: *site_count - before_sites,
            endpoints: endpoints
                .into_iter()
                .map(u32::try_from)
                .collect::<std::result::Result<_, _>>()?,
        },
    })
}

/// The lookup admission shared by output construction and compact charge replay.
fn internal_target_charge<'a>(
    semantic: bool,
    outcome: Option<&'a SemanticOutcome>,
    resolution: &SiteResolution,
    locate: &impl Fn(usize, &str) -> Option<usize>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<(usize, &'a String)>> {
    if let (true, Some(SemanticOutcome::Internal(document, sid))) = (semantic, outcome)
        && let Some(target) = locate(*document, sid)
        && resolution.targets.contains(&Target::Native(target))
    {
        owner_step(work, check)?;
        return Ok(Some((target, sid)));
    }
    Ok(None)
}

/// What a semantic binding makes of the site it resolves: the mapping, the provenance and
/// metadata of its anchor, no caller across a semantic boundary, and a call or type kind
/// for a plain reference.
fn apply_semantic(site: &mut Site, anchor: &SemanticAnchor) {
    site.mapping = "semantic";
    site.provenance_id = Some(anchor.provenance_id.clone());
    site.semantic_metadata = anchor.metadata.clone();
    if anchor.metadata.get("semanticBoundary").as_deref() == Some(&json!(true)) {
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
}

/// One resolved site into its source's tallies: coverage by role and gates, the proof, and
/// per target the edge counts and a sample while the edge has room; an unresolved site is
/// kept with its reason.
fn record_site(
    found: &mut SourceReferences,
    site: Site,
    resolution: SiteResolution,
    sample_limit: usize,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let role = site.role;
    let test = role == Some("test");
    let bound = site.mapping == "semantic";
    let semantic = bound && resolution.residual.is_none();
    let mapped = !resolution.targets.is_empty();
    let covered = found
        .coverage
        .entry((test, site.gates.clone()))
        .or_default();
    covered.sites += 1;
    covered.semantic += usize::from(semantic);
    covered.mapped += usize::from(mapped);
    covered.unresolved += usize::from(resolution.residual.is_some());
    found.semantic += usize::from(semantic);
    found.mapped += usize::from(mapped);
    found.proof.extend(resolution.proof);
    for target in resolution.targets {
        owner_step(work, check)?;
        let edge = found
            .edges
            .entry((target, test, site.gates.clone()))
            .or_default();
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
    Ok(())
}

/// Frees the syntax trees of a read.
fn drop_trees(trees: Vec<Tree>, workers: usize) {
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
        self.select_where(paths, prefix, None, &[], sample_limit, check)
    }

    /// [`Self::select_under`] of only the sources whose path is in `files`, when it is given:
    /// what [`project_where`] returns for the same arguments (#851).
    pub fn select_where(
        &self,
        paths: &[String],
        prefix: Option<&str>,
        files: Option<&BTreeSet<String>>,
        test_features: &[String],
        sample_limit: usize,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<Projection> {
        self.select_inner(
            Sources {
                paths,
                prefix,
                files,
                test_features,
            },
            sample_limit,
            check,
        )
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
        // #988: a gate of a named test feature makes its production sites test for this read.
        let gated = |gates: &[String]| {
            gates
                .iter()
                .any(|gate| sources.test_features.contains(gate))
        };
        let site_json = |site: &Site| {
            let mut value = json!(site);
            if gated(&site.gates) {
                value["role"] = json!("test");
            }
            value
        };
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
            for ((test, gates), row) in &found.coverage {
                let total = coverage
                    .entry((path.clone(), *test || gated(gates)))
                    .or_default();
                total.sites += row.sites;
                total.mapped += row.mapped;
                total.unresolved += row.unresolved;
                total.semantic += row.semantic;
            }
            for (identity, package) in self.external_packages.selected(source, &found.packages) {
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
            // Edges that differ only in gates are one edge of the role this read gives them.
            let mut merged: BTreeMap<(&Target, bool), Vec<&EdgeSites>> = BTreeMap::new();
            for ((target, test, gates), edge) in &found.edges {
                check()?;
                merged
                    .entry((target, *test || gated(gates)))
                    .or_default()
                    .push(edge);
            }
            for ((target, test), parts) in &merged {
                let mut kinds = KindCounts::default();
                let (mut count, mut semantic) = (0usize, 0usize);
                for part in parts {
                    count += part.count;
                    semantic += part.semantic;
                    kinds.merge(&part.kinds);
                }
                let (count, semantic) = (&count, &semantic);
                let in_order = |pick: fn(&EdgeSites) -> &Vec<Site>| {
                    let mut sites: Vec<&Site> = parts.iter().flat_map(|part| pick(part)).collect();
                    sites.sort_by_key(|site| site.ordinal);
                    sites
                };
                check()?;
                let target = match target {
                    Target::Native(target) => {
                        emitted_modules.insert(*target);
                        json!(identities[*target])
                    }
                    Target::External(identity) => json!(identity),
                };
                let sampled: Vec<_> = in_order(|part| &part.calls)
                    .into_iter()
                    .chain(in_order(|part| &part.others))
                    .take(sample_limit)
                    .map(site_json)
                    .collect();
                let omitted = count - sampled.len();
                let tier = match semantic {
                    0 => "syntax",
                    n if n == count => "semantic",
                    _ => "mixed",
                };
                let mut edge = json!({"source":identities[source],"target":target,"count":count,
                    "kindCounts":kinds,"sites":sampled,"omittedSites":omitted,"evidenceTier":tier,"semanticSites":semantic});
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
                unresolved.push(
                    json!({"source":identities[source],"site":site_json(site),"reason":reason}),
                );
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
        // #837: a prefix scopes the partly read files too, as it does every source. Without
        // one they stay checkout-wide.
        let partial_files: Vec<_> = self
            .partial_files
            .iter()
            .filter(|file| sources.under(&file.path))
            .collect();
        let bounds_limitation = syntax_bounds::limitation(partial_files.len());
        if !partial_files.is_empty() {
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
                "semanticDocumentStatus":{"olderBasis":self.document_status[0],
                    "catchingUp":self.document_status[1],"needsReimport":self.document_status[2]},
                "partial":self.document_status.iter().any(|&count| count > 0),
                "modules":emitted_modules,"externalPackages":external_packages.into_values().collect::<Vec<_>>(),"production":production,"tests":tests,"unresolved":unresolved,
                "counts":{"scannedDocuments":scanned,"mappedSites":mapped,"unresolvedSites":unresolved.len(),"semanticSites":semantic_sites},
                "partialFiles":partial_files,
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

/// How a semantic binding resolves a site, or `None` to keep its structural result
/// (`structural` is `None` for a semantic-only site, which is then dropped).
#[cfg(test)]
fn semantic_resolution(
    anchor: &SemanticAnchor,
    structural: Option<&SiteResolution>,
    locate: &impl Fn(usize, &str) -> Option<usize>,
) -> Option<SiteResolution> {
    shared::semantic_charge_resolution(
        &shared::SemanticCharge::new(anchor).ok()?,
        structural,
        locate,
    )
}

enum CatalogJob {
    Rust(Box<((rust::Catalog, rust::RustExecution), external::RustPackages)>),
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
    tree: &'a Tree,
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
        let mut role = owner.map(|d| match d.role {
            DeclarationRole::Test => "test",
            DeclarationRole::Production => "production",
        });
        // A `#[cfg(test)]` statement, block, arm or `use` item is no declaration, so no
        // stored role covers it (#988). A feature gate is kept for the read to judge.
        let mut gates = Vec::new();
        if role == Some("production") && doc.key.language == "rust" {
            let marks = rust::site_marks(doc, self.tree, start, end, work, check)?;
            if marks.test {
                role = Some("test");
            } else {
                gates = marks.features;
            }
        }
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
            semantic_metadata: SemanticMetadata::default(),
            gates,
            ordinal: 0,
        })
    }
}

fn merge_work(work: &mut Work, used: usize, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
    check()?;
    // The request total, not the phase whose count tipped it over (#894).
    ensure!(work.charge(used), "bounded_work: reference work total");
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
        callers::lookup_keys(doc, check)?
    };
    let context = SiteContext {
        doc,
        callers: callers::Callers::with_keys(doc, &keys),
        keys,
        declarations: RangeIndex::new(doc.declarations.iter().map(|d| &d.range), &mut work, check)?,
        calls: RangeIndex::new(doc.calls.iter().map(|c| &c.range), &mut work, check)?,
        newlines: newlines(&doc.file.text),
        tree,
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
    // unresolved and external bindings add no site. An older-basis projection (#620)
    // follows the same rule, so it answers P's sites with labels and never a superset
    // of them (#934): a binding that was such a site at P, and whose endpoint is now
    // catching up, is a boundary site; any other boundary is never a site of its own.
    let mut covered = BTreeSet::new();
    for calls_first in [true, false] {
        for (index, anchor) in anchors.iter().enumerate() {
            check()?;
            if anchor.call_id.is_some() != calls_first
                || consumed[index]
                || !usable(anchor)
                || !match anchor.outcome {
                    SemanticOutcome::Internal(..) => true,
                    SemanticOutcome::Boundary => anchor.basis_site,
                    _ => false,
                }
                || (anchor.call_id.is_none() && !anchor.type_role)
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
/// A document's syntax, and whether it is [`syntax_bounds::unread_tree`]: the tree of a Rust
/// source past measurement's parse-work budget, whether indexing found it so (`unparsed`, never
/// parsed again) or this bounded parse does (#1300).
fn syntax(
    doc: &DocumentInput,
    unparsed: bool,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<(Tree, bool)> {
    let grammar = grammar(&doc.key)?;
    let tree = if unparsed {
        None
    } else {
        syntax_bounds::parse_stored(
            &doc.key.language,
            &grammar,
            &doc.file.text,
            "bounded_work: reference parser stopped",
            check,
        )?
    };
    Ok(match tree {
        Some(tree) => (tree, false),
        None => (syntax_bounds::unread_tree(&doc.file.text), true),
    })
}
/// A document's [`syntax`], parsed.
#[cfg(test)]
fn parse(doc: &DocumentInput, check: &mut impl FnMut() -> Result<()>) -> Result<Tree> {
    Ok(syntax(doc, false, check)?.0)
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
mod historical_projection_tests;

#[cfg(test)]
mod range_index_tests;

#[cfg(test)]
mod parallel_tests;

#[cfg(test)]
mod classification_tests;

#[cfg(test)]
mod read_pins_tests;
