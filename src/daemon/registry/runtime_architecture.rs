//! The checkout runtime's metrics, rules-edit and Architecture reads, and the wait for the
//! Architecture owner that precedes them.
use super::{CheckoutEvidence, CheckoutRuntime, MetricsResponseHooks};
use crate::http;
use crate::model::IndexPin;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// How an Architecture read that waits for the owner depends on the rules file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArchitectureRules {
    Unused,
    /// It needs this file's rule rows.
    Evaluated,
    /// It needs this file's rule rows when there is a file: each file's app (#837, #851).
    IfPresent,
    /// A summary: it needs them only if the file has layers.
    Layers,
}

/// What one Architecture read uses from a core.
#[derive(Clone, Copy)]
struct CoreNeeds {
    /// The core must have evaluated the read's rules file.
    rules: bool,
    /// A core computed for this read alone also measures metrics.
    metrics: bool,
    /// An older retained pin is computed on the request.
    compute_older_pin: bool,
}

impl CheckoutRuntime {
    pub fn metrics_response(
        &self,
        options: &crate::metrics_read::MetricsOptions,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
        hooks: MetricsResponseHooks<'_>,
        max_bytes: usize,
    ) -> anyhow::Result<(Vec<u8>, bool)> {
        // A running build makes these rows anyway; a narrow read does not wait for it.
        if matches!(options.scope.as_str(), "revision" | "directory") {
            self.await_architecture_metrics(options.pin, check);
        }
        self.projected_response(check, hooks, max_bytes, |response, check| {
            options.validate()?;
            let pin = response.selected_pin(options.pin)?;
            let core = self.resources()?.scheduler.architecture_core_if_available(
                pin,
                Duration::ZERO,
                &mut || Ok(()),
            );
            let prepared = core.as_ref().and_then(|core| core.metrics_if_computed());
            response.metrics_at_prepared(options, prepared, &mut || check())
        })
    }

    pub(crate) fn rules_edit_response(
        &self,
        request: &crate::rules_edit::Request,
        apply: bool,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
        max_bytes: usize,
        before_commit: &mut impl FnMut(),
    ) -> anyhow::Result<(Vec<u8>, bool, Option<&'static str>)> {
        request.validate(apply)?;
        // The blocking request owns this lock, never the registry or an actor.
        let _owner = self
            .rules_edit_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        check()?;
        let resources = self.resources()?;
        let root = Path::new(resources.store.workspace_root());
        resources.store.verify_root()?;
        let (response, catching_up) = self.evidence_response()?;
        let prepared = (|| {
            let base = crate::rules_edit::load_base(root, &request.base_hash)?;
            let proposal = crate::rules_edit::propose(&base.config, request, check)?;
            Ok::<_, anyhow::Error>((base, proposal))
        })();
        let (base, proposal) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(diagnostic) = crate::rules::diagnostic(&error) {
                    crate::metrics::serialize_checked(
                        &serde_json::json!({"error":diagnostic}),
                        max_bytes,
                        check,
                    )?;
                }
                check()?;
                response.finish(())?;
                return Err(error);
            }
        };
        let pin = request.pin()?;
        if let Err(error) =
            response.validate_rules_edit(pin, &base.config, &proposal.config, request, check)
        {
            if let Some(diagnostic) = crate::rules::diagnostic(&error) {
                crate::metrics::serialize_checked(
                    &serde_json::json!({"error":diagnostic}),
                    max_bytes,
                    check,
                )?;
            }
            check()?;
            response.finish(())?;
            return Err(error);
        }
        let preview = serde_json::json!({"payload":proposal.preview(pin, &request.base_hash),"catchingUp":catching_up});
        let preview_bytes = crate::metrics::serialize_checked(&preview, max_bytes, check)?;
        response.finish(())?;
        if !apply {
            return Ok((preview_bytes, catching_up, None));
        }
        anyhow::ensure!(
            request.proposed_hash.as_deref() == Some(proposal.config.content_hash.as_str()),
            "rules_conflict: proposed bytes changed; preview again"
        );
        let mut committed = serde_json::json!({"payload":{"indexGeneration":pin.index_generation.to_string(),"indexRevision":pin.index_revision,"rulesFile":crate::rules::descriptor(&proposal.config),"mutationOutcome":"committed"},"catchingUp":catching_up});
        // Bound both possible post-rename replies before touching the file.
        let committed_bytes = crate::metrics::serialize_checked(&committed, max_bytes, check)?;
        committed["payload"]["mutationOutcome"] = serde_json::json!("durability_uncertain");
        let uncertain_bytes = crate::metrics::serialize_checked(&committed, max_bytes, check)?;
        let outcome = crate::rules_edit::commit(root, &base, &proposal.config, || {
            before_commit();
            check()?;
            resources.store.verify_root()?;
            response.finish(())
        })?;
        // The next summary needs this file's core: start it now.
        resources.scheduler.architecture_hint();
        let bytes = if outcome == "committed" {
            committed_bytes
        } else {
            uncertain_bytes
        };
        Ok((bytes, catching_up, Some(outcome)))
    }

    pub fn architecture_response(
        &self,
        options: &crate::metrics_read::MetricsOptions,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
        hooks: MetricsResponseHooks<'_>,
        max_bytes: usize,
    ) -> anyhow::Result<(Vec<u8>, bool)> {
        if options.validate().is_ok() {
            let wait = self.resources()?.scheduler.architecture_browser_wait();
            self.await_architecture(options.pin, ArchitectureRules::Layers, true, wait, check);
        }
        self.projected_response(check, hooks, max_bytes, |response, check| {
            options.validate()?;
            check()?;
            let resources = self.resources()?;
            let root = Path::new(resources.store.workspace_root());
            let config = crate::rules::load(root)?;
            use crate::architecture_core::ArchitectureSummary;
            let view = response.architecture_view_head(options, config.as_ref())?;
            let pin = match &view {
                crate::architecture_core::ArchitectureView::Ready { pin, .. } => *pin,
                crate::architecture_core::ArchitectureView::Done(payload) => {
                    return Ok(ArchitectureSummary(Arc::new(payload.clone())));
                }
            };
            let config = config.as_ref().expect("ready config");
            let mut check = || check();
            let core = self.architecture_core_for_read(
                response,
                pin,
                Some(config),
                CoreNeeds {
                    rules: true,
                    metrics: true,
                    compute_older_pin: false,
                },
                &mut check,
            )?;
            response
                .architecture_view_at(options, view, config, &core, &mut check)
                .map(ArchitectureSummary)
        })
    }

    /// Wait for the owner before admitting a read. Waiting inside one would hold SQLite's
    /// shared lock against the publication and the other readers. The read that follows
    /// decides what to answer, so this needs no outcome and reports no failure.
    fn await_architecture(
        &self,
        pin: Option<IndexPin>,
        rules: ArchitectureRules,
        older: bool,
        wait: Duration,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
    ) {
        let Ok(resources) = self.resources() else {
            return;
        };
        let target = (|| {
            let (response, _) = self.evidence_response()?;
            let pin = response.selected_pin(pin)?;
            let current = pin == response.selected_pin(None)?;
            let root = Path::new(resources.store.workspace_root());
            let config = if rules == ArchitectureRules::Unused {
                None
            } else {
                crate::rules::load(root)?
            };
            response.finish((pin, current, config))
        })();
        let Ok((pin, current, config)) = target else {
            return;
        };
        // An MCP or CLI read of an older revision computes it, in its admitted read.
        if !current && !older {
            return;
        }
        // A read of rules there are none of fails at once. So does a summary without layers.
        if matches!(
            rules,
            ArchitectureRules::Evaluated | ArchitectureRules::Layers
        ) && config.is_none()
            || rules == ArchitectureRules::Layers
                && config
                    .as_ref()
                    .is_some_and(|config| crate::rules::view_layers(config).is_empty())
        {
            return;
        }
        // A read that evaluates rules waits for a core of exactly the current file, or of no
        // file once it is removed (#851).
        let hash = config.as_ref().map(|config| config.content_hash.as_str());
        let rules = if rules == ArchitectureRules::Unused {
            crate::http::architecture::CoreRules::Any
        } else {
            crate::http::architecture::CoreRules::Exactly(hash)
        };
        if !current {
            resources.scheduler.architecture_request_older(pin, rules);
        }
        resources
            .scheduler
            .architecture_wait(pin, rules, wait, check);
    }

    fn await_architecture_metrics(
        &self,
        pin: Option<IndexPin>,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
    ) {
        let Ok(resources) = self.resources() else {
            return;
        };
        let target = (|| {
            let (response, _) = self.evidence_response()?;
            let pin = response.selected_pin(pin)?;
            response.finish(pin)
        })();
        if let Ok(pin) = target {
            resources.scheduler.architecture_core_if_available(
                pin,
                crate::http::architecture::METRICS_WAIT,
                check,
            );
        }
    }

    /// Before a CLI architecture read: wait at most `wait` for the owner. An MCP read waits
    /// after its admitted read found the core building (#1252).
    pub fn await_architecture_tool(
        &self,
        kind: &str,
        arguments: &serde_json::Value,
        wait: Duration,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
    ) {
        if let Ok(parsed) = crate::store::EvidenceResponse::architecture_arguments(kind, arguments)
        {
            let rules = if matches!(kind, "check" | "explain") {
                ArchitectureRules::Evaluated
            } else if parsed.apps() {
                ArchitectureRules::IfPresent
            } else {
                ArchitectureRules::Unused
            };
            // A scoped read waits like any other: it does not repeat the owner's build (#798).
            self.await_architecture(parsed.pin(), rules, false, wait, check);
        }
    }

    /// #1252: after an MCP read found its core building and ended, wait at most `wait` for the
    /// owner outside any read. `None` when the read should be admitted again.
    pub(crate) fn architecture_still_building(
        &self,
        building: &crate::http::architecture::ArchitectureBuilding,
        wait: Duration,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
    ) -> Option<crate::http::architecture::ArchitectureBuilding> {
        let scheduler = self.resources().ok()?.scheduler.clone();
        scheduler.architecture_still_building(building, wait, check)
    }

    /// One read's core. The owner computes the head's once, and an admitted read does not
    /// wait for it. An older retained pin keeps the page's reads with the owner too, which
    /// builds it after the head; a tool read computes it, which is rare.
    fn architecture_core_for_read(
        &self,
        response: &CheckoutEvidence,
        pin: IndexPin,
        config: Option<&crate::rules::Config>,
        needs: CoreNeeds,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
    ) -> anyhow::Result<Arc<crate::architecture_core::ArchitectureCore>> {
        // #851: a read that needs the rules needs exactly the current file's, or a core built
        // without one when there is none; a core built under a removed file never answers it.
        let rules = if needs.rules {
            crate::http::architecture::CoreRules::Exactly(
                config.map(|config| config.content_hash.as_str()),
            )
        } else {
            crate::http::architecture::CoreRules::Any
        };
        // #1131: any other read still resolves references through the current file's specifier
        // mappings, none when it is missing or does not load. It takes a core of another file
        // whose mappings are the same; otherwise the current file's core, or one of none.
        let resources = self.resources()?;
        let current = (!needs.rules).then(|| {
            match crate::rules::load(Path::new(resources.store.workspace_root())) {
                Ok(Some(config)) => (
                    Some(config.content_hash.clone()),
                    config.specifiers().to_vec(),
                ),
                _ => (None, Vec::new()),
            }
        });
        let fits = |core: &crate::architecture_core::ArchitectureCore| {
            current
                .as_ref()
                .is_none_or(|(_, mappings)| core.specifiers() == mappings.as_slice())
        };
        let exact = match &current {
            Some((hash, _)) => crate::http::architecture::CoreRules::Exactly(hash.as_deref()),
            None => rules,
        };
        let scheduler = resources.scheduler.clone();
        if pin == response.selected_pin(None)? {
            let core = scheduler.architecture_core(pin, rules, Duration::ZERO, check)?;
            if fits(&core) {
                return Ok(core);
            }
            return scheduler.architecture_core(pin, exact, Duration::ZERO, check);
        }
        if let Some(core) =
            scheduler.architecture_core_if_available(pin, Duration::ZERO, &mut || Ok(()))
            && rules.admits(core.rules_hash())
            && fits(&core)
        {
            return Ok(core);
        }
        if !needs.compute_older_pin {
            scheduler.architecture_request_older(pin, exact);
            return scheduler.architecture_core(pin, exact, Duration::ZERO, check);
        }
        response
            .architecture_core_at(pin, config, needs.metrics, check, &|_| {})
            .map(Arc::new)
    }

    /// #851: the files of the apps `app` names, by name or manifest path, as an unfiltered
    /// `trellis_rules_check` assigns them, for a find, search or call graph read at the
    /// admitted head. The head's core is the owner's; a read that finds it building fails
    /// [`crate::http::architecture::ArchitectureBuilding`], which the caller waits out.
    pub fn mcp_app_files(
        &self,
        response: &CheckoutEvidence,
        app: &str,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
    ) -> anyhow::Result<std::collections::BTreeSet<String>> {
        let (config, core) = self.app_core(response, false, check)?;
        let (assignment, whole) =
            crate::store::EvidenceResponse::app_assignment_at(config.as_ref(), &core, check)?;
        let named = assignment.named(app)?;
        crate::store::EvidenceResponse::app_files(&assignment, &whole, &named, check)
    }

    /// #851: `trellis_workspace_describe`'s `apps`, from the head's core if the owner has
    /// published it, without waiting or asking for one: `ready` with the effective inventory,
    /// `building` until the owner (which builds every head, #798) has it for the current rules
    /// file, `invalidRules` when the rules file is rejected, and `unavailable` otherwise.
    pub fn mcp_describe_apps(
        &self,
        response: &CheckoutEvidence,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
    ) -> anyhow::Result<serde_json::Value> {
        let state = |state: &str| serde_json::json!({"state": state, "items": [], "noApp": null});
        let answer = self
            .app_core(response, true, check)
            .and_then(|(config, core)| {
                let (assignment, whole) = crate::store::EvidenceResponse::app_assignment_at(
                    config.as_ref(),
                    &core,
                    check,
                )?;
                crate::store::EvidenceResponse::app_inventory(&assignment, &whole, check)
            });
        match answer {
            Ok(apps) => Ok(apps),
            // Its message starts `deadline_exceeded`, so it is matched first.
            Err(error)
                if error
                    .downcast_ref::<crate::http::architecture::ArchitectureBuilding>()
                    .is_some() =>
            {
                Ok(state("building"))
            }
            Err(error) if crate::store::cli_read_retryable_contention(&error) => Err(error),
            Err(error) if error.to_string().starts_with("deadline_exceeded") => Err(error),
            Err(error) if crate::rules::diagnostic(&error).is_some() => Ok(state("invalidRules")),
            Err(_) => Ok(state("unavailable")),
        }
    }

    /// The head's core and the rules file it evaluated, for each file's app (#851). A
    /// `passive` read only takes a published core; it never asks the owner for one.
    fn app_core(
        &self,
        response: &CheckoutEvidence,
        passive: bool,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
    ) -> anyhow::Result<(
        Option<crate::rules::Config>,
        Arc<crate::architecture_core::ArchitectureCore>,
    )> {
        let parsed =
            crate::store::EvidenceResponse::architecture_arguments("apps", &serde_json::json!({}))?;
        let config = response.architecture_config("apps", &parsed)?;
        let pin = response.selected_pin(None)?;
        if passive {
            let hash = config.as_ref().map(|config| config.content_hash.as_str());
            let scheduler = self.resources()?.scheduler.clone();
            return match scheduler
                .architecture_core_if_available(pin, Duration::ZERO, &mut || Ok(()))
            {
                Some(core)
                    if crate::http::architecture::CoreRules::Exactly(hash)
                        .admits(core.rules_hash()) =>
                {
                    Ok((config, core))
                }
                _ => Err(crate::http::architecture::ArchitectureBuilding {
                    pin,
                    progress: None,
                    rules: crate::http::architecture::CoreRules::Exactly(hash).into(),
                }
                .into()),
            };
        }
        let core = self.architecture_core_for_read(
            response,
            pin,
            config.as_ref(),
            CoreNeeds {
                rules: true,
                metrics: false,
                compute_older_pin: false,
            },
            check,
        )?;
        Ok((config, core))
    }

    /// #853: reads a Sequence view's client calls against the HTTP interfaces of the view's
    /// revision, from the core the owner published for it under the current rules file. It
    /// never waits for or asks the owner for a core: without one the view says `notMeasured`.
    pub fn sequence_interfaces(
        &self,
        response: &CheckoutEvidence,
        view: &mut crate::behavior::SequenceView,
        visible_bound: bool,
    ) -> anyhow::Result<()> {
        let mut check = || Ok(());
        if crate::sequence_hops::call_ids(&view.steps).is_empty() {
            return Ok(());
        }
        let parsed =
            crate::store::EvidenceResponse::architecture_arguments("apps", &serde_json::json!({}))?;
        let config = match response.architecture_config("apps", &parsed) {
            Ok(config) => config,
            Err(error) if crate::store::cli_read_retryable_contention(&error) => return Err(error),
            Err(_) => {
                return response.annotate_sequence_interfaces(
                    view,
                    crate::sequence_hops::InterfaceEvidence::Unavailable,
                    visible_bound,
                    &mut check,
                );
            }
        };
        let rules = http::architecture::CoreRules::Exactly(
            config.as_ref().map(|config| config.content_hash.as_str()),
        );
        let core = self
            .resources()?
            .scheduler
            .architecture_core_if_available(view.revision, Duration::ZERO, &mut check)
            .filter(|core| rules.admits(core.rules_hash()));
        let evidence = match &core {
            Some(core) => crate::sequence_hops::InterfaceEvidence::Core {
                config: config.as_ref(),
                core,
            },
            None => crate::sequence_hops::InterfaceEvidence::NotMeasured,
        };
        response.annotate_sequence_interfaces(view, evidence, visible_bound, &mut check)
    }

    /// An MCP or CLI architecture operation in this admitted read.
    pub fn architecture_tool(
        &self,
        response: &CheckoutEvidence,
        kind: &str,
        arguments: &serde_json::Value,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
    ) -> anyhow::Result<serde_json::Value> {
        let parsed = crate::store::EvidenceResponse::architecture_arguments(kind, arguments)?;
        check()?;
        let config = response.architecture_config(kind, &parsed)?;
        let pin = response.selected_pin(parsed.pin())?;
        let needs = CoreNeeds {
            // #837: app groups evaluate the rules as check does.
            rules: matches!(kind, "check" | "explain") || parsed.apps(),
            metrics: false,
            compute_older_pin: true,
        };
        let answer = self
            .architecture_core_for_read(response, pin, config.as_ref(), needs, check)
            .and_then(|core| {
                response.architecture_response_at(
                    kind,
                    arguments,
                    &parsed,
                    config.as_ref(),
                    &core,
                    check,
                )
            });
        match answer {
            // The revision is over a whole-revision budget, and a narrow read need not be: it
            // computes its own scope within its deadline. A read that finds the owner still
            // building answers `building` instead of repeating the build's work (#798).
            Err(error) if parsed.scoped() && error.to_string().starts_with("bounded_work") => {
                response.architecture_response_scoped(
                    kind,
                    arguments,
                    &parsed,
                    config.as_ref(),
                    check,
                )
            }
            answer => answer,
        }
    }

    /// One paged Architecture detail read, bounded like the summary.
    pub fn architecture_details_response(
        &self,
        query: &crate::architecture_details::Query,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
        hooks: MetricsResponseHooks<'_>,
        max_bytes: usize,
    ) -> anyhow::Result<(Vec<u8>, bool)> {
        let wait = self.resources()?.scheduler.architecture_browser_wait();
        self.await_architecture(
            Some(query.pin),
            ArchitectureRules::Evaluated,
            true,
            wait,
            check,
        );
        self.projected_response(check, hooks, max_bytes, |response, check| {
            check()?;
            let config = response.architecture_details_config(query)?;
            let mut check = || check();
            let core = self.architecture_core_for_read(
                response,
                query.pin,
                Some(&config),
                CoreNeeds {
                    rules: true,
                    metrics: false,
                    compute_older_pin: false,
                },
                &mut check,
            )?;
            response.architecture_details_at(query, &config, &core, &mut check)
        })
    }

    fn projected_response<P: serde::Serialize, F: FnMut() -> anyhow::Result<()> + Send>(
        &self,
        check: &mut F,
        hooks: MetricsResponseHooks<'_>,
        max_bytes: usize,
        materialize: impl FnOnce(
            &CheckoutEvidence,
            &mut (dyn FnMut() -> anyhow::Result<()> + Send),
        ) -> anyhow::Result<P>,
    ) -> anyhow::Result<(Vec<u8>, bool)> {
        use serde::Serialize;
        struct Bounded<'a, F> {
            bytes: Vec<u8>,
            check: &'a mut F,
            on_write: &'a mut dyn FnMut(usize),
            max_bytes: usize,
            failure: Option<anyhow::Error>,
        }
        impl<F: FnMut() -> anyhow::Result<()>> std::io::Write for Bounded<'_, F> {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                (self.on_write)(self.bytes.len());
                if let Err(error) = (self.check)() {
                    self.failure = Some(error);
                    return Err(std::io::Error::other("metrics check failed"));
                }
                if data.len() > self.max_bytes.saturating_sub(self.bytes.len()) {
                    self.failure = Some(anyhow::anyhow!("response_too_large"));
                    return Err(std::io::Error::other("response_too_large"));
                }
                self.bytes.extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Envelope<'a, P> {
            catching_up: bool,
            payload: &'a P,
        }
        let (response, catching_up) = self.evidence_response()?;
        (hooks.before_work)();
        check()?;
        let payload = match materialize(&response, &mut || {
            (hooks.on_progress)();
            check()
        }) {
            Ok(payload) => payload,
            Err(error) => {
                // Located config failures are public evidence too. Fence and bound
                // them before the HTTP layer preserves the structured diagnostic.
                if let Some(diagnostic) = crate::rules::diagnostic(&error) {
                    crate::metrics::serialize_checked(
                        &serde_json::json!({"error": diagnostic}),
                        max_bytes,
                        check,
                    )?;
                    (hooks.before_finish)();
                    check()?;
                    response.finish(())?;
                    (hooks.after_finish)();
                    check()?;
                }
                return Err(error);
            }
        };
        let mut writer = Bounded {
            bytes: Vec::new(),
            check,
            on_write: hooks.on_write,
            max_bytes,
            failure: None,
        };
        let encoded = serde_json::to_writer(
            &mut writer,
            &Envelope {
                catching_up,
                payload: &payload,
            },
        );
        if let Some(error) = writer.failure.take() {
            return Err(error);
        }
        encoded.map_err(|_| anyhow::anyhow!("bounded_work: metrics serialization failed"))?;
        (writer.check)()?;
        (hooks.before_finish)();
        (writer.check)()?;
        let bytes = response.finish(writer.bytes)?;
        (hooks.after_finish)();
        (writer.check)()?;
        Ok((bytes, catching_up))
    }
}
