//! The metrics read service: the transport options and rows of one pinned metrics
//! projection, and the read that computes them inside an admitted response's snapshot.
//!
//! Moved up from `store.rs` (S5, #1045). The methods are still `impl Store` and
//! `impl EvidenceResponse` blocks, so they run in the same read transaction and behind the
//! same fence as before. The memo they share stays a field of `Store`.
use crate::architecture_core::ArchitectureMetrics;
use crate::model::*;
use crate::native_records::DeclarationRole;
#[cfg(test)]
use crate::store::MetricsDocumentStage;
use crate::store::{EvidenceResponse, FetchedDocument, SelectedIntegrity, Store, selected_ensure};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Validated transport options for one pinned metrics projection.
#[derive(Debug, Clone)]
pub struct MetricsOptions {
    pub pin: Option<IndexPin>,
    pub role: String,
    pub workbench: bool,
    pub scope: String,
    pub source_set_id: Option<String>,
    pub language: Option<String>,
    pub path: Option<String>,
    pub sid: Option<String>,
    pub aggregate: String,
    /// Empty means all metrics; transport rejects an explicitly empty list.
    pub metrics: Vec<String>,
    pub top_cognitive: Option<usize>,
}
impl Default for MetricsOptions {
    fn default() -> Self {
        Self {
            pin: None,
            scope: "revision".into(),
            role: "all".into(),
            workbench: false,
            source_set_id: None,
            language: None,
            path: None,
            sid: None,
            aggregate: "sum".into(),
            metrics: vec![],
            top_cognitive: None,
        }
    }
}
impl MetricsOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.role.as_str(), "production" | "all" | "test"),
            "invalid role"
        );
        ensure!(
            matches!(
                self.scope.as_str(),
                "revision" | "directory" | "file" | "declaration"
            ),
            "invalid scope"
        );
        ensure!(
            matches!(self.aggregate.as_str(), "sum" | "max" | "mean"),
            "invalid aggregate"
        );
        ensure!(
            self.metrics
                .iter()
                .all(|m| crate::metrics::METRIC_ORDER.contains(&m.as_str()))
                && self.metrics.iter().collect::<BTreeSet<_>>().len() == self.metrics.len(),
            "invalid metrics"
        );
        ensure!(
            self.top_cognitive
                .is_none_or(|count| (1..=10).contains(&count)),
            "invalid top cognitive count"
        );
        let located = self.scope != "revision";
        ensure!(
            self.source_set_id.is_some() == located && self.path.is_some() == located,
            "invalid metrics selector"
        );
        ensure!(
            self.language.is_some() == (self.scope == "file"),
            "invalid metrics language"
        );
        ensure!(
            self.sid.is_some() == (self.scope == "declaration"),
            "invalid metrics sid"
        );
        if let Some(path) = &self.path {
            ensure!(
                !path.starts_with('/')
                    && !path
                        .split('/')
                        .any(|s| s == "." || s == ".." || s.is_empty())
                    || (self.scope == "directory" && path.is_empty()),
                "invalid metrics path"
            );
        }
        Ok(())
    }

    /// `validate`, then the identifier forms a transport accepts. Every
    /// transport (CLI, daemon request, browser API) validates through this.
    /// The message names the first failing rule, in a fixed order.
    pub fn validate_transport(&self, source_set_ids: MetricsSourceSetIds) -> Result<()> {
        fn lower_hex(text: &str) -> bool {
            text.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        }
        self.validate()?;
        ensure!(
            self.source_set_id.as_deref().is_none_or(|id| {
                let Some(identity) = id.strip_prefix("source-set:v1:") else {
                    return false;
                };
                if let Some(hash) = identity.strip_prefix("path-") {
                    return hash.len() == 64 && lower_hex(hash);
                }
                source_set_ids == MetricsSourceSetIds::Current
                    && uuid::Uuid::parse_str(identity)
                        .is_ok_and(|uuid| uuid.to_string() == identity)
            }),
            "source set ID"
        );
        ensure!(
            self.sid.as_deref().is_none_or(|sid| {
                sid.len() == 39 && sid.starts_with("sid:v1:") && lower_hex(&sid[7..])
            }),
            "sid"
        );
        ensure!(
            self.path
                .as_deref()
                .is_none_or(|path| !path.contains([':', '\\', '\0'])),
            "path"
        );
        ensure!(
            self.language.as_deref().is_none_or(|language| matches!(
                language,
                "java" | "python" | "javascript" | "typescript" | "rust"
            )),
            "language"
        );
        Ok(())
    }

    /// How long one metrics read of `scope` may work: revision and directory
    /// reads aggregate many files, so they get the long budget.
    pub fn work_budget(scope: &str) -> std::time::Duration {
        std::time::Duration::from_secs(if matches!(scope, "revision" | "directory") {
            300
        } else {
            30
        })
    }
}

/// Which source-set ID forms a metrics transport accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricsSourceSetIds {
    /// Every current form: path-derived and UUID. The browser API.
    Current,
    /// Only `source-set:v1:path-<hash>`. The CLI and its daemon request
    /// reject UUID source-set IDs today (#984).
    PathOnly,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsFileRow {
    pub source_set_id: String,
    pub language: String,
    pub path: String,
    pub physical_lines: usize,
    pub file_role: DeclarationRole,
    pub role_counts: MetricsRoleCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_reason: Option<&'static str>,
    pub declaration_count: usize,
    pub metrics: serde_json::Value,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsRoleCounts {
    pub production: usize,
    pub test: usize,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsDirectoryRow {
    pub source_set_id: String,
    pub path: String,
    pub declaration_count: usize,
    pub metrics: serde_json::Value,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsDeclarationRow {
    pub role: DeclarationRole,
    pub source_set_id: String,
    pub language: String,
    pub path: String,
    pub sid: String,
    pub range: crate::native_records::Range,
    pub metrics: serde_json::Value,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsTopCognitiveRow {
    pub role: DeclarationRole,
    pub source_set_id: String,
    pub language: String,
    pub path: String,
    pub sid: String,
    pub range: crate::native_records::Range,
    pub name: Option<String>,
    pub kind: String,
    pub metrics: MetricsTopCognitiveScore,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsTopCognitiveScore {
    pub cognitive: usize,
    pub cyclomatic: usize,
    pub max_nesting: usize,
}
const LAYER_EXCLUSIONS_WARNING: &str =
    "Rules unavailable; layer-based heat exclusions were not applied.";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsPayload {
    pub metrics_version: &'static str,
    pub role: String,
    pub workbench: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_exclusions_warning: Option<&'static str>,
    pub index_generation: String,
    pub index_revision: u64,
    pub scope: String,
    pub aggregate: String,
    pub selected_metrics: Vec<String>,
    pub declarations: Vec<MetricsDeclarationRow>,
    pub files: Vec<MetricsFileRow>,
    pub directories: Vec<MetricsDirectoryRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_cognitive: Option<Vec<MetricsTopCognitiveRow>>,
    /// Selected files whose syntax exceeds the traversal bounds (#554); absent when none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub partial_files: Vec<crate::syntax_bounds::PartialFile>,
}

// Adapt the read helpers' boolean progress seam without erasing the caller's
// original typed cancellation/deadline error. The request thread and the
// metrics workers share it: the caller's check runs on whichever thread asks,
// one at a time, and its first failure stops every later check.
pub(crate) struct MetricsReadCheck<'a> {
    pub(crate) check: Mutex<&'a mut (dyn FnMut() -> Result<()> + Send)>,
    pub(crate) failure: Mutex<Option<anyhow::Error>>,
    pub(crate) stopped: AtomicBool,
}
impl MetricsReadCheck<'_> {
    /// The caller's check; its failure is kept for `result` to report.
    fn expired(&self) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return true;
        }
        let mut check = self.check.lock().unwrap_or_else(|e| e.into_inner());
        if self.stopped.load(Ordering::Acquire) {
            return true;
        }
        match (*check)() {
            Ok(()) => false,
            Err(error) => {
                self.failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_insert(error);
                self.stopped.store(true, Ordering::Release);
                true
            }
        }
    }
    /// `expired` on every `every`-th call through `calls`, else only whether
    /// a check already failed. Concurrent workers poll the caller's check this
    /// way so they don't serialize on it; the poll interval is a few rows or
    /// parser steps.
    pub(crate) fn expired_every(&self, calls: &std::cell::Cell<u32>, every: u32) -> bool {
        let n = calls.get().wrapping_add(1);
        calls.set(n);
        if n.is_multiple_of(every) {
            self.expired()
        } else {
            self.stopped.load(Ordering::Acquire)
        }
    }
    pub(crate) fn check(&self) -> Result<()> {
        if self.expired() {
            return self.result(Err(anyhow::anyhow!("cancelled")));
        }
        Ok(())
    }
    /// The caller's check failure, if one happened, else `result`.
    pub(crate) fn result<T>(&self, result: Result<T>) -> Result<T> {
        if let Some(error) = self
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            return Err(error);
        }
        result
    }
}

impl Store {
    /// Metrics of one document's stored source and native rows.
    pub(crate) fn metrics_document(
        &self,
        key: &crate::native_records::DocumentKey,
        file: &SourceFile,
        declarations: &[crate::native_records::Declaration],
        calls: &[crate::native_records::Call],
        regions: &[crate::native_records::ControlRegion],
        expired: &dyn Fn() -> bool,
    ) -> std::result::Result<MetricsComputedDocument, MetricsDocumentFailure> {
        // Metrics use the stored source and native rows as fetched.
        if file.language != key.language {
            return Err(MetricsDocumentFailure::Other(
                SelectedIntegrity("incompatible_index: metrics language mismatch".into()).into(),
            ));
        }
        if expired() {
            return Err(MetricsDocumentFailure::Stopped);
        }
        let mut roots = declarations
            .iter()
            .filter(|row| row.kind == "module" && row.ancestors.is_empty());
        let file_role = roots.next().map(|row| row.role);
        let Some(file_role) = file_role.filter(|_| roots.next().is_none()) else {
            return Err(MetricsDocumentFailure::Read(
                SelectedIntegrity("incompatible_index: metrics file module".into()).into(),
            ));
        };
        let physical_lines = file.text.lines().count();
        let source = crate::metrics::SourceInput {
            language: &key.language,
            path: &key.path,
            content_hash: &file.hash,
            text: &file.text,
        };
        let cached = self
            .metrics_memo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lookup(&source);
        crate::metrics::compute_with_cached(
            cached,
            source,
            declarations,
            calls,
            regions,
            &mut || {
                if expired() {
                    anyhow::bail!("cancelled")
                }
                Ok(())
            },
        )
        .map(|computed| MetricsComputedDocument {
            computed,
            physical_lines,
            physical_bytes: file.text.len(),
            file_role,
        })
        .map_err(MetricsDocumentFailure::Other)
    }
}

/// Metrics and file metadata from one fetched indexed document.
pub(crate) struct MetricsComputedDocument {
    pub(crate) computed: crate::metrics::Computed,
    pub(crate) physical_lines: usize,
    pub(crate) physical_bytes: usize,
    pub(crate) file_role: DeclarationRole,
}

/// Why one revision document did not produce metrics.
pub(crate) enum MetricsDocumentFailure {
    /// Reading the selected document failed; reported as a selected-read failure.
    Read(anyhow::Error),
    /// Any other failure.
    Other(anyhow::Error),
    /// The caller's check failed before this document's work.
    Stopped,
}

/// How often a metrics worker polls the caller's check, in progress steps.
/// A single-document request runs inline and checks at every step.
pub(crate) const METRICS_WORKER_CHECK_EVERY: u32 = 64;
/// At most this many documents are read into one chunk for the workers.
const METRICS_FETCH_DOCUMENTS: usize = 64;
/// The two overlapping metrics chunks together have at most this much
/// read-ahead charge. Each chunk has at most half the budget, except that a
/// document over half is computed alone. Deriving graph DTOs before charging
/// can also leave one fully fetched lookahead document carried across a chunk
/// boundary. Thus the held charge is pending + current + carried: at most
/// 64 MiB for the two ordinary chunks, plus one document within the existing
/// per-document source, ancillary, graph and F typed envelopes. This charge
/// counts derived graph bytes once; it is not a bound on decoded Rust RSS.
const METRICS_FETCH_BYTES: u64 = 64 * 1024 * 1024;

/// Test overrides for one metrics request on this thread.
#[cfg(test)]
#[derive(Clone, Copy)]
struct MetricsTestPlan {
    workers: usize,
    fetch_bytes: u64,
}
/// What one metrics request on this thread held between its reads and
/// its acceptance of each document.
#[cfg(test)]
#[derive(Debug, Default)]
struct MetricsFetchTrace {
    /// Each fetched document's read-ahead charge, in manifest order.
    charges: Vec<u64>,
    /// The most documents read and not yet accepted at once.
    max_resident_documents: usize,
    /// The most read-ahead charge held at once, including a fetched lookahead.
    max_resident_bytes: u64,
    /// (pending, current, carried) charges when a document crosses a boundary.
    boundaries: Vec<(u64, u64, u64)>,
}
#[cfg(test)]
thread_local! {
    static METRICS_TEST_PLAN: std::cell::Cell<Option<MetricsTestPlan>> =
        const { std::cell::Cell::new(None) };
    static METRICS_FETCH_TRACE: std::cell::RefCell<MetricsFetchTrace> =
        std::cell::RefCell::default();
}
#[cfg(test)]
fn trace_metrics_resident(documents: usize, bytes: u64) {
    METRICS_FETCH_TRACE.with_borrow_mut(|trace| {
        trace.max_resident_documents = trace.max_resident_documents.max(documents);
        trace.max_resident_bytes = trace.max_resident_bytes.max(bytes);
    });
}

impl EvidenceResponse {
    /// Compute metrics from one selected revision in this response's read transaction.
    pub fn metrics_at(
        &self,
        options: &MetricsOptions,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<MetricsPayload> {
        self.metrics_at_prepared(options, None, check)
    }

    /// `metrics_at`, taking this revision's rows from `prepared` when it has them.
    pub(crate) fn metrics_at_prepared(
        &self,
        options: &MetricsOptions,
        prepared: Option<&ArchitectureMetrics>,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<MetricsPayload> {
        options.validate()?;
        check()?;
        let (config, invalid_rules) = if options.workbench {
            match crate::rules::load(Path::new(self.store.workspace_root())) {
                Ok(config) => (config, false),
                Err(error) if crate::rules::diagnostic(&error).is_some() => (None, true),
                Err(error) => return Err(error),
            }
        } else {
            (None, false)
        };
        let mut payload =
            self.metrics_at_with_config(options, config.as_ref(), false, prepared, check)?;
        if invalid_rules {
            payload.layer_exclusions_warning = Some(LAYER_EXCLUSIONS_WARNING);
        }
        Ok(payload)
    }

    pub(crate) fn metrics_at_with_config(
        &self,
        options: &MetricsOptions,
        config: Option<&crate::rules::Config>,
        strict_rules: bool,
        prepared: Option<&ArchitectureMetrics>,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<MetricsPayload> {
        let progress = MetricsReadCheck {
            check: Mutex::new(check),
            failure: Mutex::new(None),
            stopped: AtomicBool::new(false),
        };
        progress.check()?;
        let selected = self.selected(options.pin)?;
        let selected_metrics: Vec<String> = crate::metrics::METRIC_ORDER
            .iter()
            .filter(|name| {
                options.metrics.is_empty() || options.metrics.iter().any(|m| m == **name)
            })
            .map(|name| (*name).to_owned())
            .collect();
        let names: Vec<&str> = selected_metrics.iter().map(String::as_str).collect();
        use crate::native_records::DocumentKey;
        let mut stmt = self.db.prepare(
            "SELECT r.source_set_id,m.language,m.path,m.coverage_diagnostic
            FROM revision_documents m
            JOIN native_revisions r ON r.id=m.revision_id
            WHERE m.revision_id=?1 ORDER BY r.source_set_id,m.language,m.path",
        )?;
        let mut documents = Vec::new();
        let mut evidence_gaps = BTreeMap::new();
        let mut cursor = stmt.query([&selected.key])?;
        while let Some(row) = cursor.next()? {
            progress.check()?;
            let key = DocumentKey {
                source_set_id: row.get(0)?,
                language: row.get(1)?,
                path: row.get(2)?,
            };
            if let Some(reason) = crate::syntax_bounds::module_only_reason(
                row.get::<_, Option<String>>(3)?.as_deref(),
            ) {
                evidence_gaps.insert(key.clone(), reason);
            }
            documents.push(key);
        }
        progress.check()?;
        // Existence is decided from the manifest, not from callable rows.
        let included: Vec<&DocumentKey> = documents
            .iter()
            .filter(|key| match options.scope.as_str() {
                "revision" => true,
                "directory" => {
                    options.source_set_id.as_ref() == Some(&key.source_set_id)
                        && options.path.as_ref().is_some_and(|path| {
                            path.is_empty() || key.path.starts_with(&format!("{path}/"))
                        })
                }
                "file" => {
                    options.source_set_id.as_ref() == Some(&key.source_set_id)
                        && options.language.as_ref() == Some(&key.language)
                        && options.path.as_ref() == Some(&key.path)
                }
                "declaration" => {
                    options.source_set_id.as_ref() == Some(&key.source_set_id)
                        && options.path.as_ref() == Some(&key.path)
                }
                _ => false,
            })
            .collect();
        if options.scope != "revision" && included.is_empty() {
            anyhow::bail!("not_found: metrics selector");
        }
        let mut layer_exclusions_warning = None;
        let hidden_layers = if options.workbench && options.role == "production" {
            if let Some(config) = config {
                match crate::rules::hidden_layer_paths(
                    config,
                    &included
                        .iter()
                        .map(|key| key.path.as_str())
                        .collect::<Vec<_>>(),
                    &mut || progress.check(),
                ) {
                    Ok(paths) => paths,
                    Err(error) if !strict_rules && crate::rules::diagnostic(&error).is_some() => {
                        layer_exclusions_warning = Some(LAYER_EXCLUSIONS_WARNING);
                        BTreeSet::new()
                    }
                    Err(error) => return Err(error),
                }
            } else {
                BTreeSet::new()
            }
        } else {
            BTreeSet::new()
        };
        let mut declarations = Vec::new();
        let mut files = Vec::new();
        let mut top_cognitive = options
            .top_cognitive
            .map(|_| Vec::<MetricsTopCognitiveRow>::new());
        let mut directories: BTreeMap<(String, String), crate::metrics::Accumulator> =
            BTreeMap::new();
        // Read source and native rows only from the selected version.
        // Every SQL read runs in order on this request's connection and snapshot.
        // Metrics run on workers, a chunk behind the
        // reads, and are accepted strictly in manifest order: the first failure
        // in that order is the one reported, and nothing after it is accepted.
        let workers = if included.len() > 1 {
            crate::parallel::default_workers()
        } else {
            1
        };
        #[cfg(test)]
        let workers = METRICS_TEST_PLAN
            .get()
            .map_or(workers, |plan| plan.workers.min(included.len().max(1)));
        let fetch_bytes = METRICS_FETCH_BYTES;
        #[cfg(test)]
        let fetch_bytes = METRICS_TEST_PLAN
            .get()
            .map_or(fetch_bytes, |plan| plan.fetch_bytes);
        let expired = || progress.expired();
        let check_every = if workers == 1 {
            1
        } else {
            METRICS_WORKER_CHECK_EVERY
        };
        let store = &self.store;
        let compute_document =
            |fetched: FetchedDocument,
             key: &DocumentKey|
             -> std::result::Result<MetricsComputedDocument, MetricsDocumentFailure> {
                let calls = std::cell::Cell::new(0);
                let expired = || progress.expired_every(&calls, check_every);
                store.metrics_document(
                    key,
                    &fetched.file,
                    &fetched.declarations,
                    &fetched.calls,
                    &fetched.regions,
                    &expired,
                )
            };
        let compute = |fetched: FetchedDocument, key: &DocumentKey| {
            #[cfg(test)]
            store.run_metrics_document_hook(&key.path, MetricsDocumentStage::Start);
            let outcome = compute_document(fetched, key);
            #[cfg(test)]
            store.run_metrics_document_hook(&key.path, MetricsDocumentStage::Done);
            outcome
        };
        let mut partial_files = Vec::new();
        let mut accept = |key: &DocumentKey,
                          outcome: Option<
            std::result::Result<MetricsComputedDocument, MetricsDocumentFailure>,
        >|
         -> Result<()> {
            let MetricsComputedDocument {
                computed,
                physical_lines,
                physical_bytes,
                file_role,
            } = match outcome {
                Some(Ok(computed)) => computed,
                Some(Err(MetricsDocumentFailure::Read(error))) => {
                    return progress.result(Err(self.store.report_index_failure(error)));
                }
                Some(Err(MetricsDocumentFailure::Other(error))) => {
                    return progress.result(Err(error));
                }
                // A document a worker never started was skipped after a check failed.
                Some(Err(MetricsDocumentFailure::Stopped)) | None => {
                    return progress.result(Err(anyhow::anyhow!("cancelled")));
                }
            };
            partial_files.extend(
                evidence_gaps
                    .get(key)
                    .map(|&reason| {
                        crate::syntax_bounds::SyntaxGap::module_only(reason, physical_bytes)
                            .file(key)
                    })
                    .or_else(|| computed.gap().map(|gap| gap.file(key))),
            );
            let mut rows = computed.commit(
                &mut self
                    .store
                    .metrics_memo
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            )?;
            let mut role_counts = MetricsRoleCounts {
                production: 0,
                test: 0,
            };
            for row in &rows {
                progress.check()?;
                match row.role {
                    DeclarationRole::Production => role_counts.production += 1,
                    DeclarationRole::Test => role_counts.test += 1,
                }
            }
            // Workbench population only: stored roles, file identity and policy stay intact.
            // Validate and measure every file before excluding its callable contributions.
            let excluded_reason = if !options.workbench {
                None
            } else if key.path.starts_with("web/vendor/") {
                Some("vendored")
            } else if options.role == "production"
                && (key.path.starts_with("tests/fixtures/")
                    || key.path.starts_with("tools/synthetic-cohorts/templates/"))
            {
                Some("fixture")
            } else if hidden_layers.contains(&key.path) {
                Some("hidden_layer")
            } else {
                None
            };
            rows.retain(|row| {
                excluded_reason.is_none()
                    && (options.role == "all" || options.role == row.role.as_str())
            });
            let mut file_fold = crate::metrics::Accumulator::default();
            let mut parents = vec![String::new()];
            let mut directory = String::new();
            for segment in key
                .path
                .split('/')
                .collect::<Vec<_>>()
                .split_last()
                .map(|(_, parents)| parents)
                .unwrap_or(&[])
            {
                if !directory.is_empty() {
                    directory.push('/');
                }
                directory.push_str(segment);
                parents.push(directory.clone());
            }
            if options.scope == "directory" {
                let selected_path = options.path.as_deref().unwrap_or_default();
                parents.retain(|path| {
                    selected_path.is_empty()
                        || path == selected_path
                        || path.starts_with(&format!("{selected_path}/"))
                });
            }
            for path in &parents {
                directories
                    .entry((key.source_set_id.clone(), path.clone()))
                    .or_default();
            }
            for row in &rows {
                progress.check()?;
                file_fold.push(&row.metrics)?;
                for path in &parents {
                    directories
                        .get_mut(&(key.source_set_id.clone(), path.clone()))
                        .expect("inserted directory")
                        .push(&row.metrics)?;
                }
            }
            for row in rows {
                let selected_callable =
                    options.scope != "declaration" || options.sid.as_ref() == Some(&row.sid);
                if selected_callable
                    && let (Some(count), Some(top)) = (options.top_cognitive, &mut top_cognitive)
                {
                    top.push(MetricsTopCognitiveRow {
                        role: row.role,
                        source_set_id: row.source_set_id.clone(),
                        language: row.language.clone(),
                        path: row.path.clone(),
                        sid: row.sid.clone(),
                        range: row.range.clone(),
                        name: row.name.clone(),
                        kind: row.kind.clone(),
                        metrics: MetricsTopCognitiveScore {
                            cognitive: row.metrics.cognitive,
                            cyclomatic: row.metrics.cyclomatic,
                            max_nesting: row.metrics.max_nesting,
                        },
                    });
                    top.sort_by(|a, b| {
                        b.metrics.cognitive.cmp(&a.metrics.cognitive).then_with(|| {
                            (
                                &a.source_set_id,
                                &a.language,
                                &a.path,
                                a.range.start,
                                a.range.end,
                                &a.sid,
                            )
                                .cmp(&(
                                    &b.source_set_id,
                                    &b.language,
                                    &b.path,
                                    b.range.start,
                                    b.range.end,
                                    &b.sid,
                                ))
                        })
                    });
                    top.truncate(count);
                }
                if options.scope == "file" || (options.scope == "declaration" && selected_callable)
                {
                    let mut projected = serde_json::to_value(&row.metrics)?;
                    projected
                        .as_object_mut()
                        .expect("metric score object")
                        .retain(|name, _| selected_metrics.iter().any(|selected| selected == name));
                    declarations.push(MetricsDeclarationRow {
                        role: row.role,
                        source_set_id: row.source_set_id,
                        language: row.language,
                        path: row.path,
                        sid: row.sid,
                        range: row.range,
                        metrics: projected,
                    });
                }
            }
            if options.scope != "declaration" {
                files.push(MetricsFileRow {
                    source_set_id: key.source_set_id.clone(),
                    language: key.language.clone(),
                    path: key.path.clone(),
                    physical_lines,
                    file_role,
                    role_counts,
                    excluded_reason,
                    declaration_count: file_fold.count(),
                    metrics: file_fold.project(&options.aggregate, &names)?,
                });
            }
            Ok(())
        };
        if let Some(prepared) = prepared {
            for &key in &included {
                let document = prepared
                    .get(key)
                    .context("store_unavailable: prepared metrics document")?;
                progress.check()?;
                accept(
                    key,
                    Some(Ok(MetricsComputedDocument {
                        computed: crate::metrics::Computed::ready(
                            document.rows.clone(),
                            document.gap.clone(),
                        ),
                        physical_lines: document.physical_lines,
                        physical_bytes: document.physical_bytes,
                        file_role: document.file_role,
                    })),
                )?;
            }
        }
        std::thread::scope(|scope| -> Result<()> {
            if prepared.is_some() {
                return Ok(());
            }
            type Fetched = std::result::Result<FetchedDocument, MetricsDocumentFailure>;
            let mut remaining = included.iter();
            let mut exhausted = false;
            // A document whose rows did not fit beside what is already held.
            let mut carried: Option<(&DocumentKey, FetchedDocument)> = None;
            let mut pending: Option<(
                Vec<&DocumentKey>,
                std::thread::ScopedJoinHandle<'_, Vec<Option<_>>>,
            )> = None;
            // The read-ahead charge of the chunk the workers hold, for the trace.
            #[cfg(test)]
            let mut pending_bytes = 0u64;
            let chunk_bytes = fetch_bytes / 2;
            loop {
                // Read the next chunk while the workers compute the previous one.
                // The two ordinary chunks together stay within the budget.
                // A document is charged its stored rows once fetched, so one
                // fully fetched document may be carried past that budget.
                // A document over half the budget is computed alone.
                let mut keys: Vec<&DocumentKey> = Vec::new();
                let mut fetched: Vec<std::sync::Mutex<Option<Fetched>>> = Vec::new();
                let mut bytes = 0u64;
                let mut alone = false;
                while keys.len() < METRICS_FETCH_DOCUMENTS {
                    let (key, item) = match carried.take() {
                        Some(carry) => carry,
                        None => {
                            if exhausted {
                                break;
                            }
                            let Some(&key) = remaining.next() else {
                                exhausted = true;
                                break;
                            };
                            let item = if progress.expired() {
                                Err(MetricsDocumentFailure::Stopped)
                            } else {
                                self.store
                                    .fetch_selected_header(
                                        &self.db,
                                        &key.path,
                                        &selected,
                                        Some(&expired),
                                    )
                                    .and_then(|header| {
                                        self.store.fetch_selected_rows(
                                            &self.db,
                                            header,
                                            &selected,
                                            Some(&expired),
                                        )
                                    })
                                    .map_err(MetricsDocumentFailure::Read)
                            };
                            match item {
                                Ok(item) => (key, item),
                                Err(failure) => {
                                    keys.push(key);
                                    fetched.push(std::sync::Mutex::new(Some(Err(failure))));
                                    exhausted = true;
                                    break;
                                }
                            }
                        }
                    };
                    let charge = item.stored_bytes;
                    // The newly fetched item is resident before deciding whether
                    // it fits: count pending + current + this lookahead even if
                    // pending is about to be joined and accepted.
                    #[cfg(test)]
                    trace_metrics_resident(
                        fetched.len() + pending.as_ref().map_or(0, |(keys, _)| keys.len()) + 1,
                        pending_bytes.saturating_add(bytes).saturating_add(charge),
                    );
                    let oversized = charge > chunk_bytes;
                    if !keys.is_empty() && (oversized || bytes.saturating_add(charge) > chunk_bytes)
                    {
                        #[cfg(test)]
                        METRICS_FETCH_TRACE.with_borrow_mut(|trace| {
                            trace.boundaries.push((pending_bytes, bytes, charge));
                        });
                        carried = Some((key, item));
                        break;
                    }
                    if oversized && pending.is_some() {
                        #[cfg(test)]
                        METRICS_FETCH_TRACE.with_borrow_mut(|trace| {
                            trace.boundaries.push((pending_bytes, bytes, charge));
                        });
                        carried = Some((key, item));
                        break;
                    }
                    keys.push(key);
                    fetched.push(std::sync::Mutex::new(Some(Ok(item))));
                    bytes = bytes.saturating_add(charge);
                    #[cfg(test)]
                    {
                        METRICS_FETCH_TRACE.with_borrow_mut(|trace| trace.charges.push(charge));
                    }
                    if oversized {
                        alone = true;
                        break;
                    }
                }
                if let Some((keys, handle)) = pending.take() {
                    #[cfg(test)]
                    {
                        pending_bytes = 0;
                    }
                    let outcomes = handle
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                    for (key, outcome) in keys.into_iter().zip(outcomes) {
                        accept(key, outcome)?;
                    }
                }
                if keys.is_empty() {
                    if carried.is_some() {
                        continue;
                    }
                    return Ok(());
                }
                let run = {
                    let compute = &compute;
                    let stopped = &progress.stopped;
                    let keys = keys.clone();
                    move || {
                        crate::parallel::map_ordered(&fetched, workers, stopped, |index, item| {
                            let item = item
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .take()
                                .expect("each fetched document is computed once");
                            item.and_then(|document| compute(document, keys[index]))
                        })
                    }
                };
                if workers == 1 || alone {
                    // Nothing else is read while this chunk is held.
                    for (key, outcome) in keys.into_iter().zip(run()) {
                        accept(key, outcome)?;
                    }
                } else {
                    #[cfg(test)]
                    {
                        pending_bytes = bytes;
                    }
                    pending = Some((keys, scope.spawn(run)));
                }
            }
        })?;
        if options.scope == "declaration" {
            if declarations.is_empty() {
                anyhow::bail!("not_found: metrics declaration");
            }
            selected_ensure!(
                declarations.len() == 1,
                "incompatible_index: ambiguous metrics declaration"
            );
        }
        declarations.sort_by(|a, b| {
            (
                &a.source_set_id,
                &a.language,
                &a.path,
                a.range.start,
                a.range.end,
                &a.sid,
            )
                .cmp(&(
                    &b.source_set_id,
                    &b.language,
                    &b.path,
                    b.range.start,
                    b.range.end,
                    &b.sid,
                ))
        });
        files.sort_by(|a, b| {
            (&a.source_set_id, &a.language, &a.path).cmp(&(&b.source_set_id, &b.language, &b.path))
        });
        let directory_rows = if matches!(options.scope.as_str(), "revision" | "directory") {
            directories
                .into_iter()
                .map(|((source_set_id, path), fold)| {
                    progress.check()?;
                    Ok(MetricsDirectoryRow {
                        source_set_id,
                        path,
                        declaration_count: fold.count(),
                        metrics: fold.project(&options.aggregate, &names)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            vec![]
        };
        progress.check()?;
        Ok(MetricsPayload {
            metrics_version: crate::metrics::VERSION,
            role: options.role.clone(),
            workbench: options.workbench,
            layer_exclusions_warning,
            index_generation: selected.pin.index_generation.to_string(),
            index_revision: selected.pin.index_revision,
            scope: options.scope.clone(),
            aggregate: options.aggregate.clone(),
            selected_metrics,
            declarations,
            files,
            directories: directory_rows,
            top_cognitive,
            partial_files,
        })
    }
}

#[cfg(test)]
#[path = "metrics_read_parallel_tests.rs"]
mod parallel_tests;
