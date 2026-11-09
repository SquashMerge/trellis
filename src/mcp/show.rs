//! `trellis_show` (#818): take the user's open viewers of this checkout to one Sequence, Class
//! diagram or source line.
//!
//! The target is checked inside the call's admitted snapshot the way the browser's own deep
//! links check theirs (#787): the stale pin, then existence, then the view's own bounds. Only a
//! target that passes is recorded, after the read is finished, as the checkout's one latest view
//! request ([`crate::daemon::view_request`]). The answer is always one typed outcome.
use crate::daemon::registry::CheckoutEvidence;
use crate::daemon::view_request::{Shown, ViewTarget};
use crate::model::{IndexPin, SymbolKind};
use crate::store::mcp_read::SourceRangeRejected;
use serde_json::{Map, Value, json};

pub const NAME: &str = "trellis_show";

/// The catalog entry: the tool's closed input and output schemas.
const ENTRY: &str = include_str!("show_entry.json");

pub fn entry() -> Value {
    serde_json::from_str(ENTRY).expect("static trellis_show entry must parse")
}

/// The validated arguments, as the call worker reads them.
#[derive(Clone, Debug)]
pub enum Request {
    Symbol {
        class: bool,
        symbol_id: Option<String>,
        lookup_key: Option<String>,
    },
    Source {
        path: String,
        line: u64,
    },
}

impl Request {
    /// Arguments that already passed [`super::tools::check`].
    pub fn from_arguments(args: &Map<String, Value>) -> Self {
        let text = |field: &str| args.get(field).and_then(Value::as_str).map(str::to_owned);
        match args["view"].as_str().expect("validated view") {
            "source" => Self::Source {
                path: text("path").expect("validated path"),
                line: super::wire::uint(&args["line"]).expect("validated line"),
            },
            view => Self::Symbol {
                class: view == "class",
                symbol_id: text("symbolId"),
                lookup_key: text("lookupKey"),
            },
        }
    }

    fn view(&self) -> &'static str {
        match self {
            Self::Symbol { class: true, .. } => "class",
            Self::Symbol { .. } => "sequence",
            Self::Source { .. } => "source",
        }
    }
}

/// One outcome's envelope at the admitted basis. Messages are static server text.
fn envelope(id: &Value, pin: IndexPin, view: &str, outcome: &str, message: &str) -> Value {
    json!({"schemaVersion":1,"requestId":id,
        "evidenceBasis":{"indexGeneration":pin.index_generation.to_string(),"indexRevision":pin.index_revision},
        "data":{"outcome":outcome,"view":view,"generation":null,"message":message},
        "warnings":[],"partial":false,"truncated":false,"truncationReason":null})
}

/// What the read decided: an answer, or a target to record once the read has finished.
pub enum Checked {
    Answer(Value),
    Show(Value, ViewTarget, IndexPin),
}

/// Checks `request` in the admitted snapshot. Contention and store failures are errors, which
/// the caller retries or reports with their codes; every other outcome is an answer.
pub fn check(
    id: &Value,
    request: &Request,
    expected: Option<IndexPin>,
    response: &CheckoutEvidence,
) -> anyhow::Result<Checked> {
    let pin = response.status()?.revision;
    let view = request.view();
    let answer = |outcome: &str, message: &str| {
        Ok(Checked::Answer(envelope(id, pin, view, outcome, message)))
    };
    // The deep link's first check: a target read at another snapshot is refused.
    if expected.is_some_and(|expected| expected != pin) {
        return answer(
            "stale",
            "expectedBasis is not the current index snapshot. Read the target again at the \
             evidenceBasis of this answer.",
        );
    }
    let target = match request {
        Request::Source { path, line } => {
            match response.mcp_source(path, *line, *line, Some(pin), None) {
                Ok(Some(_)) => ViewTarget::Source {
                    path: path.clone(),
                    line: *line,
                },
                Ok(None) => return answer("not_found", "No indexed file at that path."),
                // Every typed reason the cached line cannot be read is a target that
                // cannot be shown (#818), never a failure.
                Err(error) => match error.downcast_ref::<SourceRangeRejected>() {
                    Some(reason) => {
                        return answer(
                            "invalid",
                            match reason {
                                SourceRangeRejected::LineAbsent => {
                                    "The line is past the end of the indexed file."
                                }
                                SourceRangeRejected::ByteCap | SourceRangeRejected::LineCap => {
                                    "The line is longer than 16 KiB, so it cannot be shown."
                                }
                                SourceRangeRejected::NotUtf8 => {
                                    "The indexed line is not UTF-8 text."
                                }
                            },
                        );
                    }
                    None => return Err(error),
                },
            }
        }
        Request::Symbol {
            class,
            symbol_id,
            lookup_key,
        } => {
            let symbol_id = match (symbol_id, lookup_key) {
                (Some(symbol_id), _) => symbol_id.clone(),
                (None, Some(key)) => {
                    let found = response.mcp_resolve_lookup_with_lines(key, Some(pin))?;
                    match found.symbols.as_slice() {
                        [] => {
                            return answer(
                                "not_found",
                                "No indexed declaration has that lookup key.",
                            );
                        }
                        [symbol] => symbol.symbol.declaration.syntax_id.clone(),
                        _ => {
                            return answer(
                                "invalid",
                                "The lookup key names several declarations. Pass the symbolId of \
                                 one, from trellis_find_symbols with this lookupKey.",
                            );
                        }
                    }
                }
                (None, None) => unreachable!("validated selector"),
            };
            let Some((_, symbol)) = response.symbol_at(&symbol_id, Some(pin))? else {
                return answer("not_found", "No indexed declaration has that symbol ID.");
            };
            if *class {
                let request = crate::class_diagram::ClassDiagramRequest {
                    seed: symbol_id.clone(),
                    expected_revision: pin,
                    expanded: vec![],
                    include_unmatched: false,
                    include_hierarchy: false,
                };
                match response.class_diagram_at(&request) {
                    Ok(_) => {}
                    Err(error) => {
                        match error.downcast_ref::<crate::class_diagram::InvalidRequest>() {
                            Some(crate::class_diagram::InvalidRequest(message)) => {
                                return answer("invalid", message);
                            }
                            None => return Err(error),
                        }
                    }
                }
                ViewTarget::Class { symbol_id }
            } else {
                if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
                    return answer("invalid", "A sequence starts at a function or method.");
                }
                // The deep link's own read: the sequence must be admitted for this exact seed.
                match response.sequence_at(&symbol_id, pin, false)? {
                    Some(sequence) if sequence.seed.id == symbol_id => {}
                    _ => return answer("not_found", "No indexed sequence for that symbol ID."),
                }
                ViewTarget::Sequence { symbol_id }
            }
        }
    };
    Ok(Checked::Show(
        envelope(id, pin, view, "shown", "Shown."),
        target,
        pin,
    ))
}

/// A checked target waiting for its call's final successful send decision.
pub struct Pending {
    pub runtime: Option<std::sync::Arc<crate::daemon::registry::CheckoutRuntime>>,
    pub target: ViewTarget,
    pub pin: IndexPin,
}

/// Records `pending` at its call's send decision and completes the tool result answered with
/// it. Without a runtime or an open viewer, the answer is `no_viewer` and nothing is recorded.
pub fn commit(pending: Pending, result: Value, name: &str, modern: bool) -> Value {
    let shown = pending.runtime.map_or(Shown::NoViewer, |runtime| {
        runtime
            .view_requests()
            .show(pending.target, pending.pin, std::time::Instant::now())
    });
    super::tools::result(
        name,
        record(result["structuredContent"].clone(), shown),
        modern,
    )
}

/// Completes a checked target's answer with what recording it did.
pub fn record(mut envelope: Value, shown: Shown) -> Value {
    let data = &mut envelope["data"];
    match shown {
        Shown::Shown(generation) => {
            data["generation"] = json!(generation);
            data["message"] = json!(
                "Shown in the open viewers of this checkout. A pinned viewer keeps its view."
            );
        }
        Shown::NoViewer => {
            data["outcome"] = json!("no_viewer");
            data["message"] =
                json!("No browser tab or Herdr pane has this checkout open, so nothing was shown.");
        }
    }
    envelope
}

/// The bounded display summary.
pub fn summary(envelope: &Value) -> String {
    format!(
        "{NAME}: {} {}; full result in structuredContent.",
        envelope["data"]["outcome"].as_str().expect("show outcome"),
        envelope["data"]["view"].as_str().expect("show view"),
    )
}
