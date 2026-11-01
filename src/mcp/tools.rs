//! Closed application DTO validation and read-only tool envelopes.
use super::{IdentityError, OpenedWorkspace, wire};
use crate::daemon::registry::SelectionError;
use serde_json::{Value, json};

const TOOLS: [&str; 11] = super::catalog::NAMES;
#[derive(Debug)]
pub struct PreparedTool {
    pub name: String,
    pub response: Value,
    /// False for invalid application fields: their errors precede workspace verification.
    pub valid_arguments: bool,
    pub id: Value,
    pub modern: bool,
    pub answered: bool,
}
fn closed(o: &serde_json::Map<String, Value>, fields: &[&str], required: &[&str]) -> bool {
    o.keys().all(|k| fields.contains(&k.as_str())) && required.iter().all(|k| o.contains_key(*k))
}
fn string(v: Option<&Value>) -> Option<&str> {
    v?.as_str()
}
fn valid_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && [8, 13, 18, 23].iter().all(|i| b[*i] == b'-')
        && b.iter().enumerate().all(|(i, x)| {
            [8, 13, 18, 23].contains(&i) || x.is_ascii_digit() || (b'a'..=b'f').contains(x)
        })
        && b.iter()
            .enumerate()
            .any(|(i, x)| ![8, 13, 18, 23].contains(&i) && *x != b'0')
}
fn hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn basis(v: &Value) -> bool {
    let Some(o) = v.as_object() else { return false };
    closed(
        o,
        &["indexGeneration", "indexRevision"],
        &["indexGeneration", "indexRevision"],
    ) && string(o.get("indexGeneration")).is_some_and(valid_uuid)
        && o.get("indexRevision").and_then(wire::uint).is_some()
}
fn expected_basis(o: &serde_json::Map<String, Value>) -> bool {
    o.get("expectedBasis").is_none_or(basis)
}
fn limit(o: &serde_json::Map<String, Value>) -> Result<(), &'static str> {
    match o.get("limit") {
        None => Ok(()),
        Some(v) => match wire::uint(v) {
            Some(1..=50) => Ok(()),
            Some(_) => Err("range_too_large"),
            None => Err("invalid_request"),
        },
    }
}
/// An optional explicit semantic producer (#467): 1..=512 bytes, never null.
fn semantic_producer(o: &serde_json::Map<String, Value>) -> Result<(), &'static str> {
    match o.get("semanticProducerId") {
        None => Ok(()),
        Some(Value::String(id)) if crate::model::valid_semantic_producer_id(id) => Ok(()),
        Some(_) => Err("invalid_request"),
    }
}
/// An architecture page offset is any safe UInt; one past the result is
/// `range_too_large` once the result is known.
fn offset(o: &serde_json::Map<String, Value>) -> Result<(), &'static str> {
    match o.get("offset") {
        Some(value) if wire::uint(value).is_none() => Err("invalid_request"),
        _ => Ok(()),
    }
}
fn valid_path(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 4096
        && !s.starts_with('/')
        && !s.contains(['\\', ':', '\0'])
        && s.split('/').all(|part| !matches!(part, "" | "." | ".."))
}
/// Argument validation is independent of workspace state. A malformed UInt is an invalid
/// request even if its numeric value would exceed a narrower application cap.
pub fn validate(name: &str, args: Option<&Value>) -> Result<(), &'static str> {
    if !TOOLS.contains(&name) {
        return Err("invalid_request");
    }
    let Some(o) = args.and_then(Value::as_object) else {
        return Err("invalid_request");
    };
    let (fields, required): (&[&str], &[&str]) = match name {
        "trellis_workspace_describe" => (&["schemaVersion", "workspace"], &["schemaVersion"]),
        "trellis_references" => (
            &[
                "schemaVersion",
                "workspace",
                "expectedBasis",
                "paths",
                "limit",
                "offset",
            ],
            &["schemaVersion"],
        ),
        "trellis_rules_check" => (
            &[
                "schemaVersion",
                "workspace",
                "expectedBasis",
                "paths",
                "limit",
                "offset",
            ],
            &["schemaVersion"],
        ),
        "trellis_rules_explain" => (
            &[
                "schemaVersion",
                "workspace",
                "expectedBasis",
                "rule",
                "target",
                "destination",
                "limit",
                "offset",
            ],
            &["schemaVersion", "rule", "target"],
        ),
        "trellis_find_symbols" => (
            &[
                "schemaVersion",
                "query",
                "limit",
                "expectedBasis",
                "workspace",
            ],
            &["schemaVersion", "query"],
        ),
        "trellis_search_symbols" => (
            &[
                "schemaVersion",
                "query",
                "limit",
                "role",
                "expectedBasis",
                "workspace",
            ],
            &["schemaVersion", "query"],
        ),
        "trellis_declaration" => (
            &["schemaVersion", "symbolId", "expectedBasis", "workspace"],
            &["schemaVersion", "symbolId"],
        ),
        "trellis_outgoing_calls" | "trellis_incoming_calls" => (
            &[
                "schemaVersion",
                "symbolId",
                "limit",
                "semanticProducerId",
                "compact",
                "expectedBasis",
                "workspace",
            ],
            &["schemaVersion", "symbolId"],
        ),
        "trellis_call_graph" => (
            &[
                "schemaVersion",
                "symbolId",
                "semanticProducerId",
                "depth",
                "maxNodes",
                "maxCalls",
                "includeExternal",
                "includeTests",
                "expectedBasis",
                "workspace",
            ],
            &["schemaVersion", "symbolId"],
        ),
        _ => (
            &[
                "schemaVersion",
                "path",
                "startLine",
                "endLine",
                "expectedBasis",
                "expectedContentHash",
                "workspace",
            ],
            &["schemaVersion", "path", "startLine", "endLine"],
        ),
    };
    if !closed(o, fields, required)
        || o.get("schemaVersion").and_then(wire::uint) != Some(1)
        || !expected_basis(o)
    {
        return Err("invalid_request");
    }
    let validated = match name {
        "trellis_workspace_describe" => Ok(()),
        "trellis_references" | "trellis_rules_check" => {
            if !o.get("paths").is_none_or(|value| {
                value.as_array().is_some_and(|paths| {
                    paths.iter().all(|path| {
                        path.as_str().is_some_and(|path| {
                            path.len() <= 4096 && crate::references::valid_path_glob(path)
                        })
                    })
                })
            }) {
                return Err("invalid_request");
            }
            limit(o).and_then(|()| offset(o))
        }
        "trellis_rules_explain" => {
            let rule = string(o.get("rule")).ok_or("invalid_request")?;
            if rule.is_empty()
                || rule.len() > 256
                || !o
                    .get("target")
                    .is_some_and(super::architecture::valid_identity)
                || !o
                    .get("destination")
                    .is_none_or(super::architecture::valid_destination)
                || (!matches!(rule, "$layers" | "$external") && o.contains_key("destination"))
            {
                return Err("invalid_request");
            }
            limit(o).and_then(|()| offset(o))
        }
        "trellis_find_symbols" | "trellis_search_symbols" => {
            let q = string(o.get("query")).ok_or("invalid_request")?;
            let max_bytes = if name == "trellis_search_symbols" {
                1024
            } else {
                256
            };
            if q.is_empty()
                || q.len() > max_bytes
                || (name == "trellis_search_symbols"
                    && !o.get("role").is_none_or(|role| {
                        matches!(role.as_str(), Some("any" | "production" | "test"))
                    }))
            {
                return Err("invalid_request");
            }
            limit(o)
        }
        "trellis_declaration"
        | "trellis_outgoing_calls"
        | "trellis_incoming_calls"
        | "trellis_call_graph" => {
            let sid = string(o.get("symbolId")).ok_or("invalid_request")?;
            if sid.len() > 8192
                || sid.len() != 39
                || !sid.starts_with("sid:v1:")
                || !sid[7..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err("invalid_request");
            }
            if name == "trellis_call_graph" {
                return call_graph_arguments(o).and_then(|()| workspace_field(o));
            }
            if name != "trellis_declaration" {
                semantic_producer(o)?;
                if o.get("compact").is_some_and(|value| !value.is_boolean()) {
                    return Err("invalid_request");
                }
                limit(o)
            } else {
                Ok(())
            }
        }
        _ => {
            let path = string(o.get("path")).ok_or("invalid_request")?;
            if !valid_path(path)
                || !o
                    .get("expectedContentHash")
                    .is_none_or(|v| string(Some(v)).is_some_and(hash))
            {
                return Err("invalid_request");
            }
            let start = o
                .get("startLine")
                .and_then(wire::uint)
                .ok_or("invalid_request")?;
            let end = o
                .get("endLine")
                .and_then(wire::uint)
                .ok_or("invalid_request")?;
            if start == 0 || end == 0 || end < start {
                return Err("invalid_request");
            }
            if end - start >= 200 {
                return Err("range_too_large");
            }
            Ok(())
        }
    };
    validated?;
    workspace_field(o)
}
fn workspace_field(o: &serde_json::Map<String, Value>) -> Result<(), &'static str> {
    if o.get("workspace")
        .is_some_and(|value| value.as_str().is_none_or(|path| path.len() > 4096))
    {
        return Err("invalid_request");
    }
    Ok(())
}
/// `trellis_call_graph` bounds are the contract's and are never clamped: an out-of-range
/// or non-integer value is `invalid_request`, not `range_too_large`.
fn call_graph_arguments(o: &serde_json::Map<String, Value>) -> Result<(), &'static str> {
    use crate::graph_query::{MAX_CALLS, MAX_DEPTH, MAX_NODES};
    let in_range = |field: &str, range: std::ops::RangeInclusive<u64>| match o.get(field) {
        None => Ok(()),
        Some(value) if wire::uint(value).is_some_and(|n| range.contains(&n)) => Ok(()),
        Some(_) => Err("invalid_request"),
    };
    in_range("depth", 0..=MAX_DEPTH)?;
    in_range("maxNodes", 1..=MAX_NODES)?;
    in_range("maxCalls", 0..=MAX_CALLS)?;
    for field in ["includeExternal", "includeTests"] {
        if o.get(field).is_some_and(|value| !value.is_boolean()) {
            return Err("invalid_request");
        }
    }
    match o.get("semanticProducerId") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(producer)) if !producer.is_empty() => Ok(()),
        Some(_) => Err("invalid_request"),
    }
}
/// The validated request with defaults materialized; the server fills in the snapshot IDs.
pub fn call_graph_query(args: &Value) -> crate::graph_query::GraphQuery {
    use crate::graph_query::{DEFAULT_DEPTH, DEFAULT_MAX_CALLS, DEFAULT_MAX_NODES};
    let number = |field: &str, default: u64| {
        args.get(field).map_or(default, |v| {
            wire::uint(v).expect("validated call graph bound")
        })
    };
    crate::graph_query::GraphQuery {
        root_syntax_id: args["symbolId"]
            .as_str()
            .expect("validated call graph root")
            .to_owned(),
        semantic_producer_id: args
            .get("semanticProducerId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        depth: number("depth", DEFAULT_DEPTH),
        max_nodes: number("maxNodes", DEFAULT_MAX_NODES),
        max_calls: number("maxCalls", DEFAULT_MAX_CALLS),
        include_external: args
            .get("includeExternal")
            .is_some_and(|value| value.as_bool().expect("validated call graph flag")),
        include_tests: args
            .get("includeTests")
            .is_some_and(|value| value.as_bool().expect("validated call graph flag")),
    }
}
fn details(code: &str) -> (&'static str, bool) {
    match code {
        "invalid_request" => ("Invalid tool arguments", false),
        "range_too_large" => ("Requested range is too large", false),
        "unsupported_encoding" => ("Unsupported source encoding", false),
        "revision_conflict" => ("Revision conflict", false),
        "index_not_ready" => ("Index is not ready", true),
        "not_found" => ("Not found", false),
        "too_many_requests" => ("Too many requests", true),
        "deadline_exceeded" => ("Deadline exceeded", true),
        "rebuild_in_progress" => ("Index is being rebuilt; retry after it finishes", true),
        "root_changed" => ("Workspace root changed", false),
        "store_unavailable" => ("Store is unavailable", false),
        "checkout_capacity" => ("Checkout capacity reached", true),
        "daemon_unavailable" => ("Daemon is unavailable", true),
        "outcome_unknown" => ("Outcome unknown; inspect request status", false),
        "invalid_rules" => ("Invalid architecture rules", false),
        "rules_not_found" => ("Rules file not found", false),
        "bounded_work" => ("Architecture projection work bound exceeded", false),
        "response_too_large" => ("Architecture result is too large", false),
        _ => ("Store is unavailable", false),
    }
}
pub fn failure(id: &Value, code: &str) -> Value {
    let (message, retryable) = details(code);
    json!({"schemaVersion":1,"requestId":id,"error":{
      "code":code,"message":message,"retryable":retryable,
      "currentBasis":null,"currentContentHash":null}})
}
pub fn describe(id: &Value, label: &str) -> Value {
    super::projection::describe(id, label, "unavailable", None, None, None, false)
}
pub fn attempted(value: &Value) -> Value {
    if let Some(path) = value.as_str() {
        let mut end = path.len().min(256);
        while !path.is_char_boundary(end) {
            end -= 1;
        }
        json!({"kind":"path","value":&path[..end],"utf8Bytes":path.len(),"truncated":end<path.len()})
    } else {
        let kind = match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
            Value::String(_) => unreachable!(),
        };
        json!({"kind":"invalid_type","jsonType":kind})
    }
}
pub fn selection_failure(id: &Value, error: SelectionError, value: &Value) -> Value {
    let mut envelope = failure(id, error.code());
    envelope["error"]["message"] = json!("Workspace selection failed");
    envelope["error"]["retryable"] = json!(error.retryable());
    envelope["error"]["reason"] = json!(error.reason());
    envelope["error"]["attemptedWorkspace"] = attempted(value);
    envelope
}
/// A `too_many_requests` or `deadline_exceeded` decided before read admission
/// sampled `catchingUp` (Decision 0009 §7): no answering workspace, only the
/// bounded attempted one (the selection, or the launch workspace path).
/// A `too_many_requests` refusal names the per-connection call limit (#528), whether
/// the relay or the daemon session refuses; the daemon-wide worker limit overrides it.
pub fn admission_failure(id: &Value, code: &str, attempted_workspace: &Value) -> Value {
    debug_assert!(matches!(code, "too_many_requests" | "deadline_exceeded"));
    let mut envelope = failure(id, code);
    if code == "too_many_requests" {
        envelope["error"]["message"] = json!(format!(
            "Too many concurrent tool calls on this connection (limit {}); wait for one to finish",
            super::MAX_CONNECTION_CALLS
        ));
    }
    envelope["error"]["attemptedWorkspace"] = attempted(attempted_workspace);
    envelope
}
pub fn attributed_root(mut envelope: Value, root: &std::path::Path, catching_up: bool) -> Value {
    envelope["workspace"] = json!(root);
    envelope["catchingUp"] = json!(catching_up);
    envelope
}
pub fn attributed(envelope: Value, workspace: &OpenedWorkspace, catching_up: bool) -> Value {
    attributed_root(envelope, workspace.root(), catching_up)
}
pub fn resolved_root_changed(
    name: &str,
    id: &Value,
    root: &std::path::Path,
    catching_up: bool,
    modern: bool,
) -> Value {
    result(
        name,
        attributed_root(failure(id, "root_changed"), root, catching_up),
        modern,
    )
}
/// Keep the full, schema-valid envelope in structuredContent. Text is only a
/// bounded display summary, not a second JSON representation of the evidence.
pub fn result(name: &str, envelope: Value, modern: bool) -> Value {
    if name == super::architecture::PROMPT_NAME {
        return super::architecture::prompt_result(envelope, modern);
    }
    let error = envelope.get("error").is_some();
    let text = if error {
        format!(
            "{name} failed: {}: {}",
            envelope["error"]["code"]
                .as_str()
                .expect("typed failure code"),
            envelope["error"]["message"]
                .as_str()
                .expect("sanitized failure message")
        )
    } else {
        let (count, items) = match name {
            "trellis_workspace_describe" => (1, "workspace"),
            "trellis_find_symbols" => (
                envelope["data"]["symbols"]
                    .as_array()
                    .expect("find symbols")
                    .len(),
                "symbols",
            ),
            "trellis_search_symbols" => (
                envelope["data"]["hits"]
                    .as_array()
                    .expect("search hits")
                    .len(),
                "hits",
            ),
            "trellis_declaration" => (1, "declaration"),
            "trellis_references" => (
                envelope["data"]["production"]
                    .as_array()
                    .expect("production edges")
                    .len()
                    + envelope["data"]["tests"]
                        .as_array()
                        .expect("test edges")
                        .len(),
                "edges",
            ),
            "trellis_rules_check" | "trellis_rules_explain" => (
                envelope["data"]["instances"]
                    .as_array()
                    .expect("rule instances")
                    .len(),
                "instances",
            ),
            "trellis_incoming_calls" => (
                envelope["data"]["callers"]
                    .as_array()
                    .expect("incoming callers")
                    .len(),
                "calls",
            ),
            "trellis_outgoing_calls" => (
                envelope["data"]["calls"]
                    .as_array()
                    .expect("outgoing calls")
                    .len(),
                "calls",
            ),
            "trellis_call_graph" => (
                envelope["data"]["result"]["edges"]
                    .as_array()
                    .expect("call graph edges")
                    .len(),
                "edges",
            ),
            "trellis_read_source" => (
                (envelope["data"]["endLine"]
                    .as_u64()
                    .expect("source end line")
                    - envelope["data"]["startLine"]
                        .as_u64()
                        .expect("source start line")
                    + 1) as usize,
                "lines",
            ),
            _ => unreachable!("known tool name"),
        };
        let truncation = if envelope["truncated"] == true {
            format!(
                ", truncated: {}",
                envelope["truncationReason"]
                    .as_str()
                    .expect("truncation reason")
            )
        } else {
            String::new()
        };
        // A count of one takes the singular item word (#167): "1 symbol", "1 call".
        let items = if count == 1 {
            items.strip_suffix('s').unwrap_or(items)
        } else {
            items
        };
        format!("{name}: {count} {items}{truncation}; full result in structuredContent.")
    };
    assert!(text.len() <= 512, "tool summary exceeds 512 UTF-8 bytes");
    let mut value = json!({"isError":error,"structuredContent":envelope,
        "content":[{"type":"text","text":text}]});
    if modern {
        value["resultType"] = json!("complete")
    }
    value
}
fn identity_error(error: IdentityError) -> &'static str {
    match error {
        IdentityError::RootChanged => "root_changed",
        IdentityError::StoreUnavailable => "store_unavailable",
    }
}
/// Build one envelope; the text is a display summary of the structured result.
pub fn prepare(
    name: &str,
    args: Option<&Value>,
    id: &Value,
    modern: bool,
    workspace: &OpenedWorkspace,
) -> PreparedTool {
    let validation = if name == super::architecture::PROMPT_NAME {
        Ok(())
    } else {
        validate(name, args)
    };
    let valid_arguments = validation.is_ok();
    let invalid_workspace = args.and_then(Value::as_object).and_then(|object| {
        let value = object
            .get("workspace")?
            .as_str()
            .is_none_or(|path| path.len() > 4096);
        if !value {
            return None;
        }
        let mut without = object.clone();
        let attempted = without.remove("workspace")?;
        validate(name, Some(&Value::Object(without)))
            .ok()
            .map(|_| attempted)
    });
    let envelope = match validation {
        Err(code) => {
            let mut envelope = failure(id, code);
            if let Some(value) = invalid_workspace.as_ref() {
                envelope["error"]["attemptedWorkspace"] = attempted(value);
            }
            envelope
        }
        Ok(()) => attributed(
            match workspace.check() {
                Err(e) => failure(id, identity_error(e)),
                Ok(()) if name == "trellis_workspace_describe" => describe(id, workspace.label()),
                Ok(()) => failure(id, "index_not_ready"),
            },
            workspace,
            false,
        ),
    };
    PreparedTool {
        name: name.to_owned(),
        response: if name == super::architecture::PROMPT_NAME {
            json!({"structuredContent":envelope})
        } else {
            result(name, envelope, modern)
        },
        valid_arguments,
        id: id.clone(),
        modern,
        answered: valid_arguments,
    }
}
/// Run immediately before a valid known-tool outcome is committed to the wire.
/// Never override a malformed application's typed validation failure.
pub fn final_check(prepared: &mut PreparedTool, workspace: &OpenedWorkspace) {
    if prepared.valid_arguments
        && let Err(error) = workspace.check()
    {
        prepared.response = result(
            &prepared.name,
            attributed(
                failure(&prepared.id, identity_error(error)),
                workspace,
                false,
            ),
            prepared.modern,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::wire::{RESPONSE_BYTES, write_response};
    use crate::store::topology::WorkspaceIdentity;
    fn context() -> (tempfile::TempDir, OpenedWorkspace) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("selected");
        std::fs::create_dir(&root).unwrap();
        let opened =
            OpenedWorkspace::new(WorkspaceIdentity::discover(Some(&root), dir.path()).unwrap());
        (dir, opened)
    }
    fn pin() -> Value {
        json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":9007199254740991_u64})
    }
    fn sid() -> String {
        format!("sid:v1:{}", "a".repeat(32))
    }
    fn good(name: &str) -> Value {
        match name {
            "trellis_workspace_describe" => json!({"schemaVersion":1}),
            "trellis_find_symbols" | "trellis_search_symbols" => {
                json!({"schemaVersion":1,"query":"a"})
            }
            "trellis_references" | "trellis_rules_check" => json!({"schemaVersion":1}),
            "trellis_rules_explain" => json!({"schemaVersion":1,"rule":"rule-a",
                "target":{"sourceSetId":"app","language":"rust","path":"src/a.rs","sid":sid()}}),
            "trellis_declaration"
            | "trellis_outgoing_calls"
            | "trellis_incoming_calls"
            | "trellis_call_graph" => {
                json!({"schemaVersion":1,"symbolId":sid()})
            }
            _ => json!({"schemaVersion":1,"path":"src/a.rs","startLine":1,"endLine":1}),
        }
    }
    fn check(name: &str, v: &Value, code: &str) {
        assert_eq!(validate(name, Some(v)), Err(code), "{name} {v}");
    }
    #[test]
    fn closed_application_objects_and_pins() {
        let n = "trellis_workspace_describe";
        assert_eq!(validate(n, Some(&good(n))), Ok(()));
        for v in [
            json!({}),
            json!({"schemaVersion":0}),
            json!({"schemaVersion":1,"other":true}),
            json!({"schemaVersion":1.1}),
            json!({"schemaVersion":null}),
        ] {
            check(n, &v, "invalid_request")
        }
        for n in TOOLS {
            assert_eq!(validate(n, None), Err("invalid_request"));
            for v in [json!(null), json!([]), json!(true), json!("x")] {
                assert_eq!(validate(n, Some(&v)), Err("invalid_request"));
            }
            let mut v = good(n);
            v["schemaVersion"] = json!(2);
            check(n, &v, "invalid_request");
            let mut v = good(n);
            v["unknown"] = json!(true);
            check(n, &v, "invalid_request");
            if n == "trellis_workspace_describe" {
                continue;
            }
            for wrong in [
                json!(null),
                json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000"}),
                json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":1,"extra":0}),
                json!({"indexGeneration":"00000000-0000-0000-0000-000000000000","indexRevision":1}),
                json!({"indexGeneration":"123E4567-e89b-42d3-a456-426614174000","indexRevision":1}),
                json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":-1}),
                json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":9007199254740992_u64}),
            ] {
                let mut v = good(n);
                v["expectedBasis"] = wrong;
                check(n, &v, "invalid_request");
            }
            let mut v = good(n);
            v["expectedBasis"] = pin();
            assert_eq!(validate(n, Some(&v)), Ok(()));
        }
    }
    #[test]
    fn find_and_inspect_boundaries() {
        let n = "trellis_find_symbols";
        for q in ["", &"é".repeat(129), &"x".repeat(257)] {
            let mut v = good(n);
            v["query"] = json!(q);
            check(n, &v, "invalid_request");
        }
        for q in [&"x".repeat(256), &"é".repeat(128), "  a  "] {
            let mut v = good(n);
            v["query"] = json!(q);
            assert_eq!(validate(n, Some(&v)), Ok(()));
        }
        let mut v = good(n);
        v["query"] = json!(null);
        check(n, &v, "invalid_request");
        for name in [
            "trellis_find_symbols",
            "trellis_outgoing_calls",
            "trellis_incoming_calls",
        ] {
            let original = good(name);
            for (number, expected) in [
                (json!(0), Some("range_too_large")),
                (json!(1), None),
                (json!(50), None),
                (json!(51), Some("range_too_large")),
                (json!(null), Some("invalid_request")),
                (json!(1.5), Some("invalid_request")),
                (json!(-1), Some("invalid_request")),
                (json!(9007199254740992_u64), Some("invalid_request")),
                (json!("1"), Some("invalid_request")),
            ] {
                let mut v = original.clone();
                v["limit"] = number;
                assert_eq!(validate(name, Some(&v)).err(), expected, "{name} {v}");
            }
        }
        let n = "trellis_declaration";
        for view in [
            "incoming_calls",
            "call_paths",
            "usages",
            "type_hierarchy",
            "implementations",
            "coverage",
            "other",
        ] {
            let mut v = good(n);
            v["view"] = json!(view);
            check(n, &v, "invalid_request");
        }
        let mut v = good(n);
        v["limit"] = json!(1);
        check(n, &v, "invalid_request");
        let mut v = good("trellis_outgoing_calls");
        v["view"] = json!("outgoing_calls");
        check("trellis_outgoing_calls", &v, "invalid_request");
        for wrong in [
            "sid:v1:ABCDEF0123456789abcdef0123456789",
            "sid:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sid:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaag",
            "occ:v2:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "occ:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            let mut v = good(n);
            v["symbolId"] = json!(wrong);
            check(n, &v, "invalid_request");
        }
    }
    #[test]
    fn search_is_closed_and_bounds_decoded_utf8_bytes() {
        let name = "trellis_search_symbols";
        assert_eq!(super::super::search::DEFAULT_LIMIT, 10);
        for query in [
            "x".repeat(1024),
            "é".repeat(512),
            "  a  ".into(),
            "\n".into(),
        ] {
            assert_eq!(
                validate(name, Some(&json!({"schemaVersion":1,"query":query}))),
                Ok(())
            );
        }
        for query in [
            json!(""),
            json!("x".repeat(1025)),
            json!("é".repeat(513)),
            json!(null),
            json!(1),
        ] {
            check(
                name,
                &json!({"schemaVersion":1,"query":query}),
                "invalid_request",
            );
        }
        for field in [
            "filter", "filters", "backend", "score", "scores", "debug", "cursor", "index", "view",
        ] {
            let mut args = good(name);
            args[field] = json!(true);
            check(name, &args, "invalid_request");
        }
        for (limit, expected) in [
            (json!(1), Ok(())),
            (json!(50), Ok(())),
            (json!(0), Err("range_too_large")),
            (json!(51), Err("range_too_large")),
            (json!(-1), Err("invalid_request")),
            (json!(1.5), Err("invalid_request")),
            (json!(null), Err("invalid_request")),
            (json!("10"), Err("invalid_request")),
        ] {
            assert_eq!(
                validate(
                    name,
                    Some(&json!({"schemaVersion":1,"query":"q","limit":limit}))
                ),
                expected
            );
        }
    }

    #[test]
    fn search_role_is_optional_closed_and_does_not_change_find() {
        let name = "trellis_search_symbols";
        assert_eq!(validate(name, Some(&good(name))), Ok(()));
        for role in ["any", "production", "test"] {
            let mut args = good(name);
            args["role"] = json!(role);
            assert_eq!(validate(name, Some(&args)), Ok(()));
            check("trellis_find_symbols", &args, "invalid_request");
        }
        for role in [
            json!(""),
            json!("Any"),
            json!("tests"),
            json!(" production"),
            json!("test "),
            json!(null),
            json!(true),
            json!(1),
            json!([]),
            json!({}),
        ] {
            let mut args = good(name);
            args["role"] = role;
            check(name, &args, "invalid_request");
        }
    }

    #[test]
    fn source_path_hash_and_line_matrix() {
        let n = "trellis_read_source";
        for path in [
            "",
            "/a",
            "../a",
            "a/../b",
            "a/./b",
            "./a",
            "a//b",
            "a/",
            "a\\b",
            "a:b",
            "a\0b",
            &"x".repeat(4097),
        ] {
            let mut v = good(n);
            v["path"] = json!(path);
            check(n, &v, "invalid_request");
        }
        for path in [&"x".repeat(4096), ".hidden/file", "a/é"] {
            let mut v = good(n);
            v["path"] = json!(path);
            assert_eq!(validate(n, Some(&v)), Ok(()));
        }
        for hash in ["A".repeat(64), "z".repeat(64), "a".repeat(63)] {
            let mut v = good(n);
            v["expectedContentHash"] = json!(hash);
            check(n, &v, "invalid_request");
        }
        let mut v = good(n);
        v["expectedContentHash"] = json!("a".repeat(64));
        v["expectedBasis"] = pin();
        assert_eq!(validate(n, Some(&v)), Ok(()));
        for field in ["startLine", "endLine"] {
            for wrong in [
                json!(0),
                json!(-1),
                json!(null),
                json!(1.1),
                json!(9007199254740992_u64),
                json!("1"),
            ] {
                let mut v = good(n);
                v[field] = wrong;
                check(n, &v, "invalid_request");
            }
        }
        let mut v = good(n);
        v["endLine"] = json!(200);
        assert_eq!(validate(n, Some(&v)), Ok(()));
        v["endLine"] = json!(201);
        check(n, &v, "range_too_large");
        v["startLine"] = json!(202);
        check(n, &v, "invalid_request");
    }
    #[test]
    fn exact_envelopes_identity_precedence_and_complete_wire_bound() {
        let (dir, workspace) = context();
        let max_ids = [
            json!("\u{0000}".repeat(42) + "aa"),
            json!(9007199254740991_i64),
            json!(-9007199254740991_i64),
        ];
        let describe_expected = json!({"schemaVersion":1,"requestId":7,"evidenceBasis":null,"data":{
            "workspaceLabel":"selected","indexState":"unavailable","progress":null,"watcherDegraded":false,"languages":[],"toolVersion":3,"schemaVersion":1,
            "searchIndex":{"backend":null,"unavailableReason":null},
            "limits":{"requestBytes":16384,"responseBytes":65536,"sourceBytes":16384,"sourceLines":200,"defaultLimit":20,"callDefaultLimit":6,"maxLimit":50,"pageBytes":24576,"calleeTextBytes":1024,"hintWorkMs":1000,"deadlineMs":5000,"concurrency":4}},
            "warnings":[],"partial":true,"truncated":false,"truncationReason":null});
        assert_eq!(describe(&json!(7), workspace.label()), describe_expected);
        for modern in [false, true] {
            for id in &max_ids {
                for name in TOOLS {
                    let good = good(name);
                    let prepared = prepare(name, Some(&good), id, modern, &workspace);
                    let result = &prepared.response;
                    assert_eq!(result.get("resultType").is_some(), modern);
                    assert_eq!(result["isError"], name != "trellis_workspace_describe");
                    assert_eq!(
                        result["content"][0]["text"],
                        if name == "trellis_workspace_describe" {
                            format!("{name}: 1 workspace; full result in structuredContent.")
                        } else {
                            format!("{name} failed: index_not_ready: Index is not ready")
                        }
                    );
                    assert_eq!(result["structuredContent"]["requestId"], *id);
                    if name != "trellis_workspace_describe" {
                        assert_eq!(
                            result["structuredContent"]["error"]["code"],
                            "index_not_ready"
                        );
                    }
                    let mut response = Vec::new();
                    write_response(
                        &mut response,
                        &json!({"jsonrpc":"2.0","id":id,"result":result}),
                    )
                    .unwrap();
                    assert!(response.len() <= RESPONSE_BYTES);
                    let invalid = prepare(
                        name,
                        Some(&json!({"schemaVersion":1,"unknown":0})),
                        id,
                        modern,
                        &workspace,
                    );
                    assert_eq!(
                        invalid.response["structuredContent"]["error"]["code"],
                        "invalid_request"
                    );
                    for code in [
                        "invalid_request",
                        "range_too_large",
                        "unsupported_encoding",
                        "revision_conflict",
                        "index_not_ready",
                        "not_found",
                        "too_many_requests",
                        "deadline_exceeded",
                        "rebuild_in_progress",
                        "root_changed",
                        "store_unavailable",
                    ] {
                        let tool = super::result(name, super::failure(id, code), modern);
                        let failure = &tool["structuredContent"]["error"];
                        assert_eq!(
                            tool["content"][0]["text"],
                            format!(
                                "{name} failed: {}: {}",
                                failure["code"].as_str().unwrap(),
                                failure["message"].as_str().unwrap()
                            )
                        );
                        assert!(tool["content"][0]["text"].as_str().unwrap().len() <= 512);
                        let mut wire = Vec::new();
                        write_response(&mut wire, &json!({"jsonrpc":"2.0","id":id,"result":tool}))
                            .unwrap();
                        assert!(wire.len() <= RESPONSE_BYTES);
                        assert_eq!(
                            tool["structuredContent"]["error"]["currentBasis"],
                            Value::Null
                        );
                        assert_eq!(
                            tool["structuredContent"]["error"]["currentContentHash"],
                            Value::Null
                        );
                    }
                }
            }
        }
        let mut unsent = prepare(
            "trellis_workspace_describe",
            Some(&good("trellis_workspace_describe")),
            &json!("pending"),
            true,
            &workspace,
        );
        assert_eq!(unsent.response["isError"], false);
        let root = dir.path().join("selected");
        std::fs::rename(&root, dir.path().join("moved")).unwrap();
        final_check(&mut unsent, &workspace);
        assert_eq!(
            unsent.response["structuredContent"]["error"]["code"],
            "root_changed"
        );
        for name in TOOLS {
            let valid = good(name);
            let mut prepared = prepare(name, Some(&valid), &json!(2), true, &workspace);
            assert_eq!(
                prepared.response["structuredContent"]["error"]["code"],
                "root_changed"
            );
            final_check(&mut prepared, &workspace);
            assert_eq!(
                prepared.response["structuredContent"]["error"]["code"],
                "root_changed"
            );
            let mut invalid = prepare(
                name,
                Some(&json!({"schemaVersion":1,"unknown":0})),
                &json!(2),
                true,
                &workspace,
            );
            final_check(&mut invalid, &workspace);
            assert_eq!(
                invalid.response["structuredContent"]["error"]["code"],
                "invalid_request"
            );
        }
    }
    #[test]
    fn marker_verification_error_is_sanitized_and_does_not_repair() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("git-selected");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        let workspace =
            OpenedWorkspace::new(WorkspaceIdentity::discover(Some(&root), temp.path()).unwrap());
        let mut prepared = prepare(
            "trellis_workspace_describe",
            Some(&good("trellis_workspace_describe")),
            &json!(5),
            false,
            &workspace,
        );
        assert_eq!(prepared.response["isError"], false);
        let marker = root.join(".git/trellis/workspace-id");
        std::fs::remove_file(&marker).unwrap();
        final_check(&mut prepared, &workspace);
        assert_eq!(
            prepared.response["structuredContent"],
            attributed(failure(&json!(5), "store_unavailable"), &workspace, false)
        );
        assert!(!marker.exists());
        let valid = prepare(
            "trellis_find_symbols",
            Some(&good("trellis_find_symbols")),
            &json!(6),
            true,
            &workspace,
        );
        assert_eq!(
            valid.response["structuredContent"]["error"]["code"],
            "store_unavailable"
        );
        let invalid = prepare(
            "trellis_find_symbols",
            Some(&json!({"schemaVersion":1,"query":""})),
            &json!(7),
            true,
            &workspace,
        );
        assert_eq!(
            invalid.response["structuredContent"]["error"]["code"],
            "invalid_request"
        );
    }
    #[test]
    fn call_graph_bounds_are_exact_and_unclamped() {
        let name = "trellis_call_graph";
        for (field, valid, invalid) in [
            ("depth", vec![0, 2, 5], vec![json!(6), json!(-1)]),
            ("maxNodes", vec![1, 150], vec![json!(0), json!(151)]),
            ("maxCalls", vec![0, 500], vec![json!(501), json!(-1)]),
        ] {
            for n in valid {
                let mut v = good(name);
                v[field] = json!(n);
                assert_eq!(validate(name, Some(&v)), Ok(()), "{field} {n}");
            }
            for n in invalid.into_iter().chain([
                json!(null),
                json!(1.5),
                json!("2"),
                json!(9007199254740992_u64),
            ]) {
                let mut v = good(name);
                v[field] = n;
                check(name, &v, "invalid_request");
            }
        }
        for producer in [json!(""), json!(1), json!(true), json!([])] {
            let mut v = good(name);
            v["semanticProducerId"] = producer;
            check(name, &v, "invalid_request");
        }
        for producer in [json!(null), json!("p")] {
            let mut v = good(name);
            v["semanticProducerId"] = producer;
            assert_eq!(validate(name, Some(&v)), Ok(()));
        }
        for field in ["view", "limit", "revisionId", "sourceSetId"] {
            let mut v = good(name);
            v[field] = json!(1);
            check(name, &v, "invalid_request");
        }
        let mut v = good(name);
        v["workspace"] = json!("x".repeat(4097));
        check(name, &v, "invalid_request");
        for field in ["includeExternal", "includeTests"] {
            for flag in [false, true] {
                let mut v = good(name);
                v[field] = json!(flag);
                assert_eq!(validate(name, Some(&v)), Ok(()));
                let query = call_graph_query(&v);
                assert_eq!(
                    if field == "includeExternal" {
                        query.include_external
                    } else {
                        query.include_tests
                    },
                    flag
                );
            }
            for invalid in [json!(null), json!(0), json!("true"), json!([]), json!({})] {
                let mut v = good(name);
                v[field] = invalid;
                check(name, &v, "invalid_request");
            }
        }
        let defaults = call_graph_query(&good(name));
        assert!(!defaults.include_external && !defaults.include_tests);
        assert_eq!(
            (
                defaults.depth,
                defaults.max_nodes,
                defaults.max_calls,
                defaults.semantic_producer_id
            ),
            (2, 150, 500, None)
        );
        let mut v = good(name);
        v["semanticProducerId"] = json!("p");
        v["depth"] = json!(0);
        v["maxNodes"] = json!(1);
        v["maxCalls"] = json!(0);
        let query = call_graph_query(&v);
        assert_eq!((query.depth, query.max_nodes, query.max_calls), (0, 1, 0));
        assert_eq!(query.semantic_producer_id.as_deref(), Some("p"));
        assert_eq!(query.root_syntax_id, sid());
    }
    #[test]
    fn incoming_is_prepared_and_catalogued() {
        let name = "trellis_incoming_calls";
        assert!(TOOLS.contains(&name));
        let mut valid = json!({"schemaVersion":1,"symbolId":sid(),"limit":20,
            "expectedBasis":pin()});
        assert_eq!(validate(name, Some(&valid)), Ok(()));
        for field in ["view", "other"] {
            valid[field] = json!("invalid");
            check(name, &valid, "invalid_request");
            valid.as_object_mut().unwrap().remove(field);
        }
        for field in ["workspace", "expectedBasis", "limit"] {
            valid[field] = Value::Null;
            check(name, &valid, "invalid_request");
            valid.as_object_mut().unwrap().remove(field);
        }
        for limit_value in [json!(0), json!(51), json!(51_u64)] {
            valid["limit"] = limit_value;
            check(name, &valid, "range_too_large");
        }
        for limit_value in [json!(-1), json!(1.5), json!("1")] {
            valid["limit"] = limit_value;
            check(name, &valid, "invalid_request");
        }
        valid.as_object_mut().unwrap().remove("limit");
        valid["symbolId"] = json!("sid:v1:BAD");
        check(name, &valid, "invalid_request");
        valid["symbolId"] = json!("x".repeat(8193));
        check(name, &valid, "invalid_request");
        valid["symbolId"] = json!(sid());
        let (_dir, workspace) = context();
        let prepared = prepare(name, Some(&valid), &json!(1), true, &workspace);
        assert!(prepared.valid_arguments);
    }
    /// #467: the call tools take an optional explicit producer of 1..=512 bytes; anything
    /// else, including an explicit null, is `invalid_request` before any read.
    #[test]
    fn call_tools_bound_the_explicit_semantic_producer() {
        for name in ["trellis_outgoing_calls", "trellis_incoming_calls"] {
            let mut valid = good(name);
            for producer in [json!("scip-typescript"), json!("p".repeat(512))] {
                valid["semanticProducerId"] = producer;
                assert_eq!(validate(name, Some(&valid)), Ok(()), "{name}");
            }
            for producer in [
                Value::Null,
                json!(""),
                json!("p".repeat(513)),
                json!("a\u{0}b"),
                json!(7),
                json!(["scip-typescript"]),
            ] {
                valid["semanticProducerId"] = producer;
                check(name, &valid, "invalid_request");
            }
        }
    }
    /// #528: `compact` is an optional boolean on the two call tools only.
    #[test]
    fn compact_is_a_closed_boolean_on_the_call_tools() {
        for name in ["trellis_outgoing_calls", "trellis_incoming_calls"] {
            let mut valid = good(name);
            for compact in [json!(true), json!(false)] {
                valid["compact"] = compact;
                assert_eq!(validate(name, Some(&valid)), Ok(()), "{name}");
            }
            for compact in [Value::Null, json!(1), json!("true"), json!([true])] {
                valid["compact"] = compact;
                check(name, &valid, "invalid_request");
            }
        }
        for name in [
            "trellis_find_symbols",
            "trellis_declaration",
            "trellis_call_graph",
        ] {
            let mut other = good(name);
            other["compact"] = json!(true);
            check(name, &other, "invalid_request");
        }
    }
    #[test]
    fn summary_uses_the_singular_for_one_item() {
        let success = |data: Value| json!({"schemaVersion":1,"requestId":1,"data":data,"truncated":false,"truncationReason":null});
        for (name, data, text) in [
            ("trellis_find_symbols", json!({"symbols":[{}]}), "1 symbol"),
            ("trellis_find_symbols", json!({"symbols":[]}), "0 symbols"),
            ("trellis_search_symbols", json!({"hits":[{}]}), "1 hit"),
            ("trellis_search_symbols", json!({"hits":[]}), "0 hits"),
            ("trellis_outgoing_calls", json!({"calls":[{}]}), "1 call"),
            ("trellis_incoming_calls", json!({"callers":[{}]}), "1 call"),
            ("trellis_incoming_calls", json!({"callers":[]}), "0 calls"),
            (
                "trellis_outgoing_calls",
                json!({"calls":[{},{}]}),
                "2 calls",
            ),
            (
                "trellis_read_source",
                json!({"startLine":4,"endLine":4}),
                "1 line",
            ),
            (
                "trellis_read_source",
                json!({"startLine":4,"endLine":5}),
                "2 lines",
            ),
            ("trellis_declaration", json!({}), "1 declaration"),
            ("trellis_workspace_describe", json!({}), "1 workspace"),
        ] {
            assert_eq!(
                result(name, success(data), true)["content"][0]["text"],
                format!("{name}: {text}; full result in structuredContent.")
            );
        }
    }
    #[test]
    fn architecture_inputs_are_closed_and_prompt_is_private() {
        let identity = json!({"sourceSetId":"app","language":"rust","path":"src/a.rs","sid":sid()});
        for name in ["trellis_references", "trellis_rules_check"] {
            assert_eq!(
                validate(name, Some(&json!({"schemaVersion":1,"paths":["src/**"]}))),
                Ok(())
            );
            for paths in [
                json!(null),
                json!("src/**"),
                json!(["src/[ab].rs"]),
                json!(["src/**bad"]),
                json!(["../src"]),
                json!(["/src"]),
                json!([true]),
            ] {
                assert_eq!(
                    validate(name, Some(&json!({"schemaVersion":1,"paths":paths}))),
                    Err("invalid_request")
                );
            }
        }
        assert_eq!(
            validate(
                "trellis_references",
                Some(&json!({"schemaVersion":1,"limit":51}))
            ),
            Err("range_too_large")
        );
        // #448: every architecture tool pages with `offset` and bounds samples with `limit`.
        for name in ["trellis_references", "trellis_rules_check"] {
            assert_eq!(
                validate(
                    name,
                    Some(&json!({"schemaVersion":1,"limit":1,"offset":9007199254740991_u64}))
                ),
                Ok(())
            );
            assert_eq!(
                validate(name, Some(&json!({"schemaVersion":1,"limit":0}))),
                Err("range_too_large")
            );
            for offset in [json!(-1), json!(1.5), json!("1"), json!(null)] {
                assert_eq!(
                    validate(name, Some(&json!({"schemaVersion":1,"offset":offset}))),
                    Err("invalid_request")
                );
            }
        }
        assert_eq!(
            validate(
                "trellis_rules_explain",
                Some(
                    &json!({"schemaVersion":1,"rule":"$layers","target":identity,"limit":50,"offset":2})
                )
            ),
            Ok(())
        );
        assert_eq!(
            validate(
                "trellis_rules_explain",
                Some(&json!({"schemaVersion":1,"rule":"$layers","target":identity,"limit":51}))
            ),
            Err("range_too_large")
        );
        assert_eq!(
            validate(
                "trellis_rules_explain",
                Some(&json!({"schemaVersion":1,"rule":"$layers","target":identity}))
            ),
            Ok(())
        );
        assert_eq!(
            validate(
                "trellis_rules_explain",
                Some(
                    &json!({"schemaVersion":1,"rule":"named","target":identity,"destination":identity})
                )
            ),
            Err("invalid_request")
        );
        assert_eq!(
            validate(
                super::super::architecture::PROMPT_NAME,
                Some(&json!({"schemaVersion":1}))
            ),
            Err("invalid_request")
        );
    }
}
