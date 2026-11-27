//! Ordered charge replay against fresh catalogs, retaining only completed source output.
use super::*;
use shared::{ChargeSite, SemanticChargeOutcome};

#[derive(Clone, Serialize)]
pub(crate) struct SourceChargePlan {
    pub(crate) prepared_work: usize,
    pub(crate) sites: Vec<ChargeSite>,
}

#[derive(Clone, Default)]
pub(crate) struct SourceFacts {
    pub(crate) plan: Option<Arc<SourceChargePlan>>,
    pub(crate) work: usize,
    pub(crate) sites: usize,
    pub(crate) endpoints: Vec<u32>,
    /// A projected source's semantic endpoint documents: those whose eligibility its Read B
    /// reads under its producer at the current pin (#934, #1310). Empty for a current source.
    pub(crate) eligibility: Vec<u32>,
}

/// Build-local owned state. All document borrows and request callbacks are made on the
/// stack when this state is used; neither the plan nor a completed core is self-referential.
pub(crate) struct ReferencePreparation {
    order: Vec<usize>,
    sorted: Vec<usize>,
    trees: Vec<Tree>,
    identities: Vec<Identity>,
    modules: Vec<Value>,
    document_map: BTreeMap<DocumentKey, usize>,
    partial_files: Vec<syntax_bounds::PartialFile>,
    selected: Vec<usize>,
    catalogs: Catalogs,
    pub(crate) limit: usize,
    pub(crate) rust_digest: [u8; 32],
    pub(crate) python_digest: [u8; 32],
    pub(crate) javascript_digest: [u8; 32],
    pub(crate) java_digest: [u8; 32],
    workers: usize,
    /// This build's syntax, for the next one (#1315), when the caller keeps it.
    pub(crate) kept: Option<KeptSyntax>,
}

/// What a build keeps of its syntax for the next one (#1315): each document's syntax-bounds
/// gap and the first half of its Rust catalog, valid for the same document inventory and
/// manifests, document by document while its stored version and parse-work flag hold.
pub(crate) struct KeptSyntax {
    identity: SyntaxIdentity,
    gaps: Vec<Option<syntax_bounds::SyntaxGap>>,
    rust: rust::kept::Prepared,
}

/// What kept syntax is valid for: by sorted document, its key, stored version and parse-work
/// flag, and the manifests.
#[derive(PartialEq, Eq)]
struct SyntaxIdentity {
    order: Vec<DocumentKey>,
    versions: Vec<String>,
    unparsed: Vec<bool>,
    manifests: [u8; 32],
}

impl SyntaxIdentity {
    /// `docs` in sorted order; `versions` in snapshot order, `sorted` mapping it.
    fn of(
        snapshot: &Snapshot,
        docs: &[&DocumentInput],
        sorted: &[usize],
        versions: Option<&[String]>,
    ) -> Result<Option<Self>> {
        let Some(versions) = versions else {
            return Ok(None);
        };
        ensure!(
            versions.len() == sorted.len(),
            "store_unavailable: kept syntax inventory"
        );
        let mut by_document = vec![String::new(); docs.len()];
        for (i, &position) in sorted.iter().enumerate() {
            by_document[position].clone_from(&versions[i]);
        }
        Ok(Some(Self {
            order: docs.iter().map(|doc| doc.key.clone()).collect(),
            versions: by_document,
            unparsed: docs
                .iter()
                .map(|doc| snapshot.unparsed.contains(&doc.key))
                .collect(),
            manifests: crate::architecture_core::dependencies::manifest_digest(&snapshot.manifests),
        }))
    }
}

impl KeptSyntax {
    /// `docs` and `gaps` in sorted order; `versions` in snapshot order, `sorted` mapping it.
    pub(super) fn new(
        snapshot: &Snapshot,
        docs: &[&DocumentInput],
        sorted: &[usize],
        versions: &[String],
        gaps: Vec<Option<syntax_bounds::SyntaxGap>>,
        rust: rust::kept::Prepared,
    ) -> Result<Self> {
        ensure!(
            gaps.len() == docs.len(),
            "store_unavailable: kept syntax inventory"
        );
        Ok(Self {
            identity: SyntaxIdentity::of(snapshot, docs, sorted, Some(versions))?
                .context("store_unavailable: kept syntax inventory")?,
            gaps,
            rust,
        })
    }
}

#[cfg(all(test, feature = "test-support"))]
impl ReferencePreparation {
    pub(super) fn catalogs_for_tests(&self) -> &Catalogs {
        &self.catalogs
    }
}

/// What an incremental build reads with kept syntax (#1315).
pub(crate) struct KeptRead<'a> {
    pub(crate) syntax: Option<&'a KeptSyntax>,
    /// Each snapshot document's stored version, in snapshot order.
    pub(crate) versions: &'a [String],
    /// The sources this build extracts again, which need their syntax.
    pub(crate) dirty: &'a BTreeSet<DocumentKey>,
}

#[derive(Default, Debug, Clone, Serialize)]
pub struct ReferenceReplayTrace {
    pub catalog_work: usize,
    pub executed_work: usize,
    pub logical_work: usize,
    pub file_references: Vec<DocumentKey>,
    pub batches: Vec<(Vec<DocumentKey>, usize)>,
    /// An incremental build's documents whose syntax it parsed (#1315): with a kept Rust
    /// catalog, the dirty sources and every document outside Rust.
    pub parsed: Vec<DocumentKey>,
}

pub(crate) struct IncrementalReferences {
    pub(crate) references: ReferenceBase,
    pub(crate) trace: ReferenceReplayTrace,
}

pub(crate) fn prepare_incremental(
    snapshot: &Snapshot,
    kept: Option<KeptRead<'_>>,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<ReferencePreparation> {
    prepare_with_workers(snapshot, crate::parallel::default_workers(), kept, check)
}

pub(super) fn prepare_with_workers(
    snapshot: &Snapshot,
    workers: usize,
    kept: Option<KeptRead<'_>>,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<ReferencePreparation> {
    let workers = workers.clamp(1, crate::parallel::MAX_WORKERS);
    let request = parallel_check::RequestCheck::new(check);
    let result = (|| {
        let check = &mut request.worker();
        let Admitted {
            docs,
            sorted,
            identities,
            modules,
            document_map,
            total_bytes,
        } = admit_snapshot(snapshot, check)?;
        let syntax = |doc: &DocumentInput| -> Result<(Tree, Option<syntax_bounds::SyntaxGap>)> {
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
        };
        // With kept syntax for this inventory and these manifests, a Rust document whose
        // version and parse-work flag are unchanged and whose source is clean is not parsed:
        // its gap and its part of the Rust catalog are kept (#1315).
        let keeping = kept.is_some();
        let (kept_syntax, versions, dirty) = match kept {
            Some(KeptRead {
                syntax,
                versions,
                dirty,
            }) => (syntax, Some(versions), Some(dirty)),
            None => (None, None, None),
        };
        let current = SyntaxIdentity::of(snapshot, &docs, &sorted, versions)?;
        let usable = kept_syntax.filter(|kept| {
            current.as_ref().is_some_and(|current| {
                kept.identity.order == current.order && kept.identity.manifests == current.manifests
            })
        });
        let mut parsed = vec![true; docs.len()];
        let mut changed = BTreeSet::new();
        if let (Some(kept), Some(current)) = (usable, &current) {
            for (i, doc) in docs.iter().enumerate() {
                let same = kept.identity.versions[i] == current.versions[i]
                    && kept.identity.unparsed[i] == current.unparsed[i];
                if doc.key.language == "rust" {
                    if !same {
                        changed.insert(i);
                    }
                    parsed[i] = !same || dirty.is_some_and(|dirty| dirty.contains(&doc.key));
                }
            }
        }
        let placeholder = syntax_bounds::unread_tree("");
        let limit = work::reference_limit(docs.len(), total_bytes);
        let selected = docs
            .iter()
            .enumerate()
            .filter_map(|(i, doc)| (!snapshot.evidence_gaps.contains_key(&doc.key)).then_some(i))
            .collect::<Vec<_>>();
        // A read whose syntax would not fit at once parses in lanes (#1321), keeping only the
        // trees `finish_incremental` reads: its dirty sources, and the Rust sources it parsed,
        // whose replay may read their syntax.
        let plan = match dirty {
            Some(_) => lanes(&docs, |i| usable.is_none() || parsed[i]),
            None => vec![CATALOGS.to_vec()],
        };
        let (trees, gaps, catalogs, prepared) = if plan.len() > 1 {
            let dirty = dirty.expect("lanes need the dirty sources");
            let is_dirty: Vec<bool> = docs.iter().map(|doc| dirty.contains(&doc.key)).collect();
            let read = ReadContext {
                snapshot,
                docs: &docs,
                trees: &[],
                document_map: &document_map,
                selected: &selected,
                sample_limit: MAX_SAMPLE_LIMIT,
                workers,
                request: &request,
            };
            let mut gaps = vec![None; docs.len()];
            let mut built = None;
            let mut trees = Vec::new();
            if let Some(kept) = usable {
                for (i, gap) in gaps.iter_mut().enumerate() {
                    if !parsed[i] {
                        gap.clone_from(&kept.gaps[i]);
                    }
                }
                let keep = |i: usize| is_dirty[i] || (docs[i].key.language == "rust" && parsed[i]);
                (built, trees) = catalogs_in_lanes(
                    read,
                    &plan,
                    limit,
                    RustSource::Kept {
                        prepared: &kept.rust,
                        changed: &changed,
                        parsed: &parsed,
                    },
                    &|i| {
                        if parsed[i] {
                            syntax(docs[i]).map(Some)
                        } else {
                            Ok(None)
                        }
                    },
                    &keep,
                    &mut gaps,
                    check,
                )?;
            }
            if built.is_none() {
                // No kept catalog, or one that cannot take this change: build in full.
                let keep = |i: usize| is_dirty[i] || docs[i].key.language == "rust";
                (built, trees) = catalogs_in_lanes(
                    read,
                    &lanes(&docs, |_| true),
                    limit,
                    RustSource::Fresh { keep: keeping },
                    &|i| syntax(docs[i]).map(Some),
                    &keep,
                    &mut gaps,
                    check,
                )?;
            }
            let (catalogs, prepared) = built.context("store_unavailable: fresh Rust catalog")?;
            (trees, gaps, catalogs, prepared)
        } else {
            let (mut trees, mut gaps): (Vec<_>, Vec<_>) = map_all(
                &(0..docs.len()).collect::<Vec<_>>(),
                workers,
                &request,
                |&i| match usable {
                    Some(kept) if !parsed[i] => Ok((placeholder.clone(), kept.gaps[i].clone())),
                    _ => syntax(docs[i]),
                },
            )?
            .into_iter()
            .unzip();
            let mut built = None;
            if let Some(kept) = usable {
                built = build_catalogs(
                    ReadContext {
                        snapshot,
                        docs: &docs,
                        trees: &trees,
                        document_map: &document_map,
                        selected: &selected,
                        sample_limit: MAX_SAMPLE_LIMIT,
                        workers,
                        request: &request,
                    },
                    limit,
                    RustSource::Kept {
                        prepared: &kept.rust,
                        changed: &changed,
                        parsed: &parsed,
                    },
                    check,
                )?;
            }
            let (catalogs, prepared) = match built {
                Some(built) => built,
                None => {
                    // No kept catalog, or one that cannot take this change: parse the rest and
                    // build in full.
                    let rest: Vec<usize> = (0..docs.len()).filter(|&i| !parsed[i]).collect();
                    let reparsed = map_all(&rest, workers, &request, |&i| syntax(docs[i]))?;
                    for (i, (tree, gap)) in rest.into_iter().zip(reparsed) {
                        trees[i] = tree;
                        gaps[i] = gap;
                        parsed[i] = true;
                    }
                    build_catalogs(
                        ReadContext {
                            snapshot,
                            docs: &docs,
                            trees: &trees,
                            document_map: &document_map,
                            selected: &selected,
                            sample_limit: MAX_SAMPLE_LIMIT,
                            workers,
                            request: &request,
                        },
                        limit,
                        RustSource::Fresh { keep: keeping },
                        check,
                    )?
                    .context("store_unavailable: fresh Rust catalog")?
                }
            };
            (trees, gaps, catalogs, prepared)
        };
        let partial_files = docs
            .iter()
            .zip(&gaps)
            .filter_map(|(doc, gap)| {
                snapshot
                    .evidence_gaps
                    .get(&doc.key)
                    .map(|&reason| {
                        syntax_bounds::SyntaxGap::module_only(reason, doc.file.text.len())
                    })
                    .as_ref()
                    .or(gap.as_ref())
                    .map(|gap| gap.file(&doc.key))
            })
            .collect();
        let kept = match (current, prepared) {
            (Some(identity), Some(rust)) => Some(KeptSyntax {
                identity,
                gaps: gaps.clone(),
                rust,
            }),
            _ => None,
        };
        let rust_digest = catalogs.rust.digest();
        let python_digest = catalogs.python.digest();
        let javascript_digest = javascript::digest(catalogs.javascript.as_ref());
        let java_digest = catalogs.java.digest();
        let mut order = vec![0; sorted.len()];
        for (original, &position) in sorted.iter().enumerate() {
            order[position] = original;
        }
        Ok(ReferencePreparation {
            order,
            sorted,
            trees,
            identities,
            modules,
            document_map,
            partial_files,
            selected,
            catalogs,
            limit,
            rust_digest,
            python_digest,
            javascript_digest,
            java_digest,
            workers,
            kept,
        })
    })();
    request.result(result)
}

pub(crate) fn finish_incremental(
    snapshot: &Snapshot,
    preparation: ReferencePreparation,
    base: &ReferenceBase,
    dirty: &BTreeSet<DocumentKey>,
    patched: &BTreeSet<DocumentKey>,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<IncrementalReferences> {
    let request = parallel_check::RequestCheck::new(check);
    let result = (|| {
        let check = &mut request.worker();
        let ReferencePreparation {
            order,
            sorted,
            trees,
            identities,
            modules,
            document_map,
            partial_files,
            selected,
            mut catalogs,
            limit: _,
            rust_digest,
            python_digest,
            javascript_digest,
            java_digest,
            workers,
            kept: _,
        } = preparation;
        ensure!(
            base.identities.len() == order.len(),
            "incremental source inventory"
        );
        let docs: Vec<_> = order.iter().map(|&i| &snapshot.documents[i]).collect();
        let read = ReadContext {
            snapshot,
            docs: &docs,
            trees: &trees,
            document_map: &document_map,
            selected: &selected,
            sample_limit: MAX_SAMPLE_LIMIT,
            workers,
            request: &request,
        };
        let (declared, producers) =
            declared_targets(&docs, &mut catalogs.rust_execution.work, check)?;
        let locate = |document: usize, sid: &str| {
            sorted
                .get(document)
                .copied()
                .filter(|&index| declared.contains(&(index, sid)))
        };
        let mut trace = ReferenceReplayTrace {
            catalog_work: catalogs.rust_execution.work.used,
            parsed: docs
                .iter()
                .enumerate()
                .filter(|&(i, _)| catalogs.parsed.as_ref().is_none_or(|parsed| parsed[i]))
                .map(|(_, doc)| doc.key.clone())
                .collect(),
            ..Default::default()
        };
        let mut executed = trace.catalog_work;
        let mut sources = vec![None; docs.len()];
        let mut external_packages = ExternalPackageTable::new(docs.len());
        let mut facts = vec![SourceFacts::default(); docs.len()];
        let mut site_count = 0;
        let mut target_documents = BTreeMap::new();
        for batch in selected.chunks(workers) {
            let remaining = catalogs.rust_execution.work.remaining();
            trace.batches.push((
                batch.iter().map(|&i| docs[i].key.clone()).collect(),
                remaining,
            ));
            let dirty_batch: Vec<_> = batch
                .iter()
                .copied()
                .filter(|&i| dirty.contains(&docs[i].key))
                .collect();
            // Windows are the original full-source windows. Every preparation in a window
            // sees the same logical full remainder, even when its neighbours are retained.
            let prepared = map_batch(&dirty_batch, workers, &request, |&source| {
                prepare_file(
                    source,
                    docs[source],
                    &trees[source],
                    remaining,
                    &mut request.worker(),
                )
            })?;
            let mut prepared = prepared.into_iter();
            for &source in batch {
                let before = catalogs.rust_execution.work.used;
                if dirty.contains(&docs[source].key) {
                    trace.file_references.push(docs[source].key.clone());
                    let completed = file_references(
                        read,
                        &docs,
                        &mut catalogs,
                        prepared.next().expect("prepared dirty source"),
                        &locate,
                        &mut site_count,
                        &mut target_documents,
                        check,
                    )?;
                    ensure!(completed.source == source, "incremental preparation order");
                    external_packages.complete(source, completed.packages);
                    sources[source] = Some(Arc::new(completed.found));
                    facts[source] = completed.facts;
                    executed = executed
                        .checked_add(catalogs.rust_execution.work.used - before)
                        .context("incremental executed work overflow")?;
                } else {
                    let retained = base.sources[source]
                        .as_ref()
                        .context("incremental missing source")?;
                    let old = &base.source_facts[source];
                    let plan = old
                        .plan
                        .as_ref()
                        .context("incremental missing charge plan")?;
                    ensure!(
                        plan.prepared_work <= remaining,
                        "incremental preparation does not fit"
                    );
                    let retained_locate = |document, sid: &str| {
                        declared.contains(&(document, sid)).then_some(document)
                    };
                    let (packages, sites) = replay_source_charges(
                        read,
                        &mut catalogs,
                        source,
                        plan,
                        &retained_locate,
                        check,
                    )?;
                    ensure!(
                        packages.keys().eq(retained.packages.iter()),
                        "incremental package membership changed"
                    );
                    site_count = site_count
                        .checked_add(sites)
                        .context("incremental site overflow")?;
                    ensure!(
                        site_count <= site_limit(),
                        "bounded_work: reference site count"
                    );
                    external_packages.complete(source, packages);
                    let mut rewritten = patched
                        .contains(&docs[source].key)
                        .then(|| patch_source_targets(retained, read, &mut target_documents))
                        .transpose()?;
                    let semantic = &docs[source].semantic;
                    if needs_rebase(rewritten.as_ref().unwrap_or(retained), semantic)? {
                        rebase_source(
                            rewritten.get_or_insert_with(|| (**retained).clone()),
                            semantic,
                        )?;
                    }
                    sources[source] = Some(rewritten.map_or_else(|| retained.clone(), Arc::new));
                    let work = catalogs.rust_execution.work.used - before;
                    facts[source] = SourceFacts {
                        work,
                        ..old.clone()
                    };
                    ensure!(sites == old.sites, "incremental accepted sites changed");
                    executed = executed
                        .checked_add(
                            work.checked_sub(plan.prepared_work)
                                .context("incremental preparation work")?,
                        )
                        .context("incremental executed work overflow")?;
                }
            }
        }
        external_packages.validate(&sources)?;
        trace.executed_work = executed;
        trace.logical_work = catalogs.rust_execution.work.used;
        drop_trees(trees, workers);
        Ok(IncrementalReferences {
            references: ReferenceBase {
                pin: snapshot.pin,
                partial_files,
                identities,
                modules,
                sources,
                external_packages,
                rust_catalog_digest: rust_digest,
                python_catalog_digest: python_digest,
                javascript_catalog_digest: javascript_digest,
                java_catalog_digest: java_digest,
                source_facts: facts,
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
                sample_limit: MAX_SAMPLE_LIMIT,
                work_used: catalogs.rust_execution.work.used,
            },
            trace,
        })
    })();
    request.result(result)
}

/// Execute the retained requests, not their previous totals. In particular a changed early
/// source can make a later clean source's Rust memo hits (and therefore its q_s) different.
pub(super) fn replay_source_charges(
    read: ReadContext<'_, '_>,
    catalogs: &mut Catalogs,
    source: usize,
    plan: &SourceChargePlan,
    locate: &impl Fn(usize, &str) -> Option<usize>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<(SourcePackageDescriptors, usize)> {
    merge_work(&mut catalogs.rust_execution.work, plan.prepared_work, check)?;
    let mut packages = SourcePackageDescriptors::new();
    let mut sites = 0;
    for retained in &plan.sites {
        check()?;
        // Replay's locate accepts normalized indices; full construction's accepts snapshot
        // indices. Both use the complete current declaration eligibility index.
        let outcome = match retained.semantic.as_ref().map(|semantic| &semantic.outcome) {
            Some(SemanticChargeOutcome::Internal { document, sid }) => {
                Some(SemanticOutcome::Internal(*document as usize, sid.clone()))
            }
            _ => None,
        };
        let charged = shared::resolve_charge_site(
            read,
            catalogs,
            source,
            retained,
            &mut packages,
            locate,
            check,
        )?;
        let Some((resolution, semantic)) = charged.effective(source) else {
            continue;
        };
        sites += 1;
        internal_target_charge(
            semantic,
            outcome.as_ref(),
            &resolution,
            locate,
            &mut catalogs.rust_execution.work,
            check,
        )?;
        for _target in resolution.targets {
            owner_step(&mut catalogs.rust_execution.work, check)?;
        }
    }
    Ok((packages, sites))
}

pub(super) fn patch_source_targets<'a>(
    retained: &SourceReferences,
    read: ReadContext<'a, '_>,
    targets: &mut BTreeMap<usize, TargetDocument<'a>>,
) -> Result<SourceReferences> {
    let mut found = retained.clone();
    let sampled = found
        .edges
        .values_mut()
        .flat_map(|edge| edge.calls.iter_mut().chain(&mut edge.others));
    for site in sampled.chain(found.unresolved.iter_mut().map(|(site, _)| site)) {
        if let Some(target) = &mut site.target {
            let document = *read
                .document_map
                .get(&target.document)
                .context("incremental patch document")?;
            let entry = targets
                .entry(document)
                .or_insert_with(|| TargetDocument::new(read.docs[document]));
            let declaration = entry
                .by_sid
                .get(target.syntax_id.as_str())
                .context("incremental patch declaration")?;
            target.lines = lines_of(
                &entry.newlines,
                declaration.range.start,
                declaration.range.end,
            );
            target.display_key = display_key(declaration);
        }
    }
    Ok(found)
}

/// Whether a reused or patched source's revision-scoped semantic metadata was materialized
/// for another reading of its document than this core's (#1310). When not, it stays shared.
pub(super) fn needs_rebase(found: &SourceReferences, semantic: &DocumentSemantic) -> Result<bool> {
    let reading = match (&found.reading, &semantic.reading) {
        (None, _) => false,
        (Some(old), Some(new)) => old != new,
        (Some(_), None) => anyhow::bail!("incremental semantic reading without a payload"),
    };
    Ok(reading || found.projection != semantic.projection)
}

/// Materializes a reused or patched source's revision-scoped semantic metadata for this core's
/// reading of its document (#1310): rejoined proofs' provenance IDs minted at this reading's
/// native revision (Decision 0021 D2), each site's basis freshness, and the projection with
/// this core's current pin. Everything else about the source is what Read B would read again,
/// because its semantic fingerprint and its endpoints' are unchanged.
pub(super) fn rebase_source(
    found: &mut SourceReferences,
    semantic: &DocumentSemantic,
) -> Result<()> {
    found.projection.clone_from(&semantic.projection);
    if found.reading.is_none() {
        return Ok(());
    }
    found.reading.clone_from(&semantic.reading);
    let reading = semantic
        .reading
        .as_ref()
        .context("incremental semantic reading without a payload")?;
    // One rebuilt allocation per shared basis allocation, as Read B shares them (#1247).
    let mut shared: std::collections::HashMap<
        *const BTreeMap<String, Value>,
        Arc<BTreeMap<String, Value>>,
    > = std::collections::HashMap::new();
    let projection = semantic.projection.as_ref();
    let sampled = found
        .edges
        .values_mut()
        .flat_map(|edge| edge.calls.iter_mut().chain(&mut edge.others));
    for site in sampled.chain(found.unresolved.iter_mut().map(|(site, _)| site)) {
        if let Some(proof) = site.semantic_metadata.derived_proof()? {
            site.provenance_id = Some(proof.provenance_id(&reading.native));
        }
        let Some((basis, fields)) = site.semantic_metadata.basis_fields() else {
            continue;
        };
        let key = Arc::as_ptr(fields);
        let rebuilt = match shared.get(&key) {
            Some(rebuilt) => rebuilt.clone(),
            None => {
                let freshness = reading
                    .freshness
                    .get(&basis)
                    .context("incremental semantic freshness missing")?;
                let mut rebuilt = (**fields).clone();
                crate::architecture_inputs::set_freshness_fields(
                    &mut rebuilt,
                    freshness,
                    projection,
                )?;
                let rebuilt = Arc::new(rebuilt);
                shared.insert(key, rebuilt.clone());
                rebuilt
            }
        };
        site.semantic_metadata.replace_shared(rebuilt);
    }
    Ok(())
}

impl SourceFacts {
    pub(super) fn canonical(&self) -> Value {
        let Self {
            plan,
            work,
            sites,
            endpoints,
            eligibility,
        } = self;
        json!({ "plan": plan.as_deref(), "work": work, "sites": sites, "endpoints": endpoints, "eligibility": eligibility })
    }
}

impl ReferenceBase {
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn source_facts_mut_for_tests(&mut self) -> &mut [SourceFacts] {
        &mut self.source_facts
    }

    pub(crate) fn full_trace(&self, limit: usize, workers: usize) -> ReferenceReplayTrace {
        let source_work: usize = self.source_facts.iter().map(|source| source.work).sum();
        let catalog_work = self.work_used - source_work;
        let selected: Vec<_> = self
            .source_facts
            .iter()
            .enumerate()
            .filter_map(|(i, source)| source.plan.is_some().then_some(i))
            .collect();
        let key = |i: usize| {
            let identity = &self.identities[i];
            DocumentKey {
                source_set_id: identity.source_set_id.clone(),
                language: identity.language.clone(),
                path: identity.path.clone(),
            }
        };
        let mut remaining = limit - catalog_work;
        let batches = selected
            .chunks(workers.clamp(1, crate::parallel::MAX_WORKERS))
            .map(|batch| {
                let before = remaining;
                for &i in batch {
                    remaining -= self.source_facts[i].work;
                }
                (batch.iter().map(|&i| key(i)).collect(), before)
            })
            .collect();
        ReferenceReplayTrace {
            catalog_work,
            executed_work: self.work_used,
            logical_work: self.work_used,
            file_references: selected.into_iter().map(key).collect(),
            batches,
            parsed: (0..self.identities.len()).map(key).collect(),
        }
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn source_shared(&self, source: usize, other: &Self, before: usize) -> bool {
        self.sources[source]
            .as_ref()
            .zip(other.sources[before].as_ref())
            .is_some_and(|(a, b)| Arc::ptr_eq(a, b))
    }

    pub(crate) fn rust_catalog_digest(&self) -> [u8; 32] {
        self.rust_catalog_digest
    }

    pub(crate) fn python_catalog_digest(&self) -> [u8; 32] {
        self.python_catalog_digest
    }

    pub(crate) fn javascript_catalog_digest(&self) -> [u8; 32] {
        self.javascript_catalog_digest
    }

    pub(crate) fn java_catalog_digest(&self) -> [u8; 32] {
        self.java_catalog_digest
    }

    /// Every admitted document has a slot, including absent and scanned zero-site sources.
    pub(crate) fn source_facts(&self) -> &[SourceFacts] {
        &self.source_facts
    }
}

pub(super) fn normalize_document(read: ReadContext<'_, '_>, document: usize) -> Result<u32> {
    // An out-of-snapshot outcome remains unusable, rather than aliasing a real document.
    let sorted = read
        .snapshot
        .documents
        .get(document)
        .and_then(|doc| read.document_map.get(&doc.key))
        .copied()
        .unwrap_or(read.docs.len());
    Ok(u32::try_from(sorted)?)
}

impl SourceChargePlan {
    pub(super) fn push(&mut self, read: ReadContext<'_, '_>, mut site: ChargeSite) -> Result<()> {
        if let Some(shared::SemanticCharge {
            outcome: SemanticChargeOutcome::Internal { document, sid: _ },
            historical: _,
        }) = &mut site.semantic
        {
            *document = normalize_document(read, *document as usize)?;
        }
        self.sites.push(site);
        Ok(())
    }
}

pub(super) fn resolution_endpoints(endpoints: &mut BTreeSet<usize>, charged: &shared::ChargedSite) {
    for resolution in [&charged.structural, &charged.semantic]
        .into_iter()
        .flatten()
    {
        endpoints.extend(resolution.proof.iter().copied());
        endpoints.extend(resolution.targets.iter().filter_map(|target| match target {
            Target::Native(document) => Some(*document),
            Target::External(_) => None,
        }));
    }
}

pub(super) fn semantic_endpoints(
    read: ReadContext<'_, '_>,
    source: usize,
) -> Result<BTreeSet<usize>> {
    let mut endpoints = BTreeSet::new();
    let semantic = &read.docs[source].semantic;
    for document in &semantic.endpoint_documents {
        let document = normalize_document(read, *document)? as usize;
        if document < read.docs.len() {
            endpoints.insert(document);
        }
    }
    for anchor in &semantic.anchors {
        if let SemanticOutcome::Internal(document, _) = &anchor.outcome {
            let document = normalize_document(read, *document)? as usize;
            if document < read.docs.len() {
                endpoints.insert(document);
            }
        }
        metadata_endpoints(
            read,
            &read.docs[source].key.source_set_id,
            &anchor.metadata,
            &mut endpoints,
        );
    }
    for relationship in &semantic.relationships {
        endpoints.insert(source);
        if let Some((document, _)) = &relationship.target {
            let document = normalize_document(read, *document)? as usize;
            if document < read.docs.len() {
                endpoints.insert(document);
            }
        }
        metadata_endpoints(
            read,
            &read.docs[source].key.source_set_id,
            &relationship.metadata,
            &mut endpoints,
        );
    }
    Ok(endpoints)
}

fn metadata_endpoints(
    read: ReadContext<'_, '_>,
    source_set_id: &str,
    metadata: &SemanticMetadata,
    endpoints: &mut BTreeSet<usize>,
) {
    // Historical targets retain endpoints even when eligibility turns the outcome into
    // Boundary. External metadata, including coincident checkout paths, is not an endpoint.
    if let Some(targets) = metadata.get("historicalTargets")
        && let Some(targets) = targets.as_array()
    {
        for target in targets {
            if target["kind"] != "internal" {
                continue;
            }
            if let (Some(language), Some(path)) =
                (target["language"].as_str(), target["path"].as_str())
            {
                let key = DocumentKey {
                    source_set_id: source_set_id.into(),
                    language: language.into(),
                    path: path.into(),
                };
                if let Some(document) = read.document_map.get(&key) {
                    endpoints.insert(*document);
                }
            }
        }
    }
}
