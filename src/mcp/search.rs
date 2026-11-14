//! Per-call search inputs for the shared syntax-evidence wire projection.

pub use crate::search_model::SearchIndexStatus;

use super::readers::ReadCx;
use super::{
    EvidenceToolRead, app_files, projection, search, tools, validated_expected_basis, wire,
};

/// Ordered store hits and arm membership from the same admitted snapshot.
pub struct SearchResult {
    pub symbols: crate::store::mcp_read::McpFindWithLines,
    pub matched_by: Vec<Vec<String>>,
    pub search_index: SearchIndexStatus,
    pub role: crate::search_rank::SearchRole,
}

/// Search has a separate public default from the unchanged find-symbols tool.
pub const DEFAULT_LIMIT: usize = 10;

/// `trellis_search_symbols`: the ranked search inside one admitted snapshot, from the
/// resident matrix head, a cold head (lexical only) or a standalone vector stream.
pub(super) fn search_read<'a>(
    cx: ReadCx<'a>,
    args: &'a serde_json::Map<String, serde_json::Value>,
) -> Option<EvidenceToolRead<'a>> {
    let ReadCx {
        id,
        root,
        modern,
        deadline,
        cancel,
        ..
    } = cx;
    let query = args["query"].as_str().expect("validated search query");
    let limit = args.get("limit").map_or(search::DEFAULT_LIMIT, |value| {
        wire::uint(value).expect("validated search limit") as usize
    });
    let role = args
        .get("role")
        .map_or(crate::search_rank::SearchRole::Any, |value| {
            serde_json::from_value(value.clone()).expect("validated search role")
        });
    let compact = args.get("compact").and_then(|v| v.as_bool()) == Some(true);
    let expected = validated_expected_basis(args);
    let app = args.get("app").and_then(|value| value.as_str());
    Some(Box::new(move |runtime, response, sampled, prepared| {
        let prepared = prepared.expect("search query prepared before admission");
        let mut check = || {
            if cancel.load(std::sync::atomic::Ordering::Acquire)
                || runtime.mcp_clock_now() >= deadline
            {
                anyhow::bail!("deadline_exceeded: MCP search deadline");
            }
            Ok(())
        };
        let pin = response.status()?.revision;
        anyhow::ensure!(
            expected.is_none_or(|expected| expected == pin),
            "revision conflict: expected basis differs from admitted head"
        );
        // #851: the ranked hits in the app's files, before the limit.
        let in_app = match app_files(runtime, response, id, app, &mut check)? {
            Ok(Some(files)) => {
                let paths = response.mcp_vid_paths(pin)?;
                Some((files, paths))
            }
            Ok(None) => None,
            Err(envelope) => return Ok(envelope),
        };
        let policy = response.mcp_search_policy(query, pin, role, &mut check)?;
        let service = runtime.search_service();
        let head = runtime.browser_scheduler()?.search_head_slot(false);
        if service.is_some_and(|service| service.model_configured())
            && let Some(head) = &head
        {
            head.fault()?;
        }
        let warm = match (&head, service, prepared.model_id()) {
            (Some(head), Some(service), Some(model)) => {
                head.declarations(pin, model, service.cache_epoch())?
            }
            _ => None,
        };
        let resident = if let Some(declarations) = warm {
            let lexical =
                response.mcp_search_lexical(query, pin, declarations, &policy, &mut check)?;
            let head = head.as_ref().expect("published head selected");
            let service = service.expect("published head has service");
            let mut vector_check = || {
                check()?;
                if let Some(budget) = prepared.budget() {
                    budget.check()?;
                }
                Ok(())
            };
            let ranked = head.rank(
                pin,
                prepared.model_id().expect("published head has model"),
                service.cache_epoch(),
                &lexical,
                prepared.vector.as_deref(),
                &policy,
                &mut vector_check,
            );
            match ranked {
                Ok(Some(ranked)) => Some((declarations, Ok(ranked), lexical)),
                Ok(None) => None,
                Err(error) => Some((declarations, Err(error), lexical)),
            }
        } else {
            None
        };
        let resident_used = resident.is_some();
        let mut head_pending = None;
        let (declarations, ranked, lexical) = match resident {
            Some(resident) => resident,
            None if head.is_some() => {
                // Cold, capped, or concurrently replaced matrices remain
                // lexical-only at this admitted pin. Never stream vectors
                // around the resident-memory policy or wait for the owner.
                // While the owner is (re)building this head, count the
                // cache's current vectors instead (#777).
                let declarations = response.mcp_search_head_count(pin, &mut check)?;
                let pending = runtime.browser_scheduler()?.search_fill_building(pin)
                    || head
                        .as_ref()
                        .is_some_and(|head| head.pending_for(pin, declarations as u64));
                head_pending = Some(pending);
                let lexical =
                    response.mcp_search_lexical(query, pin, declarations, &policy, &mut check)?;
                let ranked = crate::search_rank::fuse_lexical(&lexical, &policy, &mut check)
                    .and_then(|mut ranked| {
                        if pending
                            && prepared.counts_pending()
                            && let Some(service) = service
                        {
                            // Optional work: the hash scan and count stop
                            // at the vector cutoff as well as the request
                            // deadline, keeping the lexical reserve.
                            let mut optional = || {
                                check()?;
                                if let Some(budget) = prepared.budget() {
                                    budget.check()?;
                                }
                                Ok(())
                            };
                            optional()?;
                            #[cfg(feature = "test-support")]
                            runtime.count_search_hash_scan_for_tests();
                            let hashes =
                                response.mcp_search_head_card_hashes(pin, &mut optional)?;
                            ranked.embedded = service.cached_embedded(
                                &hashes,
                                prepared.budget(),
                                &mut optional,
                            )?;
                        }
                        Ok(ranked)
                    });
                (declarations, ranked, lexical)
            }
            None => {
                // Standalone registries have no matrix owner.
                let candidates =
                    response.mcp_search_candidates(query, Some(pin), &policy, &mut check)?;
                let mut vector_check = || {
                    check()?;
                    if let Some(budget) = prepared.budget() {
                        budget.check()?;
                    }
                    Ok(())
                };
                let rank = |snapshot: Option<&crate::search_vectors::VectorSnapshot<'_>>| {
                    crate::search_rank::rank_snapshot(
                        &candidates.cards,
                        &candidates.lexical,
                        snapshot,
                        prepared.model_id(),
                        prepared.vector.as_deref(),
                        &policy,
                        &mut vector_check,
                    )
                };
                let ranked = match service {
                    Some(service) if prepared.model_id().is_some() => {
                        service.with_vector_snapshot_budget(prepared.budget(), rank)
                    }
                    _ => crate::search_rank::rank_lexical(
                        &candidates.cards,
                        &candidates.lexical,
                        &policy,
                        &mut check,
                    ),
                };
                (candidates.cards.len(), ranked, candidates.lexical)
            }
        };
        let (mut ranked, budget_exceeded) = match ranked {
            Ok(ranked) => (ranked, false),
            Err(error)
                if matches!(
                    error.downcast_ref::<crate::search_embed::EmbedError>(),
                    Some(crate::search_embed::EmbedError::BudgetExceeded)
                ) =>
            {
                (
                    crate::search_rank::fuse_lexical(&lexical, &policy, &mut check)?,
                    true,
                )
            }
            Err(error) if crate::store::cli_read_retryable_contention(&error) => {
                if prepared
                    .budget()
                    .is_some_and(|budget| budget.check().is_err())
                {
                    check()?;
                    (
                        crate::search_rank::fuse_lexical(&lexical, &policy, &mut check)?,
                        true,
                    )
                } else {
                    return Err(error);
                }
            }
            Err(error) if error.is::<crate::search_vectors::StoreUnavailable>() => {
                return Ok(tools::failure(id, "store_unavailable"));
            }
            Err(error) => return Err(error),
        };
        // A matrix head uses vectors only through its admitted
        // matrix; a standalone stream uses them with a query vector.
        let vector_used = if head.is_some() {
            resident_used
        } else {
            prepared.model_id().is_some()
        };
        // Used vectors keep the exact running-pin signal; only a
        // lexical-only head answer uses the broader pending signal.
        let pending = match head_pending {
            Some(pending) => pending,
            None => runtime.browser_scheduler()?.search_fill_building(pin),
        };
        let mut search_index =
            prepared.status(declarations as u64, ranked.embedded, vector_used, pending);
        if budget_exceeded {
            search_index.state = crate::search_model::SearchIndexState::Unavailable(
                crate::search_model::SearchUnavailableReason::VectorBudgetExceeded,
            );
            search_index.model_id = None;
            search_index.model_name = None;
            search_index.embedded = 0;
        }
        if let Some((files, paths)) = &in_app {
            ranked.hits.retain(|hit| {
                paths
                    .get(&hit.key.version_id)
                    .is_some_and(|path| files.contains(path))
            });
        }
        let has_more = ranked.hits.len() > limit;
        let prefix = &ranked.hits[..ranked.hits.len().min(limit)];
        let keys: Vec<_> = prefix.iter().map(|hit| hit.key.clone()).collect();
        let matched_by = prefix
            .iter()
            .map(|hit| {
                let mut arms = Vec::new();
                if hit.lexical_rank.is_some() {
                    arms.push("lexical".to_owned());
                }
                if hit.vector_rank.is_some() {
                    arms.push("vector".to_owned());
                }
                arms
            })
            .collect();
        let mut symbols = response.mcp_search_hydrate(&keys, pin, &mut check)?;
        symbols.has_more = has_more;
        let tiers = response.mcp_language_tiers()?;
        projection::search(
            id,
            search::SearchResult {
                symbols,
                matched_by,
                search_index,
                role,
            },
            &tiers,
            root,
            sampled,
            modern,
            compact,
        )
        .map_err(|error| anyhow::anyhow!(error))
        .map(|envelope| envelope.unwrap_or_else(|| tools::failure(id, "range_too_large")))
    }))
}
