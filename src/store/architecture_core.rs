//! What Architecture needs from one revision, computed once (#556).
//!
//! References, rules and metrics all start from the same native rows. A core reads them once
//! and keeps only what later reads select from: no source text, syntax trees or semantic rows.
//! `crate::http::architecture` owns building one per head; reads project from it. Only the
//! read itself holds a SQLite transaction, so the longer computing never delays a publication.
use super::{
    EvidenceResponse, METRICS_WORKER_CHECK_EVERY, MetricsComputedDocument, MetricsDocumentFailure,
    MetricsReadCheck, SelectedIntegrity, Store,
};
use crate::metrics::DeclarationMetrics;
use crate::model::IndexPin;
use crate::native_evidence::{DeclarationRole, DocumentKey};
use crate::references::{Projection, ReferenceBase};
use crate::rules::{Config, RuleError, RulesBase};
use anyhow::Result;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// The checked arguments of one architecture operation.
pub(crate) struct ArchitectureArguments {
    pub(super) pin: Option<IndexPin>,
    pub(super) paths: Vec<String>,
    pub(super) sample_limit: usize,
}

impl ArchitectureArguments {
    pub(crate) fn pin(&self) -> Option<IndexPin> {
        self.pin
    }
    pub(crate) fn scoped(&self) -> bool {
        !self.paths.is_empty()
    }
}

/// How far one core build has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ArchitectureProgress {
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
    pub(super) rows: Vec<DeclarationMetrics>,
    pub(super) gap: Option<crate::syntax_bounds::SyntaxGap>,
    pub(super) physical_lines: usize,
    pub(super) file_role: DeclarationRole,
}
pub(crate) type ArchitectureMetrics = BTreeMap<DocumentKey, MetricsDocument>;

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

/// A shared finished summary.
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
    /// Finished summaries; one per valid role, aggregate and workbench choice.
    summaries: Mutex<BTreeMap<SummaryKey, Arc<serde_json::Value>>>,
}

impl ArchitectureCore {
    pub(crate) fn references(
        &self,
        paths: &[String],
        sample_limit: usize,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<Projection> {
        self.references.select(paths, sample_limit, check)
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
    /// Metrics rows for a read that can compute its own if this core failed them.
    pub(crate) fn metrics_if_computed(&self) -> Option<&ArchitectureMetrics> {
        self.metrics.as_ref()?.as_ref().ok()
    }
    /// The rules file hash this core evaluated.
    pub(crate) fn rules_hash(&self) -> Option<&str> {
        self.rules.as_ref().map(|part| part.content_hash.as_str())
    }
    pub(super) fn summary(&self, key: &SummaryKey) -> Option<Arc<serde_json::Value>> {
        self.summaries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }
    pub(super) fn remember_summary(
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
    regions: Vec<Vec<crate::native_evidence::ControlRegion>>,
}

impl EvidenceResponse {
    /// The documents a core computes from.
    pub(crate) fn architecture_input(
        &self,
        pin: IndexPin,
        check: &mut (impl FnMut() -> Result<()> + Send),
        progress: &(dyn Fn(ArchitectureProgress) + Sync),
    ) -> Result<ArchitectureInput> {
        let (snapshot, regions) = self
            .architecture_snapshot_regions(Some(pin), check, &|completed, total| {
                progress(ArchitectureProgress {
                    phase: "reading",
                    completed,
                    total,
                });
            })
            .map_err(|error| self.store.report_index_failure(error))?;
        Ok(ArchitectureInput { snapshot, regions })
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
        let ArchitectureInput { snapshot, regions } = input;
        let total = snapshot.documents.len();
        let report = |phase| {
            progress(ArchitectureProgress {
                phase,
                completed: 0,
                total,
            });
        };
        report("resolving");
        let references = crate::references::resolve_all(&snapshot, check)?;
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
            Some(match measure_documents(store, &snapshot, &regions, check) {
                Ok(metrics) => Ok(metrics),
                Err(error) if CoreFailure::repeats(&error) => Err(CoreFailure::of(&error)),
                Err(error) => return Err(error),
            })
        } else {
            None
        };
        check()?;
        Ok(Self {
            pin: snapshot.pin,
            references,
            rules,
            metrics,
            summaries: Mutex::default(),
        })
    }
}

/// Metrics of the documents already read, in parallel.
fn measure_documents(
    store: &Store,
    snapshot: &crate::references::Snapshot,
    regions: &[Vec<crate::native_evidence::ControlRegion>],
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<ArchitectureMetrics> {
    let progress = MetricsReadCheck {
        check: Mutex::new(check),
        failure: Mutex::new(None),
        stopped: AtomicBool::new(false),
    };
    let indexes: Vec<usize> = (0..snapshot.documents.len()).collect();
    let outcomes = crate::parallel::map_ordered(
        &indexes,
        crate::parallel::default_workers(),
        &progress.stopped,
        |_, &index| {
            let document = &snapshot.documents[index];
            let calls = std::cell::Cell::new(0);
            let expired = || progress.expired_every(&calls, METRICS_WORKER_CHECK_EVERY);
            store.metrics_document(
                &document.key,
                &document.file,
                &document.declarations,
                &document.calls,
                &regions[index],
                &expired,
            )
        },
    );
    let mut documents = BTreeMap::new();
    for (document, outcome) in snapshot.documents.iter().zip(outcomes) {
        let MetricsComputedDocument {
            computed,
            physical_lines,
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
        let rows =
            computed.commit(&mut store.metrics_memo.lock().unwrap_or_else(|e| e.into_inner()))?;
        documents.insert(
            document.key.clone(),
            MetricsDocument {
                rows,
                gap,
                physical_lines,
                file_role,
            },
        );
    }
    progress.check()?;
    Ok(documents)
}
