//! One reader per evidence tool (#1053): what each tool reads and projects inside the
//! snapshot `evidence_read` admitted. `run_call` picks the reader; the arguments were
//! validated by `tools::prepare` before any reader is built.

use super::{
    ARCHITECTURE_ANSWER_TIME, CALL_DEFAULT_LIMIT, EvidenceToolRead, app_files, architecture,
    call_graph_failure, lookup_root, producer_failure, projection, show, tools,
    validated_expected_basis, wire,
};

/// What every reader takes from its call: all references or `Copy`.
#[derive(Clone, Copy)]
pub(super) struct ReadCx<'a> {
    pub id: &'a serde_json::Value,
    /// The answering root.
    pub root: &'a std::path::Path,
    pub modern: bool,
    pub deadline: std::time::Instant,
    /// When the coordinator accepted the call: `deadline - call_deadline`.
    pub accepted: std::time::Instant,
    pub cancel: &'a std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The target `trellis_show` checked in its admitted read (#818).
    pub show_target: &'a std::sync::Mutex<
        Option<(
            crate::daemon::view_request::ViewTarget,
            crate::model::IndexPin,
        )>,
    >,
}

/// The architecture tools and the architecture prompt: one store operation per name.
pub(super) fn architecture_read<'a>(
    cx: ReadCx<'a>,
    name: &'a str,
    args: &'a serde_json::Map<String, serde_json::Value>,
) -> Option<EvidenceToolRead<'a>> {
    let ReadCx {
        id,
        deadline,
        cancel,
        ..
    } = cx;
    let kind = architecture::operation(name);
    let arguments = architecture::store_arguments(name, &serde_json::Value::Object(args.clone()));
    // check/explain sample rule evidence per instance (#448). Explain's
    // `limit` sets the sample; check's counts instances per page (#725).
    let evidence_limit = match kind {
        "check" => Some(architecture::CHECK_EVIDENCE_SAMPLE),
        "explain" => Some(args.get("limit").map_or(5, |value| {
            wire::uint(value).expect("validated evidence limit") as usize
        })),
        _ => None,
    };
    Some(Box::new(move |runtime, response, _sampled, _query| {
        let mut check = || {
            if cancel.load(std::sync::atomic::Ordering::Acquire)
                || runtime.mcp_clock_now() >= deadline
            {
                anyhow::bail!("deadline_exceeded: architecture projection");
            }
            Ok(())
        };
        match runtime.architecture_tool(response, kind, &arguments, &mut check) {
            Ok(mut payload) => {
                let semantic = architecture::uses_semantic_evidence(&payload);
                if let Some(limit) = evidence_limit {
                    architecture::sample_evidence(&mut payload, limit);
                }
                // #769: check points each sampled instance at explain.
                if kind == "check" {
                    architecture::next_calls(&mut payload);
                }
                Ok(architecture::envelope_with(id, payload, semantic))
            }
            // Time is left for the owner: wait for it outside this read.
            Err(error)
                if error
                    .downcast_ref::<crate::http::architecture::ArchitectureBuilding>()
                    .is_some()
                    && deadline.saturating_duration_since(runtime.mcp_clock_now())
                        > ARCHITECTURE_ANSWER_TIME =>
            {
                Err(error)
            }
            Err(error) if crate::store::cli_read_retryable_contention(&error) => Err(error),
            Err(error) => {
                let mut failure = architecture::failure(id, &error);
                // #837: references read rules only to group by app. A bad
                // rules file is `invalid_rules` there, located by check.
                // #851: so do endpoints, whose failure schema is references'.
                if matches!(kind, "references" | "endpoints")
                    && let Some(error) = failure["error"].as_object_mut()
                {
                    error.remove("diagnostic");
                }
                Ok(failure)
            }
        }
    }))
}

/// `trellis_find_symbols`: a query or a stable lookup key, optionally within an app's files.
pub(super) fn find_read<'a>(
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
    let query = args.get("query").and_then(|value| value.as_str());
    let lookup_key = args.get("lookupKey").and_then(|value| value.as_str());
    let limit = args
        .get("limit")
        .map_or(20, |v| wire::uint(v).expect("validated limit") as usize);
    let compact = args.get("compact").and_then(|v| v.as_bool()) == Some(true);
    let expected = validated_expected_basis(args);
    let app = args.get("app").and_then(|value| value.as_str());
    Some(Box::new(move |runtime, response, sampled, _query| {
        let mut check = || {
            if cancel.load(std::sync::atomic::Ordering::Acquire)
                || runtime.mcp_clock_now() >= deadline
            {
                anyhow::bail!("deadline_exceeded: MCP find deadline");
            }
            Ok(())
        };
        // #851: only declarations in the app's files.
        let files = match app_files(runtime, response, id, app, &mut check)? {
            Ok(files) => files,
            Err(envelope) => return Ok(envelope),
        };
        let found = if let Some(key) = lookup_key {
            let mut found = response.mcp_resolve_lookup_with_lines(key, expected)?;
            if let Some(files) = &files {
                found
                    .symbols
                    .retain(|symbol| files.contains(&symbol.symbol.declaration.document.path));
            }
            found
        } else {
            response.mcp_find_with_lines_in(
                query.expect("validated query"),
                limit,
                expected,
                files.as_deref(),
            )?
        };
        let tiers = response.mcp_language_tiers()?;
        let projected = if lookup_key.is_some() {
            projection::find_lookup(id, found, &tiers, root, sampled, modern, compact)
        } else {
            projection::find(id, found, &tiers, root, sampled, modern, compact)
        };
        Ok(projected
            .map_err(|error| anyhow::anyhow!(error))?
            .unwrap_or_else(|| tools::failure(id, "range_too_large")))
    }))
}

/// `trellis_declaration`: one declaration by symbol ID or stable lookup key.
pub(super) fn declaration_read<'a>(
    cx: ReadCx<'a>,
    args: &'a serde_json::Map<String, serde_json::Value>,
) -> Option<EvidenceToolRead<'a>> {
    let ReadCx {
        id, root, modern, ..
    } = cx;
    let symbol_id = args.get("symbolId").and_then(|value| value.as_str());
    let lookup_key = args.get("lookupKey").and_then(|value| value.as_str());
    let expected = validated_expected_basis(args);
    Some(Box::new(move |_, response, sampled, _query| {
        let resolved;
        let symbol_id = if let Some(key) = lookup_key {
            resolved = match lookup_root(
                response,
                id,
                "trellis_declaration",
                key,
                expected,
                root,
                sampled,
                modern,
            )? {
                Ok(sid) => sid,
                Err(envelope) => return Ok(envelope),
            };
            resolved.as_str()
        } else {
            symbol_id.expect("validated symbol ID")
        };
        Ok(
            match response.mcp_declaration_with_lines(symbol_id, expected)? {
                Some(found) => projection::declaration(
                    id,
                    found,
                    &response.mcp_language_tiers()?,
                    root,
                    sampled,
                    modern,
                )
                .unwrap_or_else(|| tools::failure(id, "range_too_large")),
                None => tools::failure(id, "not_found"),
            },
        )
    }))
}

/// `trellis_outgoing_calls`: a declaration's calls with their semantic evidence.
pub(super) fn outgoing_read<'a>(
    cx: ReadCx<'a>,
    args: &'a serde_json::Map<String, serde_json::Value>,
) -> Option<EvidenceToolRead<'a>> {
    let ReadCx {
        id,
        root,
        modern,
        deadline,
        accepted,
        ..
    } = cx;
    let symbol_id = args.get("symbolId").and_then(|value| value.as_str());
    let lookup_key = args.get("lookupKey").and_then(|value| value.as_str());
    let limit = args.get("limit").map_or(CALL_DEFAULT_LIMIT, |v| {
        wire::uint(v).expect("validated limit") as usize
    });
    let compact = args.get("compact").and_then(|v| v.as_bool()) == Some(true);
    // #665: compact leaves external calls out unless `includeExternal`.
    let include_external = args
        .get("includeExternal")
        .and_then(|v| v.as_bool())
        .unwrap_or(!compact);
    let expected = validated_expected_basis(args);
    let producer = args.get("semanticProducerId").and_then(|v| v.as_str());
    Some(Box::new(move |runtime, response, sampled, _query| {
        let resolved;
        let symbol_id = if let Some(key) = lookup_key {
            resolved = match lookup_root(
                response,
                id,
                "trellis_outgoing_calls",
                key,
                expected,
                root,
                sampled,
                modern,
            )? {
                Ok(sid) => sid,
                Err(envelope) => return Ok(envelope),
            };
            resolved.as_str()
        } else {
            symbol_id.expect("validated symbol ID")
        };
        let found = match response.mcp_outgoing_with_lines_excluding(
            symbol_id,
            limit,
            (!include_external).then_some(producer),
            expected,
            &|| runtime.mcp_hint_clock_now(accepted),
            deadline,
        ) {
            Err(error) if error.is::<crate::store::ProducerNotPublished>() => {
                return Ok(producer_failure(id));
            }
            found => found?,
        };
        let Some(found) = found else {
            return Ok(tools::failure(id, "not_found"));
        };
        let semantic = match response.mcp_call_semantics(
            producer,
            &found.owner.selected.declaration.document,
            found.calls.iter().map(|call| &call.selected.call),
        ) {
            Err(error) if error.is::<crate::store::ProducerNotPublished>() => {
                return Ok(producer_failure(id));
            }
            semantic => semantic?,
        };
        Ok(projection::outgoing(
            id,
            found,
            &semantic,
            &response.mcp_language_tiers()?,
            compact,
            root,
            sampled,
            modern,
        )
        .map_err(|error| anyhow::anyhow!(error))?
        .unwrap_or_else(|| tools::failure(id, "range_too_large")))
    }))
}

/// `trellis_incoming_calls`: a declaration's callers with their semantic evidence.
pub(super) fn incoming_read<'a>(
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
    let symbol_id = args.get("symbolId").and_then(|value| value.as_str());
    let lookup_key = args.get("lookupKey").and_then(|value| value.as_str());
    let limit = args.get("limit").map_or(CALL_DEFAULT_LIMIT, |v| {
        wire::uint(v).expect("validated limit") as usize
    });
    let compact = args.get("compact").and_then(|v| v.as_bool()) == Some(true);
    let expected = validated_expected_basis(args);
    let producer = args.get("semanticProducerId").and_then(|v| v.as_str());
    Some(Box::new(move |runtime, response, sampled, _query| {
        let resolved;
        let symbol_id = if let Some(key) = lookup_key {
            resolved = match lookup_root(
                response,
                id,
                "trellis_incoming_calls",
                key,
                expected,
                root,
                sampled,
                modern,
            )? {
                Ok(sid) => sid,
                Err(envelope) => return Ok(envelope),
            };
            resolved.as_str()
        } else {
            symbol_id.expect("validated symbol ID")
        };
        // #724: callers proven bound elsewhere are left out while the
        // ranked callers are scanned; the deadline bounds that scan.
        let mut check = || {
            if cancel.load(std::sync::atomic::Ordering::Acquire)
                || runtime.mcp_clock_now() >= deadline
            {
                anyhow::bail!("deadline_exceeded: MCP incoming calls deadline");
            }
            Ok(())
        };
        let found = match response.mcp_incoming_excluding_bound_elsewhere(
            symbol_id, limit, expected, producer, &mut check,
        ) {
            Err(error) if error.is::<crate::store::ProducerNotPublished>() => {
                return Ok(producer_failure(id));
            }
            found => found?,
        };
        let Some(found) = found else {
            return Ok(tools::failure(id, "not_found"));
        };
        let semantic = match response.mcp_call_semantics(
            producer,
            &found.selected.selected.declaration.document,
            found.calls.iter().map(|call| &call.call),
        ) {
            Err(error) if error.is::<crate::store::ProducerNotPublished>() => {
                return Ok(producer_failure(id));
            }
            semantic => semantic?,
        };
        Ok(projection::incoming(
            id,
            found,
            &semantic,
            &response.mcp_language_tiers()?,
            compact,
            root,
            sampled,
            modern,
        )
        .map_err(|error| anyhow::anyhow!(error))?
        .unwrap_or_else(|| tools::failure(id, "range_too_large")))
    }))
}

/// `trellis_call_graph`: the rooted call graph, optionally within an app's files.
pub(super) fn call_graph_read<'a>(
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
    let query = tools::call_graph_query(&serde_json::Value::Object(args.clone()));
    let compact = args.get("compact").and_then(|v| v.as_bool()) == Some(true);
    let lookup_key = args.get("lookupKey").and_then(|value| value.as_str());
    let expected = validated_expected_basis(args);
    let app = args.get("app").and_then(|value| value.as_str());
    Some(Box::new(move |runtime, response, sampled, _query| {
        let mut query = query.clone();
        // #851: calls into declarations outside the app's files are filtered.
        let mut app_check = || {
            if cancel.load(std::sync::atomic::Ordering::Acquire)
                || runtime.mcp_clock_now() >= deadline
            {
                anyhow::bail!("deadline_exceeded: MCP call graph deadline");
            }
            Ok(())
        };
        query.app_paths = match app_files(runtime, response, id, app, &mut app_check)? {
            Ok(files) => files,
            Err(envelope) => return Ok(envelope),
        };
        if let Some(key) = lookup_key {
            query.root_syntax_id = match lookup_root(
                response,
                id,
                "trellis_call_graph",
                key,
                expected,
                root,
                sampled,
                modern,
            )? {
                Ok(sid) => sid,
                Err(envelope) => return Ok(envelope),
            };
        }
        let mut check = || {
            if cancel.load(std::sync::atomic::Ordering::Acquire)
                || runtime.mcp_clock_now() >= deadline
            {
                anyhow::bail!("deadline_exceeded: MCP call graph deadline");
            }
            Ok(())
        };
        let mut answer = response.graph_query(&query, expected, &mut check)?;
        if let Err(error) = &answer.outcome {
            return Ok(call_graph_failure(id, error));
        }
        let stable_lookup_key = response.mcp_stable_lookup_key(&query.root_syntax_id, expected)?;
        // #715: compact nodes keep their stable lookup keys.
        let node_keys = match &answer.outcome {
            Ok(result) if compact => response.mcp_stable_lookup_keys(
                result.nodes.iter().map(|node| &node.declaration),
                answer.basis,
            )?,
            _ => Default::default(),
        };
        Ok(projection::call_graph(
            id,
            &mut answer,
            stable_lookup_key.as_deref(),
            &node_keys,
            &response.mcp_language_tiers()?,
            compact,
            root,
            sampled,
            modern,
        )
        .map_err(|error| anyhow::anyhow!(error))?
        .unwrap_or_else(|| tools::failure(id, "range_too_large")))
    }))
}

/// `trellis_show`: checks the target and leaves it in `show_target` for the finished read.
pub(super) fn show_read<'a>(
    cx: ReadCx<'a>,
    args: &'a serde_json::Map<String, serde_json::Value>,
) -> Option<EvidenceToolRead<'a>> {
    let ReadCx {
        id, show_target, ..
    } = cx;
    let request = show::Request::from_arguments(args);
    let expected = validated_expected_basis(args);
    Some(Box::new(move |_, response, _sampled, _query| {
        *show_target.lock().unwrap() = None;
        Ok(match show::check(id, &request, expected, response)? {
            show::Checked::Answer(envelope) => envelope,
            show::Checked::Show(envelope, target, pin) => {
                *show_target.lock().unwrap() = Some((target, pin));
                envelope
            }
        })
    }))
}

/// `trellis_read_source`: one line range of one file.
pub(super) fn source_read<'a>(
    cx: ReadCx<'a>,
    args: &'a serde_json::Map<String, serde_json::Value>,
) -> Option<EvidenceToolRead<'a>> {
    let ReadCx {
        id, root, modern, ..
    } = cx;
    let path = args["path"].as_str().expect("validated source path");
    let start = wire::uint(&args["startLine"]).expect("validated source start");
    let end = wire::uint(&args["endLine"]).expect("validated source end");
    let expected_hash = args.get("expectedContentHash").and_then(|v| v.as_str());
    let expected = validated_expected_basis(args);
    Some(Box::new(
        move |_, response, sampled, _query| match response.mcp_source(
            path,
            start,
            end,
            expected,
            expected_hash,
        ) {
            Ok(Some(found)) => Ok(projection::source(id, found, root, sampled, modern)
                .unwrap_or_else(|| tools::failure(id, "range_too_large"))),
            Ok(None) => Ok(tools::failure(id, "not_found")),
            Err(error)
                if !crate::store::cli_read_retryable_contention(&error)
                    && error.to_string().starts_with("revision conflict:")
                    && error.to_string().contains("content hash") =>
            {
                let mut envelope = tools::failure(id, "revision_conflict");
                if let Some(hash) = response.mcp_source_hash(path)? {
                    envelope["error"]["currentContentHash"] = serde_json::json!(hash);
                }
                Ok(envelope)
            }
            Err(error) => Err(error),
        },
    ))
}
