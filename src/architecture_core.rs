//! What Architecture needs from one revision, computed once (#556).
//!
//! References, rules and metrics all start from the same native rows. A core reads them once
//! and keeps only what later reads select from: no source text, syntax trees or semantic rows.
//! `crate::http::architecture` owns building one per head; reads project from it. Only the
//! read itself holds a SQLite transaction, so the longer computing never delays a publication.
use crate::metrics::DeclarationMetrics;
use crate::metrics_read::{
    METRICS_WORKER_CHECK_EVERY, MetricsComputedDocument, MetricsDocumentFailure, MetricsReadCheck,
};
use crate::model::IndexPin;
use crate::native_records::{DeclarationRole, DocumentKey};
use crate::references::{Projection, ReferenceBase};
use crate::rules::{Config, RuleError, RulesBase};
use crate::store::{EvidenceResponse, SelectedIntegrity, Store};
use anyhow::Result;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

pub(crate) mod dependencies;

#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArchitectureExecutionBranch {
    Candidate,
    Retry,
    Oracle,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchitectureExecutionEvent {
    ReferenceInvocation,
    MetricWorker,
    BeforeMetricCommit,
    MetricCommit,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Debug, Default)]
pub struct ArchitectureExecutionTrace {
    pub file_references: Vec<DocumentKey>,
    pub measured_sources: Vec<DocumentKey>,
    /// Present once reference computation completed, independently of later metrics.
    pub references: Option<crate::references::ReferenceReplayTrace>,
}

#[cfg(feature = "test-support")]
pub type ArchitectureExecutionHook = Arc<
    dyn Fn(ArchitectureExecutionBranch, ArchitectureExecutionEvent, &DocumentKey) -> Result<()>
        + Send
        + Sync,
>;

#[cfg(feature = "test-support")]
pub(crate) struct ArchitectureExecutionObserver {
    branch: ArchitectureExecutionBranch,
    hook: Option<ArchitectureExecutionHook>,
    trace: Mutex<ArchitectureExecutionTrace>,
}

#[cfg(feature = "test-support")]
thread_local! {
    // Reference admission and metric commits run on the computing thread. Full metric
    // workers receive an explicit observer clone only for the worker admission seam.
    static EXECUTION_OBSERVER: std::cell::RefCell<Option<Arc<ArchitectureExecutionObserver>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(feature = "test-support")]
pub(crate) struct ArchitectureExecutionScope {
    previous: Option<Arc<ArchitectureExecutionObserver>>,
    thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(feature = "test-support")]
impl Drop for ArchitectureExecutionScope {
    fn drop(&mut self) {
        EXECUTION_OBSERVER.with(|observer| {
            *observer.borrow_mut() = self.previous.take();
        });
    }
}

#[cfg(feature = "test-support")]
impl ArchitectureExecutionObserver {
    pub(crate) fn new(
        branch: ArchitectureExecutionBranch,
        hook: Option<ArchitectureExecutionHook>,
    ) -> Arc<Self> {
        Arc::new(Self {
            branch,
            hook,
            trace: Mutex::default(),
        })
    }

    pub(crate) fn enter(self: &Arc<Self>) -> ArchitectureExecutionScope {
        ArchitectureExecutionScope {
            previous: EXECUTION_OBSERVER.with(|observer| observer.replace(Some(self.clone()))),
            thread: std::marker::PhantomData,
        }
    }

    fn current() -> Option<Arc<Self>> {
        EXECUTION_OBSERVER.with(|observer| observer.borrow().clone())
    }

    pub(crate) fn snapshot(&self) -> ArchitectureExecutionTrace {
        self.trace.lock().unwrap().clone()
    }

    fn event(&self, event: ArchitectureExecutionEvent, key: &DocumentKey) -> Result<()> {
        {
            let mut trace = self.trace.lock().unwrap();
            match event {
                ArchitectureExecutionEvent::ReferenceInvocation => {
                    trace.file_references.push(key.clone())
                }
                ArchitectureExecutionEvent::MetricCommit => {
                    trace.measured_sources.push(key.clone())
                }
                ArchitectureExecutionEvent::MetricWorker
                | ArchitectureExecutionEvent::BeforeMetricCommit => {}
            }
        }
        // Never run a barrier or test callback while holding the observation lock.
        self.hook
            .as_ref()
            .map_or(Ok(()), |hook| hook(self.branch, event, key))
    }
}

#[cfg(feature = "test-support")]
pub(crate) fn architecture_execution_event(
    event: ArchitectureExecutionEvent,
    key: &DocumentKey,
) -> Result<()> {
    ArchitectureExecutionObserver::current().map_or(Ok(()), |observer| observer.event(event, key))
}

#[cfg(feature = "test-support")]
pub(crate) fn architecture_references_completed(trace: crate::references::ReferenceReplayTrace) {
    if let Some(observer) = ArchitectureExecutionObserver::current() {
        observer.trace.lock().unwrap().references = Some(trace);
    }
}

/// The checked arguments of one architecture operation.
pub(crate) struct ArchitectureArguments {
    pub(crate) pin: Option<IndexPin>,
    pub(crate) paths: Vec<String>,
    /// References only (#837): the normalized directory every source lies under.
    pub(crate) prefix: Option<String>,
    /// References only (#837): `groupBy:"package"`, which needs each file's app.
    pub(crate) apps: bool,
    /// #851: the `app` filter of references, check and endpoints, an app name or manifest path.
    pub(crate) app: Option<String>,
    /// Endpoints only (#851): the method and exact path template a read keeps.
    pub(crate) method: Option<String>,
    pub(crate) path_template: Option<String>,
    pub(crate) sample_limit: usize,
}

impl ArchitectureArguments {
    pub(crate) fn pin(&self) -> Option<IndexPin> {
        self.pin
    }
    /// App groups (#837), an `app` filter, or endpoints (#851): each file's app, which
    /// evaluates the rules when there are any.
    pub(crate) fn apps(&self) -> bool {
        self.apps || self.app.is_some()
    }
    pub(crate) fn scoped(&self) -> bool {
        !self.paths.is_empty() || self.prefix.is_some()
    }
}

/// How far one core build has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ArchitectureProgress {
    pub phase: &'static str,
    pub completed: usize,
    pub total: usize,
}

/// A summary with nothing to compute, or the head of one that needs a core.
pub(crate) enum ArchitectureView {
    Done(serde_json::Value),
    Ready {
        pin: IndexPin,
        payload: serde_json::Value,
    },
}

/// One document's metrics rows for every role.
pub(crate) struct MetricsDocument {
    pub(crate) rows: Vec<DeclarationMetrics>,
    pub(crate) gap: Option<crate::syntax_bounds::SyntaxGap>,
    pub(crate) physical_lines: usize,
    pub(crate) physical_bytes: usize,
    pub(crate) file_role: DeclarationRole,
}
/// Each document's rows are shared, so a later result can keep a document that did not
/// change (#1301).
pub(crate) type ArchitectureMetrics = BTreeMap<DocumentKey, Arc<MetricsDocument>>;

/// A failure this revision and rules file will repeat: kept and answered like a result, so
/// that a read fails as a computed read did. Contention and cancellation are never kept.
#[derive(Clone, Debug)]
pub(crate) struct CoreFailure {
    rule: Option<RuleError>,
    integrity: bool,
    message: String,
}
impl CoreFailure {
    pub(crate) fn of(error: &anyhow::Error) -> Self {
        Self {
            rule: error.downcast_ref::<RuleError>().cloned(),
            integrity: error
                .chain()
                .any(|cause| cause.downcast_ref::<SelectedIntegrity>().is_some()),
            message: format!("{error:#}"),
        }
    }
    pub(crate) fn error(&self) -> anyhow::Error {
        match &self.rule {
            Some(rule) => rule.clone().into(),
            None if self.integrity => SelectedIntegrity(self.message.clone()).into(),
            None => anyhow::anyhow!("{}", self.message),
        }
    }
    /// A build that panicked has no error to keep.
    pub(crate) fn panicked() -> Self {
        Self {
            rule: None,
            integrity: false,
            message: "store_unavailable: architecture build failed".into(),
        }
    }
    /// Whether computing this revision and rules file again would fail the same way.
    pub(crate) fn repeats(error: &anyhow::Error) -> bool {
        let message = error.to_string();
        error.downcast_ref::<RuleError>().is_some()
            || message.starts_with("bounded_work")
            || message.starts_with("invalid_source_syntax")
            || error
                .chain()
                .any(|cause| cause.downcast_ref::<SelectedIntegrity>().is_some())
    }
}

struct RulesPart {
    content_hash: String,
    base: Result<RulesBase, CoreFailure>,
}

type SummaryKey = (String, String, bool);

/// A shared finished summary. Edges carry no per-file pairs: the edge scope of
/// `architecture/details` pages them (#917).
pub(crate) struct ArchitectureSummary(pub(crate) Arc<serde_json::Value>);
impl serde::Serialize for ArchitectureSummary {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

/// One revision's references, its rules at one file hash, and its metrics.
pub(crate) struct ArchitectureCore {
    pub(crate) pin: IndexPin,
    references: ReferenceBase,
    rules: Option<RulesPart>,
    /// `None` when this core was computed for one read that has no use for metrics.
    metrics: Option<Result<ArchitectureMetrics, CoreFailure>>,
    /// Every build manifest's app-detection outcome, before any rules file hides some.
    app_candidates: Result<crate::apps::Candidates, CoreFailure>,
    /// Server routes and client calls, computed only when detected plus authored apps number
    /// two or more ([`interfaces_extracted`]), with or without a rules file (#851).
    interfaces: Option<Result<crate::interfaces::Extraction, CoreFailure>>,
    /// Finished summaries; one per valid role, aggregate and workbench choice.
    summaries: Mutex<BTreeMap<SummaryKey, Arc<serde_json::Value>>>,
    dependencies: dependencies::ArchitectureDependencies,
}

impl ArchitectureCore {
    pub(crate) fn incremental_base_eligible(&self) -> bool {
        self.metrics_if_computed().is_some()
            && self.dependencies.order.len() == self.dependencies.documents.len()
    }

    pub(crate) fn document_keys(&self) -> Vec<DocumentKey> {
        self.dependencies.order.to_vec()
    }

    pub(crate) fn full_reference_trace(
        &self,
        limit: usize,
    ) -> crate::references::ReferenceReplayTrace {
        self.references
            .full_trace(limit, crate::parallel::default_workers())
    }

    pub(crate) fn references(
        &self,
        paths: &[String],
        test_features: &[String],
        sample_limit: usize,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<Projection> {
        self.references
            .select_where(paths, None, None, test_features, sample_limit, check)
    }
    /// The references of one read's sources: its `paths` under its `prefix` (#837), and in
    /// `files` when given (#851).
    pub(crate) fn references_in(
        &self,
        parsed: &ArchitectureArguments,
        files: Option<&std::collections::BTreeSet<String>>,
        test_features: &[String],
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<Projection> {
        self.references.select_where(
            &parsed.paths,
            parsed.prefix.as_deref(),
            files,
            test_features,
            parsed.sample_limit,
            check,
        )
    }
    /// The rules part for exactly this file.
    pub(crate) fn rules(&self, config: &Config) -> Result<&RulesBase> {
        match &self.rules {
            Some(part) if part.content_hash == config.content_hash => {
                part.base.as_ref().map_err(CoreFailure::error)
            }
            _ => anyhow::bail!("store_unavailable: architecture rules part"),
        }
    }
    pub(crate) fn metrics(&self) -> Result<&ArchitectureMetrics> {
        match &self.metrics {
            Some(metrics) => metrics.as_ref().map_err(CoreFailure::error),
            None => anyhow::bail!("store_unavailable: architecture metrics part"),
        }
    }
    pub(crate) fn app_candidates(&self) -> Result<&crate::apps::Candidates> {
        self.app_candidates.as_ref().map_err(CoreFailure::error)
    }
    pub(crate) fn interfaces(&self) -> Result<&crate::interfaces::Extraction> {
        match &self.interfaces {
            Some(interfaces) => interfaces.as_ref().map_err(CoreFailure::error),
            None => anyhow::bail!("store_unavailable: architecture interfaces part"),
        }
    }
    /// Metrics rows for a read that can compute its own if this core failed them.
    pub(crate) fn metrics_if_computed(&self) -> Option<&ArchitectureMetrics> {
        self.metrics.as_ref()?.as_ref().ok()
    }
    /// The rules file hash this core evaluated.
    pub(crate) fn rules_hash(&self) -> Option<&str> {
        self.rules.as_ref().map(|part| part.content_hash.as_str())
    }
    pub(crate) fn summary(&self, key: &SummaryKey) -> Option<Arc<serde_json::Value>> {
        self.summaries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }
    pub(crate) fn remember_summary(
        &self,
        key: SummaryKey,
        summary: serde_json::Value,
    ) -> Arc<serde_json::Value> {
        let summary = Arc::new(summary);
        self.summaries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, summary.clone());
        summary
    }
}

/// One revision's documents, read once in one admitted transaction.
pub(crate) struct ArchitectureInput {
    snapshot: crate::references::Snapshot,
    regions: Vec<Vec<crate::native_records::ControlRegion>>,
    fingerprints: crate::architecture_inputs::ArchitectureFingerprints,
    semantic_counts: crate::store::semantic_display::ArchitectureSemanticCounts,
}

impl ArchitectureInput {
    pub(crate) fn document_keys(&self) -> Vec<DocumentKey> {
        self.fingerprints.documents.keys().cloned().collect()
    }

    pub(crate) fn reference_limit(&self, check: &mut impl FnMut() -> Result<()>) -> Result<usize> {
        Ok(crate::references::work::reference_limit(
            self.snapshot.documents.len(),
            crate::references::work::snapshot_bytes(&self.snapshot, check)?,
        ))
    }
}

struct CoreCompletion<'a> {
    store: &'a Store,
    config: Option<&'a Config>,
    measure: bool,
    progress: &'a (dyn Fn(ArchitectureProgress) + Sync),
}

impl EvidenceResponse {
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn architecture_snapshot_for_tests(
        &self,
        pin: IndexPin,
    ) -> Result<crate::references::Snapshot> {
        Ok(self
            .architecture_input(pin, &mut || Ok(()), &|_| {})?
            .snapshot)
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn architecture_full_for_tests(&self, pin: IndexPin) -> Result<serde_json::Value> {
        let config = crate::rules::load(std::path::Path::new(self.store.workspace_root()))?;
        self.architecture_core_at(pin, config.as_ref(), true, &mut || Ok(()), &|_| {})?
            .canonical()
    }

    /// Expected structural provenance from this same-pin admitted native Read A.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn architecture_manifests_for_tests(
        &self,
        pin: IndexPin,
    ) -> Result<Vec<crate::architecture_inputs::ManifestInput>> {
        let (snapshot, _, _) =
            self.architecture_read_a_fingerprinted(Some(pin), &mut || Ok(()), &|_, _| {})?;
        Ok(snapshot.manifests)
    }

    /// The documents a core computes from: read A, then read B, in this one transaction.
    pub(crate) fn architecture_input(
        &self,
        pin: IndexPin,
        check: &mut (impl FnMut() -> Result<()> + Send),
        progress: &(dyn Fn(ArchitectureProgress) + Sync),
    ) -> Result<ArchitectureInput> {
        let mut input = self.architecture_native_input(pin, check, progress)?;
        self.architecture_semantic_input(&mut input, check)?;
        Ok(input)
    }

    /// Read A (#1301): every document's native rows, the retained manifests and the root
    /// TypeScript config, reported as `reading`. The documents have no semantic rows yet.
    pub(crate) fn architecture_native_input(
        &self,
        pin: IndexPin,
        check: &mut (impl FnMut() -> Result<()> + Send),
        progress: &(dyn Fn(ArchitectureProgress) + Sync),
    ) -> Result<ArchitectureInput> {
        let (snapshot, regions, fingerprints) = self
            .architecture_read_a_fingerprinted(Some(pin), check, &|completed, total| {
                progress(ArchitectureProgress {
                    phase: "reading",
                    completed,
                    total,
                });
            })
            .map_err(|error| self.store.report_index_failure(error))?;
        Ok(ArchitectureInput {
            snapshot,
            regions,
            fingerprints,
            semantic_counts: Default::default(),
        })
    }

    /// Read B (#1301): the semantic rows of every document of `input`, at its pin. This
    /// response may be a later admitted transaction than read A's; it reads the same rows.
    pub(crate) fn architecture_semantic_input(
        &self,
        input: &mut ArchitectureInput,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<()> {
        input.semantic_counts = self
            .architecture_semantic_for_sources(
                &mut input.snapshot,
                crate::store::semantic_display::ArchitectureSemanticSources::All,
                check,
            )
            .map_err(|error| self.store.report_index_failure(error))?;
        Ok(())
    }

    /// Read and compute one revision's core for this one read.
    pub(crate) fn architecture_core_at(
        &self,
        pin: IndexPin,
        config: Option<&Config>,
        measure: bool,
        check: &mut (impl FnMut() -> Result<()> + Send),
        progress: &(dyn Fn(ArchitectureProgress) + Sync),
    ) -> Result<ArchitectureCore> {
        let input = self.architecture_input(pin, check, progress)?;
        ArchitectureCore::compute(&self.store, input, config, measure, check, progress)
    }

    /// The revision this read answers.
    pub(crate) fn selected_pin(&self, pin: Option<IndexPin>) -> Result<IndexPin> {
        Ok(self.selected(pin)?.pin)
    }
}

impl ArchitectureCore {
    /// Compute a core from documents already read; this touches no SQLite. A rules or metrics
    /// failure of the revision stays in its part, so references remain readable. `check`
    /// failures propagate.
    pub(crate) fn compute(
        store: &Store,
        input: ArchitectureInput,
        config: Option<&Config>,
        measure: bool,
        check: &mut (impl FnMut() -> Result<()> + Send),
        progress: &(dyn Fn(ArchitectureProgress) + Sync),
    ) -> Result<Self> {
        progress(ArchitectureProgress {
            phase: "resolving",
            completed: 0,
            total: input.snapshot.documents.len(),
        });
        let references = crate::references::resolve_all(&input.snapshot, check)?;
        #[cfg(feature = "test-support")]
        if ArchitectureExecutionObserver::current().is_some() {
            let limit = input.reference_limit(&mut || Ok(()))?;
            architecture_references_completed(
                references.full_trace(limit, crate::parallel::default_workers()),
            );
        }
        Self::complete(
            input,
            references,
            CoreCompletion {
                store,
                config,
                measure,
                progress,
            },
            None,
            check,
        )
    }

    /// Rules, apps and interfaces always see the complete current snapshot. Only the
    /// reference engine and metric selection differ between the two compute paths.
    fn complete(
        input: ArchitectureInput,
        references: ReferenceBase,
        context: CoreCompletion<'_>,
        reuse: Option<(&ArchitectureCore, &std::collections::BTreeSet<DocumentKey>)>,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<Self> {
        let CoreCompletion {
            store,
            config,
            measure,
            progress,
        } = context;
        let ArchitectureInput {
            snapshot,
            regions,
            fingerprints,
            semantic_counts,
        } = input;
        let total = snapshot.documents.len();
        let report = |phase| {
            progress(ArchitectureProgress {
                phase,
                completed: 0,
                total,
            });
        };
        let mut dependencies = dependencies::ArchitectureDependencies::full(
            &snapshot,
            fingerprints,
            &semantic_counts.sources,
            semantic_counts.global,
            &references,
        )?;
        if let Some((base, _)) = reuse {
            dependencies.share_unchanged(&base.dependencies);
        }
        report("evaluating");
        let rules = config
            .map(|config| {
                let base = match crate::rules::prepare(config, &snapshot, check) {
                    Ok(base) => Ok(base),
                    Err(error) if CoreFailure::repeats(&error) => Err(CoreFailure::of(&error)),
                    Err(error) => return Err(error),
                };
                Ok(RulesPart {
                    content_hash: config.content_hash.clone(),
                    base,
                })
            })
            .transpose()?;
        let metrics = if measure {
            report("measuring");
            let measured = match reuse {
                Some((base, changed)) => engine::measure_changed_documents(
                    store,
                    &snapshot,
                    &regions,
                    base.metrics()?,
                    changed,
                    check,
                    progress,
                ),
                None => measure_documents(store, &snapshot, &regions, check, progress),
            };
            Some(match measured {
                Ok(metrics) => Ok(metrics),
                Err(error) if CoreFailure::repeats(&error) => Err(CoreFailure::of(&error)),
                Err(error) => return Err(error),
            })
        } else {
            None
        };
        let app_candidates = kept(crate::rules::app_candidates(&snapshot, check))?;
        let interfaces = match (config, &app_candidates) {
            // Authored apps can split one manifest's checkout (#794), so they count too: an
            // upper bound of the effective inventory. Without a rules file every detected
            // manifest is an app (#851: `trellis_endpoints` reads them too).
            (config, Ok(candidates)) if interfaces_extracted(config, candidates) => {
                let mut work =
                    crate::references::work::Work::new(crate::references::work::rules_limit(
                        snapshot.documents.len(),
                        crate::references::work::snapshot_bytes(&snapshot, check)?,
                    ));
                // App-scoped wrappers (#855) need each file's app, as rule evaluation assigns
                // it; a failed rules part is this part's failure too. Without a rules file
                // there are no configured wrappers.
                let wrappers = config.map_or(&[][..], |config| config.interface_wrappers());
                let apps = match config {
                    Some(config) if wrappers.iter().any(|wrapper| wrapper.app.is_some()) => {
                        match rules.as_ref().map(|part| &part.base) {
                            Some(Ok(base)) => kept(crate::rules::file_apps(
                                config,
                                base,
                                snapshot
                                    .documents
                                    .iter()
                                    .filter(|doc| {
                                        matches!(
                                            doc.key.language.as_str(),
                                            "javascript" | "typescript"
                                        )
                                    })
                                    .map(|doc| doc.key.path.as_str()),
                                &mut work,
                                check,
                            ))?,
                            Some(Err(failure)) => Err(failure.clone()),
                            None => Ok(Default::default()),
                        }
                    }
                    _ => Ok(Default::default()),
                };
                Some(match apps {
                    Ok(apps) => kept(crate::interfaces::extract(
                        &snapshot, wrappers, &apps, &mut work, check,
                    ))?,
                    Err(failure) => Err(failure),
                })
            }
            (_, Ok(_)) => Some(Ok(crate::interfaces::Extraction::default())),
            // Failed app detection is the part's failure too, never an empty extraction.
            (_, Err(failure)) => Some(Err(failure.clone())),
        };
        check()?;
        Ok(Self {
            pin: snapshot.pin,
            references,
            rules,
            metrics,
            app_candidates,
            interfaces,
            summaries: Mutex::default(),
            dependencies,
        })
    }
}

/// The path a supplied-base plan takes. The runtime owner selects bases separately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArchitectureBuildPath {
    Full(FullReason),
    Incremental {
        dirty: Vec<DocumentKey>,
        patched: Vec<DocumentKey>,
        reused: Vec<DocumentKey>,
    },
}

/// Why a build read and computed every document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullReason {
    /// The build had no earlier result to start from.
    NoBase,
    DocumentSetChanged,
    ManifestsChanged,
    RootTsconfigChanged,
    ProducerSetChanged,
    RawSelectionsChanged,
    /// A changed native document's language has no catalog digest: today, Java.
    NonRustChanged,
    DirtyMajority,
    RustCatalogChanged,
    PythonCatalogChanged,
    JavascriptCatalogChanged,
    ReferenceWorkMargin,
    /// An ordinary incremental failure was retried through the independent full path.
    IncrementalFailure,
}

#[cfg(feature = "test-support")]
pub use engine::{
    ArchitectureBuildForTests, ArchitectureCoreHandle, ArchitectureCoreWeak, ArchitectureSharing,
};
pub(crate) use engine::{
    ArchitectureBuildPlan, ArchitectureReadA, ArchitectureReadB, ReferenceWorkMargin,
    finish_architecture, prepare_architecture,
};

#[cfg(feature = "test-support")]
impl ArchitectureCore {
    pub(crate) fn test_handle(core: Arc<Self>) -> ArchitectureCoreHandle {
        engine::ArchitectureCoreHandle(core)
    }
}

mod engine {
    use super::*;
    use crate::store::semantic_display::{ArchitectureSemanticCounts, ArchitectureSemanticSources};
    use anyhow::{Context, ensure};
    #[cfg(feature = "test-support")]
    use serde::Serialize;
    use std::collections::BTreeSet;

    /// The caller supplies context and a base, not an eligibility policy. References to the
    /// store/config/progress are not references into the owned native snapshot.
    pub(crate) struct ArchitectureReadA<'a> {
        pub(crate) input: ArchitectureInput,
        pub(crate) store: &'a Store,
        pub(crate) config: Option<&'a Config>,
        pub(crate) measure: bool,
        pub(crate) progress: &'a (dyn Fn(ArchitectureProgress) + Sync),
        pub(crate) trace: &'a Mutex<crate::references::ReferenceReplayTrace>,
        /// Captured by the requester and explicitly applied on each compute worker.
        #[cfg(feature = "test-support")]
        pub(crate) work_limit: crate::references::ReferenceWorkLimitForTests,
    }

    pub(crate) struct ArchitectureBuildPlan<'a> {
        pub(super) read_a: ArchitectureReadA<'a>,
        base: Option<Arc<ArchitectureCore>>,
        pub(super) changed: dependencies::ChangedDocuments,
        references: Option<crate::references::ReferencePreparation>,
        pub(crate) path: ArchitectureBuildPath,
        all_semantic: bool,
    }

    /// Read B owns its admitted rows, not a lease. The pin prevents mixing two admissions.
    pub(crate) struct ArchitectureReadB {
        pub(super) pin: IndexPin,
        pub(super) documents: Vec<crate::references::DocumentSemantic>,
        pub(super) counts: ArchitectureSemanticCounts,
    }

    #[derive(Debug)]
    pub(crate) struct ReferenceWorkMargin;
    impl std::fmt::Display for ReferenceWorkMargin {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("incremental reference work margin")
        }
    }
    impl std::error::Error for ReferenceWorkMargin {}

    pub(crate) fn prepare_architecture<'a>(
        mut read_a: ArchitectureReadA<'a>,
        base: Option<Arc<ArchitectureCore>>,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<super::ArchitectureBuildPlan<'a>> {
        #[cfg(feature = "test-support")]
        let _work_limit = read_a.work_limit.apply();
        check()?;
        (read_a.progress)(ArchitectureProgress {
            phase: "resolving",
            completed: 0,
            total: read_a.input.snapshot.documents.len(),
        });
        let decision = dependencies::changed_documents(
            &read_a.input.snapshot,
            &read_a.input.fingerprints,
            base.as_ref().map(|base| &base.dependencies),
        )?;
        let mut plan = ArchitectureBuildPlan {
            path: ArchitectureBuildPath::Full(FullReason::NoBase),
            changed: dependencies::ChangedDocuments::default(),
            references: None,
            all_semantic: true,
            base,
            read_a: {
                // Clean payloads will not be read, but producer selection, projection
                // counts and the complete declaration eligibility index must be current.
                for doc in &mut read_a.input.snapshot.documents {
                    let state = read_a
                        .input
                        .fingerprints
                        .semantic_states
                        .get(&doc.key)
                        .context("missing architecture semantic state")?;
                    doc.semantic.producer_id = state.default_producer.clone();
                    doc.semantic.projection = state.projection.clone();
                    doc.semantic.boundary = state
                        .projection
                        .as_ref()
                        .is_some_and(|projection| projection.semantic_status.is_pending());
                }
                read_a
            },
        };
        let changed = match decision {
            Ok(changed) => changed,
            Err(reason) => {
                plan.path = ArchitectureBuildPath::Full(reason);
                return Ok(plan);
            }
        };
        let references =
            crate::references::prepare_incremental(&plan.read_a.input.snapshot, check)?;
        let base = plan.base.as_ref().expect("incremental decision has a base");
        if !changed.native.is_empty() {
            for (current, old, reason) in [
                (
                    references.rust_digest,
                    base.dependencies.rust_catalog,
                    FullReason::RustCatalogChanged,
                ),
                (
                    references.python_digest,
                    base.dependencies.python_catalog,
                    FullReason::PythonCatalogChanged,
                ),
                (
                    references.javascript_digest,
                    base.dependencies.javascript_catalog,
                    FullReason::JavascriptCatalogChanged,
                ),
            ] {
                if current != old {
                    plan.path = ArchitectureBuildPath::Full(reason);
                    return Ok(plan);
                }
            }
        }
        plan.all_semantic = plan.read_a.config.is_some_and(Config::has_rules)
            && plan
                .read_a
                .input
                .fingerprints
                .semantic_states
                .values()
                .any(|state| {
                    !state
                        .projection
                        .as_ref()
                        .is_some_and(|projection| projection.semantic_status.is_pending())
                        && state.tuple.as_ref().is_some_and(|tuple| {
                            tuple.selected
                                && matches!(tuple.state.as_str(), "complete" | "partial")
                                && tuple.payload_id.is_some()
                        })
                });
        plan.path = ArchitectureBuildPath::Incremental {
            dirty: changed.dirty.iter().cloned().collect(),
            patched: changed.patched.iter().cloned().collect(),
            reused: changed.reused.iter().cloned().collect(),
        };
        plan.changed = changed;
        plan.references = Some(references);
        Ok(plan)
    }

    impl ArchitectureBuildPlan<'_> {
        pub(crate) fn semantic_keys(&self) -> Vec<DocumentKey> {
            if self.all_semantic {
                self.read_a
                    .input
                    .fingerprints
                    .documents
                    .keys()
                    .cloned()
                    .collect()
            } else {
                self.changed.dirty.iter().cloned().collect()
            }
        }
    }

    impl EvidenceResponse {
        /// This admitted read ends before finish executes any resolver or measurement.
        pub(crate) fn architecture_plan_read_b(
            &self,
            plan: &mut ArchitectureBuildPlan<'_>,
            check: &mut (impl FnMut() -> Result<()> + Send),
        ) -> Result<super::ArchitectureReadB> {
            let total = plan.read_a.input.snapshot.documents.len();
            (plan.read_a.progress)(ArchitectureProgress {
                phase: "resolving",
                completed: 0,
                total,
            });
            let selection = if plan.all_semantic {
                ArchitectureSemanticSources::All
            } else {
                let base = plan.base.as_ref().expect("selective read has a base");
                ArchitectureSemanticSources::Sources {
                    keys: plan.changed.dirty.clone(),
                    retained: base
                        .dependencies
                        .order
                        .iter()
                        .zip(&base.dependencies.documents)
                        .filter(|(key, _)| !plan.changed.dirty.contains(*key))
                        .map(|(key, document)| (key.clone(), document.semantic))
                        .collect(),
                }
            };
            let counts = self
                .architecture_semantic_for_sources(
                    &mut plan.read_a.input.snapshot,
                    selection,
                    check,
                )
                .map_err(|error| self.store.report_index_failure(error))?;
            Ok(ArchitectureReadB {
                pin: plan.read_a.input.snapshot.pin,
                documents: plan
                    .read_a
                    .input
                    .snapshot
                    .documents
                    .iter_mut()
                    .map(|doc| std::mem::take(&mut doc.semantic))
                    .collect(),
                counts,
            })
        }
    }

    pub(crate) fn finish_architecture(
        mut plan: ArchitectureBuildPlan<'_>,
        read_b: ArchitectureReadB,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<ArchitectureCore> {
        #[cfg(feature = "test-support")]
        let _work_limit = plan.read_a.work_limit.apply();
        check()?;
        ensure!(
            read_b.pin == plan.read_a.input.snapshot.pin,
            "architecture read B pin"
        );
        ensure!(
            read_b.documents.len() == plan.read_a.input.snapshot.documents.len(),
            "architecture read B inventory"
        );
        for (doc, semantic) in plan
            .read_a
            .input
            .snapshot
            .documents
            .iter_mut()
            .zip(read_b.documents)
        {
            doc.semantic = semantic;
        }
        plan.read_a.input.semantic_counts = read_b.counts;
        let ArchitectureReadA {
            input,
            store,
            config,
            measure,
            progress,
            trace,
            #[cfg(feature = "test-support")]
                work_limit: _,
        } = plan.read_a;
        let Some(prepared) = plan.references else {
            let limit = crate::references::work::reference_limit(
                input.snapshot.documents.len(),
                crate::references::work::snapshot_bytes(&input.snapshot, check)?,
            );
            let core = ArchitectureCore::compute(store, input, config, measure, check, progress)?;
            *trace.lock().unwrap_or_else(|error| error.into_inner()) = core
                .references
                .full_trace(limit, crate::parallel::default_workers());
            return Ok(core);
        };
        let limit = prepared.limit;
        let base = plan.base.as_ref().context("incremental base")?;
        let completed = crate::references::finish_incremental(
            &input.snapshot,
            prepared,
            &base.references,
            &plan.changed.dirty,
            &plan.changed.patched,
            check,
        )?;
        #[cfg(feature = "test-support")]
        architecture_references_completed(completed.trace.clone());
        let margin = base
            .dependencies
            .order
            .iter()
            .zip(&base.dependencies.documents)
            .filter(|(key, _)| plan.changed.dirty.contains(*key))
            .try_fold(completed.trace.executed_work, |work, (_, doc)| {
                work.checked_add(doc.references.work)
            });
        *trace.lock().unwrap_or_else(|error| error.into_inner()) = completed.trace;
        let margin = margin.context("incremental reference work margin overflow")?;
        if margin > limit {
            return Err(super::ReferenceWorkMargin.into());
        }
        ArchitectureCore::complete(
            input,
            completed.references,
            CoreCompletion {
                store,
                config,
                measure,
                progress,
            },
            Some((base, &plan.changed.native)),
            check,
        )
    }

    #[cfg(feature = "test-support")]
    #[derive(Clone)]
    pub struct ArchitectureCoreHandle(pub(super) Arc<ArchitectureCore>);

    #[cfg(feature = "test-support")]
    pub struct ArchitectureCoreWeak(std::sync::Weak<ArchitectureCore>);
    #[cfg(feature = "test-support")]
    impl ArchitectureCoreWeak {
        pub fn is_alive(&self) -> bool {
            self.0.strong_count() != 0
        }
    }

    #[cfg(feature = "test-support")]
    #[derive(Debug, Serialize)]
    pub struct ArchitectureSharing {
        pub source: bool,
        pub metric: bool,
        pub dependency: bool,
        pub fingerprint: bool,
        pub references: bool,
        pub endpoints: bool,
        pub plan: bool,
    }

    #[cfg(feature = "test-support")]
    impl ArchitectureCoreHandle {
        pub fn canonical(&self) -> Result<serde_json::Value> {
            self.0.canonical()
        }
        pub fn downgrade(&self) -> super::ArchitectureCoreWeak {
            ArchitectureCoreWeak(Arc::downgrade(&self.0))
        }
        pub fn pin(&self) -> IndexPin {
            self.0.pin
        }
        pub fn sharing(&self, other: &Self) -> BTreeMap<DocumentKey, super::ArchitectureSharing> {
            let old = &other.0;
            self.0
                .dependencies
                .order
                .iter()
                .zip(&self.0.dependencies.documents)
                .enumerate()
                .filter_map(|(i, (key, document))| {
                    let j = old.dependencies.order.binary_search(key).ok()?;
                    let before = &old.dependencies.documents[j];
                    Some((
                        key.clone(),
                        ArchitectureSharing {
                            source: self.0.references.source_shared(i, &old.references, j),
                            metric: self
                                .0
                                .metrics_if_computed()
                                .and_then(|metrics| metrics.get(key))
                                .zip(
                                    old.metrics_if_computed()
                                        .and_then(|metrics| metrics.get(key)),
                                )
                                .is_some_and(|(a, b)| Arc::ptr_eq(a, b)),
                            dependency: Arc::ptr_eq(document, before),
                            fingerprint: Arc::ptr_eq(&document.fingerprint, &before.fingerprint),
                            references: Arc::ptr_eq(&document.references, &before.references),
                            endpoints: Arc::ptr_eq(
                                &document.references.endpoints,
                                &before.references.endpoints,
                            ),
                            plan: document
                                .references
                                .plan
                                .as_ref()
                                .zip(before.references.plan.as_ref())
                                .is_some_and(|(a, b)| Arc::ptr_eq(a, b)),
                        },
                    ))
                })
                .collect()
        }
    }

    #[cfg(feature = "test-support")]
    pub struct ArchitectureBuildForTests {
        pub core: ArchitectureCoreHandle,
        pub path: ArchitectureBuildPath,
        pub semantic_sources: Vec<DocumentKey>,
        pub measured_sources: Vec<DocumentKey>,
        pub progress: Vec<ArchitectureProgress>,
        pub references: crate::references::ReferenceReplayTrace,
    }

    #[cfg(feature = "test-support")]
    pub(super) fn retryable_incremental_error(error: &anyhow::Error) -> bool {
        !error.to_string().starts_with("cancelled")
            && !crate::store::transient_storage_contention(error)
    }

    #[cfg(feature = "test-support")]
    impl Store {
        /// Direct supplied-base engine adapter. It owns no cache and retains no native input.
        #[doc(hidden)]
        pub fn architecture_build_for_tests(
            &self,
            base: Option<&super::ArchitectureCoreHandle>,
            config: Option<&Config>,
        ) -> Result<super::ArchitectureBuildForTests> {
            let work_limit = crate::references::ReferenceWorkLimitForTests::capture();
            let _work_limit = work_limit.apply();
            let mut observer =
                ArchitectureExecutionObserver::new(ArchitectureExecutionBranch::Candidate, None);
            let mut scope = Some(observer.enter());
            let progress = Mutex::new(Vec::new());
            let report = |value| {
                progress
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(value)
            };
            let trace = Mutex::new(Default::default());
            let response = self.evidence_response()?;
            let pin = response.selected_pin(None)?;
            let input = response.architecture_native_input(pin, &mut || Ok(()), &report)?;
            let input = response.finish(input)?;
            drop(response);
            let mut path = None;
            let mut semantic_sources = Vec::new();
            let computed = (|| {
                let mut plan = super::prepare_architecture(
                    super::ArchitectureReadA {
                        input,
                        store: self,
                        config,
                        measure: true,
                        progress: &report,
                        trace: &trace,
                        work_limit,
                    },
                    base.map(|handle| handle.0.clone()),
                    &mut || Ok(()),
                )?;
                path = Some(plan.path.clone());
                semantic_sources = plan.semantic_keys();
                let response = self.evidence_response()?;
                let read_b = response
                    .architecture_plan_read_b(&mut plan, &mut || Ok(()))
                    .and_then(|read_b| response.finish(read_b));
                drop(response);
                super::finish_architecture(plan, read_b?, &mut || Ok(()))
            })();
            let core = match computed {
                Ok(core) => core,
                Err(error)
                    if base.is_some()
                        && path.as_ref().is_none_or(|path| {
                            matches!(path, ArchitectureBuildPath::Incremental { .. })
                        })
                        && retryable_incremental_error(&error) =>
                {
                    drop(scope.take());
                    observer = ArchitectureExecutionObserver::new(
                        ArchitectureExecutionBranch::Retry,
                        None,
                    );
                    scope = Some(observer.enter());
                    path = Some(ArchitectureBuildPath::Full(
                        if error.downcast_ref::<ReferenceWorkMargin>().is_some() {
                            FullReason::ReferenceWorkMargin
                        } else {
                            FullReason::IncrementalFailure
                        },
                    ));
                    let response = self.evidence_response()?;
                    let input = response.architecture_input(pin, &mut || Ok(()), &report)?;
                    semantic_sources = input.fingerprints.documents.keys().cloned().collect();
                    let input = response.finish(input)?;
                    drop(response);
                    let limit = crate::references::work::reference_limit(
                        input.snapshot.documents.len(),
                        crate::references::work::snapshot_bytes(&input.snapshot, &mut || Ok(()))?,
                    );
                    let core = ArchitectureCore::compute(
                        self,
                        input,
                        config,
                        true,
                        &mut || Ok(()),
                        &report,
                    )?;
                    *trace.lock().unwrap_or_else(|error| error.into_inner()) = core
                        .references
                        .full_trace(limit, crate::parallel::default_workers());
                    core
                }
                Err(error) => return Err(error),
            };
            let path = path.context("completed architecture build path")?;
            let execution = observer.snapshot();
            drop(scope);
            let mut references = execution.references.unwrap_or_default();
            references.file_references = execution.file_references;
            Ok(ArchitectureBuildForTests {
                core: ArchitectureCore::test_handle(Arc::new(core)),
                path,
                semantic_sources,
                measured_sources: execution.measured_sources,
                progress: progress.into_inner().unwrap_or_else(|e| e.into_inner()),
                references,
            })
        }
    }

    pub(super) fn measure_changed_documents(
        store: &Store,
        snapshot: &crate::references::Snapshot,
        regions: &[Vec<crate::native_records::ControlRegion>],
        base: &ArchitectureMetrics,
        changed: &BTreeSet<DocumentKey>,
        check: &mut (impl FnMut() -> Result<()> + Send),
        report: &(dyn Fn(ArchitectureProgress) + Sync),
    ) -> Result<ArchitectureMetrics> {
        let mut metrics: BTreeMap<_, _> = base
            .iter()
            .filter(|(key, _)| !changed.contains(*key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let total = snapshot.documents.len();
        report(ArchitectureProgress {
            phase: "measuring",
            completed: metrics.len(),
            total,
        });
        let mut order: Vec<_> = snapshot
            .documents
            .iter()
            .enumerate()
            .filter(|(_, doc)| changed.contains(&doc.key))
            .collect();
        order.sort_by_key(|(_, doc)| &doc.key);
        let cancelled = MetricsReadCheck {
            check: Mutex::new(check),
            failure: Mutex::new(None),
            stopped: AtomicBool::new(false),
        };
        for (i, doc) in order {
            let calls = std::cell::Cell::new(0);
            let expired = || cancelled.expired_every(&calls, METRICS_WORKER_CHECK_EVERY);
            let result = store.metrics_document(
                &doc.key,
                &doc.file,
                &doc.declarations,
                &doc.calls,
                &regions[i],
                snapshot.evidence_gaps.contains_key(&doc.key),
                &expired,
            );
            let MetricsComputedDocument {
                computed,
                physical_lines,
                physical_bytes,
                file_role,
            } = match result {
                Ok(result) => result,
                Err(MetricsDocumentFailure::Read(error)) => {
                    return cancelled.result(Err(store.report_index_failure(error)));
                }
                Err(MetricsDocumentFailure::Other(error)) => return cancelled.result(Err(error)),
                Err(MetricsDocumentFailure::Stopped) => {
                    return cancelled.result(Err(anyhow::anyhow!("cancelled")));
                }
            };
            let gap = computed.gap().cloned();
            #[cfg(feature = "test-support")]
            architecture_execution_event(ArchitectureExecutionEvent::BeforeMetricCommit, &doc.key)?;
            let rows = computed
                .commit(&mut store.metrics_memo.lock().unwrap_or_else(|e| e.into_inner()))?;
            metrics.insert(
                doc.key.clone(),
                Arc::new(MetricsDocument {
                    rows,
                    gap,
                    physical_lines,
                    physical_bytes,
                    file_role,
                }),
            );
            #[cfg(feature = "test-support")]
            architecture_execution_event(ArchitectureExecutionEvent::MetricCommit, &doc.key)?;
            report(ArchitectureProgress {
                phase: "measuring",
                completed: metrics.len(),
                total,
            });
        }
        cancelled.check()?;
        Ok(metrics)
    }
}

impl ArchitectureCore {
    /// Every part of this core in one canonical value (#1301): two cores are equal when every
    /// read answers the same from them. The summary cache is left out; reads fill it.
    pub(crate) fn canonical(&self) -> Result<serde_json::Value> {
        use serde_json::json;
        // Exhaustive, so a new field fails to compile until it is compared.
        let Self {
            pin,
            references,
            rules,
            metrics,
            app_candidates,
            interfaces,
            summaries: _,
            dependencies,
        } = self;
        let failure = |failure: &CoreFailure| {
            let CoreFailure {
                rule,
                integrity,
                message,
            } = failure;
            json!({"failure": {"rule": rule, "integrity": integrity, "message": message}})
        };
        let rules = rules.as_ref().map(|RulesPart { content_hash, base }| {
            json!({
                "contentHash": content_hash,
                "base": base.as_ref().map_or_else(failure, RulesBase::canonical),
            })
        });
        let metrics = metrics.as_ref().map(|metrics| match metrics {
            Ok(metrics) => metrics
                .iter()
                .map(|(key, document)| {
                    let MetricsDocument {
                        rows,
                        gap,
                        physical_lines,
                        physical_bytes,
                        file_role,
                    } = document.as_ref();
                    let rows: Vec<_> = rows.iter().map(canonical_metric_row).collect();
                    json!([key, rows, gap, physical_lines, physical_bytes, file_role])
                })
                .collect(),
            Err(error) => failure(error),
        });
        let app_candidates = match app_candidates {
            Ok(crate::apps::Candidates { manifests }) => manifests
                .iter()
                .map(|(path, outcome)| json!([path, format!("{outcome:?}")]))
                .collect(),
            Err(error) => failure(error),
        };
        let interfaces = interfaces.as_ref().map(|interfaces| match interfaces {
            Ok(crate::interfaces::Extraction {
                routes,
                calls,
                matched,
                callees,
            }) => json!({
                "routes": routes.iter().map(|route| format!("{route:?}")).collect::<Vec<_>>(),
                "calls": calls.iter().map(|call| format!("{call:?}")).collect::<Vec<_>>(),
                "matched": matched.iter().map(|m| format!("{m:?}")).collect::<Vec<_>>(),
                "callees": callees,
            }),
            Err(error) => failure(error),
        });
        Ok(json!({
            "pin": pin,
            "references": references.canonical()?,
            "rules": rules,
            "metrics": metrics,
            "appCandidates": app_candidates,
            "interfaces": interfaces,
            "dependencies": dependencies.canonical(),
        }))
    }
}

/// One Architecture read for tests: each document's path and semantic rows, in debug form,
/// and the canonical core computed from them.
#[cfg(feature = "test-support")]
pub type ArchitectureReadForTests = (Vec<(String, String)>, serde_json::Value);

#[cfg(feature = "test-support")]
impl Store {
    /// Exercise the existing scoped fallback without selecting or activating a runtime owner.
    #[doc(hidden)]
    pub fn architecture_scoped_response_for_tests(
        &self,
        kind: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let response = self.evidence_response()?;
        let parsed = EvidenceResponse::architecture_arguments(kind, arguments)?;
        let config = response.architecture_config(kind, &parsed)?;
        let payload = response.architecture_response_scoped(
            kind,
            arguments,
            &parsed,
            config.as_ref(),
            &mut || Ok(()),
        )?;
        response.finish(payload)
    }

    /// Read-A identities from an admitted captured revision; no source text is retained.
    #[doc(hidden)]
    pub fn architecture_fingerprints_for_tests(
        &self,
        pin: Option<IndexPin>,
    ) -> Result<crate::architecture_inputs::ArchitectureFingerprints> {
        let response = self.evidence_response()?;
        let (_, _, fingerprints) =
            response.architecture_read_a_fingerprinted(pin, &mut || Ok(()), &|_, _| {})?;
        response.finish(fingerprints)
    }

    /// Admitted captured inputs for comparing full and scoped reference construction.
    /// The transaction ends before either resolver runs.
    #[doc(hidden)]
    pub fn architecture_reference_snapshot_for_tests(&self) -> Result<crate::references::Snapshot> {
        let response = self.evidence_response()?;
        let pin = response.selected_pin(None)?;
        let input = response.architecture_input(pin, &mut || Ok(()), &|_| {})?;
        response.finish(input.snapshot)
    }

    /// The head's Architecture read and core twice (#1301): read A and read B in one
    /// transaction, then each in its own. For each: every document's path and semantic rows,
    /// in debug form, and the canonical core computed from them, with metrics and no rules.
    #[doc(hidden)]
    pub fn architecture_split_reads_for_tests(&self) -> Result<[ArchitectureReadForTests; 2]> {
        let read = |split: bool| -> Result<_> {
            let response = self.evidence_response()?;
            let pin = response.selected_pin(None)?;
            let input = if split {
                let mut input = response.architecture_native_input(pin, &mut || Ok(()), &|_| {})?;
                response.finish(())?;
                drop(response);
                let response = self.evidence_response()?;
                response.architecture_semantic_input(&mut input, &mut || Ok(()))?;
                response.finish(input)?
            } else {
                let input = response.architecture_input(pin, &mut || Ok(()), &|_| {})?;
                response.finish(input)?
            };
            let semantic = input
                .snapshot
                .documents
                .iter()
                .map(|document| {
                    (
                        document.key.path.clone(),
                        format!("{:?}", document.semantic),
                    )
                })
                .collect();
            let core = ArchitectureCore::compute(self, input, None, true, &mut || Ok(()), &|_| {})?;
            Ok((semantic, core.canonical()?))
        };
        Ok([read(false)?, read(true)?])
    }
}

/// Every field of one metric row, including `name` and `kind`, which its JSON skips and the
/// top-cognitive answer reads.
fn canonical_metric_row(row: &DeclarationMetrics) -> serde_json::Value {
    // Exhaustive, so a new field fails to compile until it is compared.
    let DeclarationMetrics {
        role,
        source_set_id,
        language,
        path,
        sid,
        range,
        name,
        kind,
        metrics,
    } = row;
    serde_json::json!({
        "role": role,
        "sourceSetId": source_set_id,
        "language": language,
        "path": path,
        "sid": sid,
        "range": range,
        "name": name,
        "kind": kind,
        "metrics": metrics,
    })
}

/// The JSON pointer of the first place `left` and `right` differ, in key and index order.
pub(crate) fn first_difference(
    left: &serde_json::Value,
    right: &serde_json::Value,
) -> Option<String> {
    use serde_json::Value;
    match (left, right) {
        (Value::Object(left_map), Value::Object(right_map)) => {
            let keys: std::collections::BTreeSet<&String> =
                left_map.keys().chain(right_map.keys()).collect();
            keys.into_iter()
                .find_map(|key| match (left_map.get(key), right_map.get(key)) {
                    (Some(l), Some(r)) => {
                        first_difference(l, r).map(|rest| format!("/{key}{rest}"))
                    }
                    _ => Some(format!("/{key}")),
                })
        }
        (Value::Array(left_items), Value::Array(right_items)) => left_items
            .iter()
            .zip(right_items)
            .enumerate()
            .find_map(|(index, (l, r))| {
                first_difference(l, r).map(|rest| format!("/{index}{rest}"))
            })
            .or_else(|| {
                (left_items.len() != right_items.len())
                    .then(|| format!("/{}", left_items.len().min(right_items.len())))
            }),
        _ => (left != right).then(String::new),
    }
}

/// A part's outcome: a repeating failure is kept as the part's result, any other is returned.
fn kept<T>(outcome: Result<T>) -> Result<std::result::Result<T, CoreFailure>> {
    match outcome {
        Ok(value) => Ok(Ok(value)),
        Err(error) if CoreFailure::repeats(&error) => Ok(Err(CoreFailure::of(&error))),
        Err(error) => Err(error),
    }
}

/// How many manifests detect an app.
/// Whether a core extracts interfaces (#788): when its detected plus authored apps number two
/// or more. Otherwise its extraction is empty without being read (#851).
pub(crate) fn interfaces_extracted(
    config: Option<&Config>,
    candidates: &crate::apps::Candidates,
) -> bool {
    detected_apps(candidates) + config.map_or(0, Config::authored_app_count) >= 2
}

fn detected_apps(candidates: &crate::apps::Candidates) -> usize {
    candidates
        .manifests
        .iter()
        .filter(|(_, outcome)| matches!(outcome, crate::apps::Outcome::App(_)))
        .count()
}

/// Metrics of the documents already read, in parallel. Each measured document advances
/// `report` (#798), counted and reported under one lock so the published count only grows.
fn measure_documents(
    store: &Store,
    snapshot: &crate::references::Snapshot,
    regions: &[Vec<crate::native_records::ControlRegion>],
    check: &mut (impl FnMut() -> Result<()> + Send),
    report: &(dyn Fn(ArchitectureProgress) + Sync),
) -> Result<ArchitectureMetrics> {
    let total = snapshot.documents.len();
    let measured = Mutex::new(0usize);
    let progress = MetricsReadCheck {
        check: Mutex::new(check),
        failure: Mutex::new(None),
        stopped: AtomicBool::new(false),
    };
    let indexes: Vec<usize> = (0..snapshot.documents.len()).collect();
    #[cfg(feature = "test-support")]
    let observer = ArchitectureExecutionObserver::current();
    let outcomes = crate::parallel::map_ordered(
        &indexes,
        crate::parallel::default_workers(),
        &progress.stopped,
        |_, &index| {
            let document = &snapshot.documents[index];
            let calls = std::cell::Cell::new(0);
            let expired = || progress.expired_every(&calls, METRICS_WORKER_CHECK_EVERY);
            let measure = || {
                store.metrics_document(
                    &document.key,
                    &document.file,
                    &document.declarations,
                    &document.calls,
                    &regions[index],
                    snapshot.evidence_gaps.contains_key(&document.key),
                    &expired,
                )
            };
            #[cfg(feature = "test-support")]
            let outcome = observer
                .as_ref()
                .map_or(Ok(()), |observer| {
                    observer.event(ArchitectureExecutionEvent::MetricWorker, &document.key)
                })
                .map_err(MetricsDocumentFailure::Other)
                .and_then(|()| measure());
            #[cfg(not(feature = "test-support"))]
            let outcome = measure();
            let mut measured = measured.lock().unwrap_or_else(|e| e.into_inner());
            *measured += 1;
            report(ArchitectureProgress {
                phase: "measuring",
                completed: *measured,
                total,
            });
            outcome
        },
    );
    let mut documents = BTreeMap::new();
    for (document, outcome) in snapshot.documents.iter().zip(outcomes) {
        let MetricsComputedDocument {
            computed,
            physical_lines,
            physical_bytes,
            file_role,
        } = match outcome {
            Some(Ok(computed)) => computed,
            Some(Err(MetricsDocumentFailure::Read(error))) => {
                return progress.result(Err(store.report_index_failure(error)));
            }
            Some(Err(MetricsDocumentFailure::Other(error))) => {
                return progress.result(Err(error));
            }
            Some(Err(MetricsDocumentFailure::Stopped)) | None => {
                return progress.result(Err(anyhow::anyhow!("cancelled")));
            }
        };
        let gap = computed.gap().cloned();
        #[cfg(feature = "test-support")]
        architecture_execution_event(
            ArchitectureExecutionEvent::BeforeMetricCommit,
            &document.key,
        )?;
        let rows =
            computed.commit(&mut store.metrics_memo.lock().unwrap_or_else(|e| e.into_inner()))?;
        documents.insert(
            document.key.clone(),
            Arc::new(MetricsDocument {
                rows,
                gap,
                physical_lines,
                physical_bytes,
                file_role,
            }),
        );
        #[cfg(feature = "test-support")]
        architecture_execution_event(ArchitectureExecutionEvent::MetricCommit, &document.key)?;
    }
    progress.check()?;
    Ok(documents)
}

#[cfg(all(test, feature = "test-support"))]
#[path = "architecture_core_tests.rs"]
mod canonical_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeating_part_error_is_kept_and_answered_again_and_any_other_is_returned() {
        let repeats = anyhow::anyhow!("bounded_work: interface extraction budget");
        let failure = kept::<u8>(Err(repeats)).unwrap().unwrap_err();
        assert_eq!(
            failure.error().to_string(),
            "bounded_work: interface extraction budget"
        );
        assert_eq!(failure.error().to_string(), failure.error().to_string());
        assert_eq!(kept(Ok(7)).unwrap().unwrap(), 7);
        for other in ["cancelled", "storage_busy: waiting"] {
            let error = kept::<u8>(Err(anyhow::anyhow!(other))).unwrap_err();
            assert_eq!(error.to_string(), other);
        }
    }
}
