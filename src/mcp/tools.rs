//! Closed application DTO validation and read-only tool envelopes.
use super::{IdentityError, OpenedWorkspace, wire};
use crate::daemon::registry::SelectionError;
use serde_json::{Value, json};

const TOOLS: [&str; 13] = super::catalog::NAMES;
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
fn valid_lookup_key(value: Option<&Value>) -> bool {
    string(value).is_some_and(|key| !key.is_empty())
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
/// A references aggregation (#775): `groupBy` is `file`, `directory` or `package`, and
/// `depth` (1–32 leading directory segments) is accepted only with `directory`.
fn group_by(o: &serde_json::Map<String, Value>) -> Result<(), &'static str> {
    let by = match o.get("groupBy") {
        None => "file",
        Some(value) => match value.as_str() {
            Some(by @ ("file" | "directory" | "package")) => by,
            _ => return Err("invalid_request"),
        },
    };
    match o.get("depth").map(|depth| (by, wire::uint(depth))) {
        None => Ok(()),
        Some(("directory", Some(1..=32))) => Ok(()),
        Some(("directory", Some(_))) => Err("range_too_large"),
        Some(_) => Err("invalid_request"),
    }
}
/// A rules check `status` filter (#725): absent, or 1–3 distinct instance statuses.
fn status_filter(o: &serde_json::Map<String, Value>) -> bool {
    o.get("status").is_none_or(|status| {
        status.as_array().is_some_and(|statuses| {
            !statuses.is_empty()
                && statuses.iter().enumerate().all(|(index, status)| {
                    matches!(status.as_str(), Some("conforms" | "deviates" | "unknown"))
                        && !statuses[..index].contains(status)
                })
        })
    })
}
fn valid_path(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 4096
        && !s.starts_with('/')
        && !s.contains(['\\', ':', '\0'])
        && s.split('/').all(|part| !matches!(part, "" | "." | ".."))
}
/// A failed argument validation (#769): its typed code and, for `invalid_request`, the
/// static server-authored message that names the failing field and the shape it expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub code: &'static str,
    pub message: Option<&'static str>,
}
/// The message of an argument naming no field this tool accepts.
const UNKNOWN_FIELD: &str = "Invalid tool arguments: a field is not in this tool's inputSchema";
/// The accepted and required argument fields of each known tool.
fn fields(name: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match name {
        "trellis_workspace_describe" => (
            &["schemaVersion", "workspace", "coverageDocuments"],
            &["schemaVersion"],
        ),
        "trellis_references" => (
            &[
                "schemaVersion",
                "workspace",
                "expectedBasis",
                "paths",
                "limit",
                "offset",
                "groupBy",
                "depth",
                "prefix",
                "app",
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
                "status",
                "app",
            ],
            &["schemaVersion"],
        ),
        "trellis_endpoints" => (
            &[
                "schemaVersion",
                "workspace",
                "expectedBasis",
                "app",
                "method",
                "pathTemplate",
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
                "lookupKey",
                "limit",
                "compact",
                "expectedBasis",
                "workspace",
                "app",
            ],
            &["schemaVersion"],
        ),
        "trellis_search_symbols" => (
            &[
                "schemaVersion",
                "query",
                "limit",
                "role",
                "compact",
                "expectedBasis",
                "workspace",
                "app",
            ],
            &["schemaVersion", "query"],
        ),
        "trellis_declaration" => (
            &[
                "schemaVersion",
                "symbolId",
                "lookupKey",
                "expectedBasis",
                "workspace",
            ],
            &["schemaVersion"],
        ),
        "trellis_outgoing_calls" => (
            &[
                "schemaVersion",
                "symbolId",
                "lookupKey",
                "limit",
                "semanticProducerId",
                "compact",
                "includeExternal",
                "expectedBasis",
                "workspace",
            ],
            &["schemaVersion"],
        ),
        "trellis_incoming_calls" => (
            &[
                "schemaVersion",
                "symbolId",
                "lookupKey",
                "limit",
                "semanticProducerId",
                "compact",
                "expectedBasis",
                "workspace",
            ],
            &["schemaVersion"],
        ),
        "trellis_show" => (
            &[
                "schemaVersion",
                "view",
                "symbolId",
                "lookupKey",
                "path",
                "line",
                "expectedBasis",
                "workspace",
            ],
            &["schemaVersion", "view"],
        ),
        "trellis_call_graph" => (
            &[
                "schemaVersion",
                "symbolId",
                "lookupKey",
                "semanticProducerId",
                "depth",
                "maxNodes",
                "maxCalls",
                "includeExternal",
                "includeTests",
                "compact",
                "expectedBasis",
                "workspace",
                "app",
            ],
            &["schemaVersion"],
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
    }
}
/// The static `invalid_request` message for one failing field of `name` (#769). Server
/// text only: it never echoes the caller's arguments or repository content.
fn field_message(name: &str, field: &str) -> &'static str {
    match (name, field) {
        (_, "arguments") => "Tool arguments must be a JSON object",
        (_, "schemaVersion") => "`schemaVersion` must be 1",
        (_, "workspace") => {
            "`workspace` must be an absolute checkout path string of at most 4096 bytes"
        }
        (_, "expectedBasis") => {
            "`expectedBasis` must be the evidenceBasis object of an earlier result: \
             {indexGeneration, indexRevision}"
        }
        (_, "limit") => "`limit` must be an integer from 1 to 50",
        (_, "offset") => "`offset` must be a non-negative integer, such as page.nextOffset",
        (_, "paths") => {
            "`paths` must be an array of anchored source path globs, each at most 4096 bytes"
        }
        (_, "status") => {
            "`status` must be 1 to 3 distinct values from \"conforms\", \"deviates\", \"unknown\""
        }
        (_, "rule") => "`rule` must be a rule name of 1 to 256 bytes, such as \"$layers\"",
        // A client that stringifies an object argument is told so (#769).
        (_, "expectedBasis:string") => {
            "`expectedBasis` must be a JSON object (got a string): the evidenceBasis object \
             of an earlier result, {indexGeneration, indexRevision}"
        }
        (_, "target:string") => {
            "`target` must be a JSON object (got a string): the identity object from a \
             trellis_rules_check instance, {sourceSetId, language, path, sid}"
        }
        (_, "destination:string") => {
            "`destination` must be a JSON object (got a string): the destination object \
             from a trellis_rules_check instance"
        }
        (_, "target") => {
            "`target` must be the identity object from a trellis_rules_check instance: \
             {sourceSetId, language, path, sid}"
        }
        (_, "destination") => {
            "`destination` must be the destination object from a trellis_rules_check \
             instance: {sourceSetId, language, path, sid} or {kind:\"externalPackage\", \
             sourceSetId, language, package, standardLibrary}, and only for $layers or \
             $external"
        }
        ("trellis_search_symbols", "query") => {
            "`query` must be a nonempty string of at most 1024 bytes"
        }
        (_, "query") => "`query` must be a nonempty string of at most 256 bytes",
        (_, "query|lookupKey") => "Pass `query` or `lookupKey`, not both",
        (_, "symbolId|lookupKey") => "Pass exactly one of `symbolId` or `lookupKey`",
        (_, "lookupKey") => {
            "`lookupKey` must be a nonempty lookup key string from an earlier result"
        }
        (_, "symbolId") => {
            "`symbolId` must be a syntax ID: sid:v1: followed by 32 lowercase hex digits"
        }
        (_, "role") => "`role` must be \"any\", \"production\" or \"test\"",
        ("trellis_call_graph", "semanticProducerId") => {
            "`semanticProducerId` must be a nonempty producer ID string or null"
        }
        (_, "semanticProducerId") => {
            "`semanticProducerId` must be a producer ID string of 1 to 512 bytes"
        }
        (_, "compact") => "`compact` must be true or false",
        (_, "coverageDocuments") => {
            "`coverageDocuments` must be an object with optional `language` (a language name), \
             `limit` (1 to 50) and `offset`"
        }
        (_, "groupBy") => {
            "`groupBy` must be \"file\", \"directory\" or \"package\"; `depth` (1 to 32) \
             goes only with \"directory\""
        }
        (_, "prefix") => {
            "`prefix` must be a checkout-relative directory such as \"src/store/\": `/` \
             separators, no `.` or `..` segment, at most 4096 bytes"
        }
        (_, "app") => {
            "`app` must be an app name or manifest path of 1 to 4096 bytes, as \
             trellis_workspace_describe lists them"
        }
        (_, "method") => {
            "`method` must be one of \"GET\", \"POST\", \"PUT\", \"DELETE\", \"PATCH\", \
             \"HEAD\", \"OPTIONS\""
        }
        (_, "pathTemplate") => {
            "`pathTemplate` must be a route path template of 1 to 4096 bytes, such as \
             \"/api/items/{id}\""
        }
        (_, "includeExternal") => "`includeExternal` must be true or false",
        (_, "includeTests") => "`includeTests` must be true or false",
        (_, "depth") => "`depth` must be an integer from 0 to 5",
        (_, "maxNodes") => "`maxNodes` must be an integer from 1 to 150",
        (_, "maxCalls") => "`maxCalls` must be an integer from 0 to 500",
        (_, "path") => "`path` must be a checkout-relative file path",
        (_, "view") => "`view` must be \"sequence\", \"class\" or \"source\"",
        (_, "line") => "`line` must be a line number from 1",
        (_, "view:symbol") => {
            "`view` \"sequence\" and \"class\" take exactly one of `symbolId` or `lookupKey`, \
             and no `path` or `line`"
        }
        (_, "view:source") => {
            "`view` \"source\" takes `path` and `line`, and no `symbolId` or `lookupKey`"
        }
        (_, "expectedContentHash") => {
            "`expectedContentHash` must be a 64-digit lowercase hex content hash"
        }
        (_, "startLine" | "endLine") => {
            "`startLine` and `endLine` must be line numbers from 1, with `endLine` not \
             before `startLine`"
        }
        _ => UNKNOWN_FIELD,
    }
}
/// Argument validation is independent of workspace state. A malformed UInt is an invalid
/// request even if its numeric value would exceed a narrower application cap.
pub fn validate(name: &str, args: Option<&Value>) -> Result<(), &'static str> {
    check(name, args).map_err(|rejection| rejection.code)
}
/// [`validate`], with the failing field's message (#769).
pub fn check(name: &str, args: Option<&Value>) -> Result<(), Rejection> {
    if !TOOLS.contains(&name) {
        return Err(Rejection {
            code: "invalid_request",
            message: None,
        });
    }
    // An `invalid_request` names its field; another code keeps its own message.
    let bad = |field: &str| Rejection {
        code: "invalid_request",
        message: Some(field_message(name, field)),
    };
    // An object field that arrived as a string says so (#769).
    let object = |field: &'static str, stringified: &'static str| {
        if string_field(args, field) {
            bad(stringified)
        } else {
            bad(field)
        }
    };
    let coded = |field: &'static str| {
        move |code: &'static str| {
            if code == "invalid_request" {
                bad(field)
            } else {
                Rejection {
                    code,
                    message: None,
                }
            }
        }
    };
    let Some(o) = args.and_then(Value::as_object) else {
        return Err(bad("arguments"));
    };
    let (fields, required) = fields(name);
    // An unknown field fails even once a missing one is added, so it is named first.
    if !o.keys().all(|key| fields.contains(&key.as_str())) {
        return Err(bad(""));
    }
    if let Some(missing) = required.iter().find(|field| !o.contains_key(**field)) {
        return Err(bad(missing));
    }
    if o.get("schemaVersion").and_then(wire::uint) != Some(1) {
        return Err(bad("schemaVersion"));
    }
    if !expected_basis(o) {
        return Err(object("expectedBasis", "expectedBasis:string"));
    }
    // `compact` (#528, #665, #715) is a closed boolean wherever a tool accepts it.
    if o.get("compact").is_some_and(|value| !value.is_boolean()) {
        return Err(bad("compact"));
    }
    // #851: `app` is one nonempty string wherever a tool accepts it.
    if !o.get("app").is_none_or(|app| bounded_text(Some(app))) {
        return Err(bad("app"));
    }
    match name {
        "trellis_workspace_describe" => {
            if let Some(request) = o.get("coverageDocuments") {
                let request = request
                    .as_object()
                    .ok_or_else(|| bad("coverageDocuments"))?;
                if !request
                    .keys()
                    .all(|key| matches!(key.as_str(), "language" | "limit" | "offset"))
                    || !request.get("language").is_none_or(|language| {
                        language.as_str().is_some_and(|language| {
                            crate::semantic_evidence::LANGUAGE_ORDER.contains(&language)
                        })
                    })
                {
                    return Err(bad("coverageDocuments"));
                }
                limit(request).map_err(coded("coverageDocuments"))?;
                offset(request).map_err(coded("coverageDocuments"))?;
            }
        }
        "trellis_references" | "trellis_rules_check" => {
            group_by(o).map_err(coded("groupBy"))?;
            // #837: a checkout-relative directory, normalized but for one trailing `/`.
            if !o.get("prefix").is_none_or(|prefix| {
                prefix
                    .as_str()
                    .and_then(crate::references::directory_prefix)
                    .is_some()
            }) {
                return Err(bad("prefix"));
            }
            if !o.get("paths").is_none_or(|value| {
                value.as_array().is_some_and(|paths| {
                    paths.iter().all(|path| {
                        path.as_str().is_some_and(|path| {
                            path.len() <= 4096 && crate::references::valid_path_glob(path)
                        })
                    })
                })
            }) {
                return Err(bad("paths"));
            }
            if !status_filter(o) {
                return Err(bad("status"));
            }
            limit(o).map_err(coded("limit"))?;
            offset(o).map_err(coded("offset"))?;
        }
        "trellis_endpoints" => {
            if !o.get("method").is_none_or(|method| {
                string(Some(method)).is_some_and(|method| {
                    matches!(
                        method,
                        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS"
                    )
                })
            }) {
                return Err(bad("method"));
            }
            if !o
                .get("pathTemplate")
                .is_none_or(|template| bounded_text(Some(template)))
            {
                return Err(bad("pathTemplate"));
            }
            limit(o).map_err(coded("limit"))?;
            offset(o).map_err(coded("offset"))?;
        }
        "trellis_rules_explain" => {
            let rule = string(o.get("rule")).ok_or_else(|| bad("rule"))?;
            if rule.is_empty() || rule.len() > 256 {
                return Err(bad("rule"));
            }
            if !o
                .get("target")
                .is_some_and(super::architecture::valid_identity)
            {
                return Err(object("target", "target:string"));
            }
            if !o
                .get("destination")
                .is_none_or(super::architecture::valid_destination)
                || (!matches!(rule, "$layers" | "$external") && o.contains_key("destination"))
            {
                return Err(object("destination", "destination:string"));
            }
            limit(o).map_err(coded("limit"))?;
            offset(o).map_err(coded("offset"))?;
        }
        "trellis_find_symbols" if o.contains_key("lookupKey") => {
            if o.contains_key("query") {
                return Err(bad("query|lookupKey"));
            }
            if !valid_lookup_key(o.get("lookupKey")) {
                return Err(bad("lookupKey"));
            }
            limit(o).map_err(coded("limit"))?;
        }
        "trellis_find_symbols" | "trellis_search_symbols" => {
            let q = string(o.get("query")).ok_or_else(|| bad("query"))?;
            let max_bytes = if name == "trellis_search_symbols" {
                1024
            } else {
                256
            };
            if q.is_empty() || q.len() > max_bytes {
                return Err(bad("query"));
            }
            if name == "trellis_search_symbols"
                && !o
                    .get("role")
                    .is_none_or(|role| matches!(role.as_str(), Some("any" | "production" | "test")))
            {
                return Err(bad("role"));
            }
            limit(o).map_err(coded("limit"))?;
        }
        "trellis_show" => match string(o.get("view")) {
            Some("sequence" | "class") => {
                if o.contains_key("path")
                    || o.contains_key("line")
                    || o.contains_key("symbolId") == o.contains_key("lookupKey")
                {
                    return Err(bad("view:symbol"));
                }
                selector(o).map_err(bad)?;
            }
            Some("source") => {
                if o.contains_key("symbolId") || o.contains_key("lookupKey") {
                    return Err(bad("view:source"));
                }
                let path = string(o.get("path")).ok_or_else(|| bad("path"))?;
                if !valid_path(path) {
                    return Err(bad("path"));
                }
                if !o
                    .get("line")
                    .and_then(wire::uint)
                    .is_some_and(|line| line >= 1)
                {
                    return Err(bad("line"));
                }
            }
            _ => return Err(bad("view")),
        },
        "trellis_declaration"
        | "trellis_outgoing_calls"
        | "trellis_incoming_calls"
        | "trellis_call_graph" => {
            if o.contains_key("symbolId") == o.contains_key("lookupKey") {
                return Err(bad("symbolId|lookupKey"));
            }
            selector(o).map_err(bad)?;
            if name == "trellis_call_graph" {
                call_graph_arguments(o).map_err(bad)?;
            } else if name != "trellis_declaration" {
                semantic_producer(o).map_err(coded("semanticProducerId"))?;
                if let Some(field) = ["compact", "includeExternal"]
                    .into_iter()
                    .find(|field| o.get(*field).is_some_and(|value| !value.is_boolean()))
                {
                    return Err(bad(field));
                }
                limit(o).map_err(coded("limit"))?;
            }
        }
        _ => {
            let path = string(o.get("path")).ok_or_else(|| bad("path"))?;
            if !valid_path(path) {
                return Err(bad("path"));
            }
            if !o
                .get("expectedContentHash")
                .is_none_or(|v| string(Some(v)).is_some_and(hash))
            {
                return Err(bad("expectedContentHash"));
            }
            let start = o
                .get("startLine")
                .and_then(wire::uint)
                .ok_or_else(|| bad("startLine"))?;
            let end = o
                .get("endLine")
                .and_then(wire::uint)
                .ok_or_else(|| bad("endLine"))?;
            if start == 0 || end == 0 || end < start {
                return Err(bad("endLine"));
            }
            if end - start >= 200 {
                return Err(Rejection {
                    code: "range_too_large",
                    message: None,
                });
            }
        }
    }
    if workspace_field(o).is_err() {
        return Err(bad("workspace"));
    }
    Ok(())
}
/// One present `symbolId` (a syntax ID) or `lookupKey` (nonempty); the failing field if not.
fn selector(o: &serde_json::Map<String, Value>) -> Result<(), &'static str> {
    if let Some(value) = o.get("symbolId") {
        let sid = value.as_str().ok_or("symbolId")?;
        if sid.len() != 39
            || !sid.starts_with("sid:v1:")
            || !sid[7..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("symbolId");
        }
    } else if !valid_lookup_key(o.get("lookupKey")) {
        return Err("lookupKey");
    }
    Ok(())
}
/// A string of 1 to 4096 bytes (#851).
fn bounded_text(value: Option<&Value>) -> bool {
    string(value).is_some_and(|text| !text.is_empty() && text.len() <= 4096)
}
/// Whether `field` of the arguments is a JSON string.
fn string_field(args: Option<&Value>, field: &str) -> bool {
    args.and_then(|args| args.get(field))
        .is_some_and(Value::is_string)
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
/// or non-integer value is `invalid_request`, not `range_too_large`. The error is the
/// failing field.
fn call_graph_arguments(o: &serde_json::Map<String, Value>) -> Result<(), &'static str> {
    use crate::graph_query::{MAX_CALLS, MAX_DEPTH, MAX_NODES};
    let in_range = |field: &'static str, range: std::ops::RangeInclusive<u64>| match o.get(field) {
        None => Ok(()),
        Some(value) if wire::uint(value).is_some_and(|n| range.contains(&n)) => Ok(()),
        Some(_) => Err(field),
    };
    in_range("depth", 0..=MAX_DEPTH)?;
    in_range("maxNodes", 1..=MAX_NODES)?;
    in_range("maxCalls", 0..=MAX_CALLS)?;
    for field in ["includeExternal", "includeTests", "compact"] {
        if o.get(field).is_some_and(|value| !value.is_boolean()) {
            return Err(field);
        }
    }
    match o.get("semanticProducerId") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(producer)) if !producer.is_empty() => Ok(()),
        Some(_) => Err("semanticProducerId"),
    }
}
/// #665: the MCP graph's default `maxNodes` and `maxCalls`, sized so a default answer fits
/// the 65,536-byte response. A full answer carries every proof: eight nodes and ten
/// semantic edges in this repository's shape (`DaemonState::enqueue_index`) measured about
/// 50 KB. A compact answer drops them: fifty nodes and sixty edges measured about 51 KB,
/// plus about 3 KB for the #715 node `stableLookupKey`s.
pub const GRAPH_DEFAULT_LIMITS: (u64, u64) = (8, 10);
pub const GRAPH_COMPACT_DEFAULT_LIMITS: (u64, u64) = (50, 60);

/// The validated request with defaults materialized; the server fills in the snapshot IDs.
pub fn call_graph_query(args: &Value) -> crate::graph_query::GraphQuery {
    use crate::graph_query::DEFAULT_DEPTH;
    let number = |field: &str, default: u64| {
        args.get(field).map_or(default, |v| {
            wire::uint(v).expect("validated call graph bound")
        })
    };
    let compact = args.get("compact").and_then(Value::as_bool) == Some(true);
    let (default_max_nodes, default_max_calls) = if compact {
        GRAPH_COMPACT_DEFAULT_LIMITS
    } else {
        GRAPH_DEFAULT_LIMITS
    };
    crate::graph_query::GraphQuery {
        // A lookup-key root is resolved inside the admitted evidence response.
        root_syntax_id: args
            .get("symbolId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        semantic_producer_id: args
            .get("semanticProducerId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        depth: number("depth", DEFAULT_DEPTH),
        max_nodes: number("maxNodes", default_max_nodes),
        max_calls: number("maxCalls", default_max_calls),
        include_external: args
            .get("includeExternal")
            .is_some_and(|value| value.as_bool().expect("validated call graph flag")),
        include_tests: args
            .get("includeTests")
            .is_some_and(|value| value.as_bool().expect("validated call graph flag")),
        // #851: filled in from the admitted read.
        app_paths: None,
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
        // #769: point at the prompt that drafts the missing file.
        "rules_not_found" => (
            "No .trellis/rules.toml. Ask the user to run the `trellis_propose_rules` MCP \
             prompt (in Claude Code: /mcp__trellis__trellis_propose_rules) to draft one \
             for review.",
            false,
        ),
        "bounded_work" => ("Architecture projection work bound exceeded", false),
        "response_too_large" => ("Architecture result is too large", false),
        "building" => (
            "Architecture is being computed for this revision; retry shortly",
            true,
        ),
        _ => ("Store is unavailable", false),
    }
}
pub fn failure(id: &Value, code: &str) -> Value {
    let (message, retryable) = details(code);
    json!({"schemaVersion":1,"requestId":id,"error":{
      "code":code,"message":message,"retryable":retryable,
      "currentBasis":null,"currentContentHash":null}})
}
/// A finished answer over the response cap, as that one call's typed failure (#930). It
/// keeps the answer's attribution (`workspace`, `catchingUp`) and its `evidenceBasis` as the
/// current basis. It is retryable only while the answering workspace is catching up, when
/// what the answer reports may still change; otherwise the same request answers the same.
pub fn response_too_large(id: &Value, answer: &Value) -> Value {
    let mut envelope = failure(id, "response_too_large");
    envelope["error"]["message"] =
        json!("Result exceeds the 65,536-byte response cap; narrow the request");
    if let Some(basis) = answer
        .get("evidenceBasis")
        .filter(|basis| basis.is_object())
    {
        envelope["error"]["currentBasis"] = basis.clone();
    }
    if let Some(workspace) = answer.get("workspace") {
        let catching_up = answer.get("catchingUp") == Some(&json!(true));
        envelope["error"]["retryable"] = json!(catching_up);
        envelope["workspace"] = workspace.clone();
        envelope["catchingUp"] = json!(catching_up);
    }
    envelope
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
        let (count, items) = if let Some(candidates) = envelope["data"]["candidates"].as_array() {
            (candidates.len(), "candidates")
        } else {
            match name {
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
                // Its summary names the outcome instead (below).
                "trellis_show" => (0, "targets"),
                "trellis_endpoints" => (
                    envelope["data"]["endpoints"]
                        .as_array()
                        .expect("endpoint rows")
                        .len(),
                    "endpoints",
                ),
                _ => unreachable!("known tool name"),
            }
        };
        let truncation = if envelope["truncated"] == true {
            let reason = envelope["truncationReason"]
                .as_str()
                .expect("truncation reason");
            // #665: say how much of the bounded graph the response bytes left out.
            let omitted = &envelope["data"]["responseOmitted"];
            match (omitted["nodes"].as_u64(), omitted["edges"].as_u64()) {
                (Some(nodes), Some(edges)) if nodes + edges > 0 => {
                    let hint = if envelope["data"].get("semanticBases").is_some() {
                        "; compact:true fits more"
                    } else {
                        ""
                    };
                    format!(", truncated: {reason} ({nodes} nodes, {edges} edges omitted{hint})")
                }
                _ => format!(", truncated: {reason}"),
            }
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
    let text = if !error && name == super::show::NAME {
        super::show::summary(&envelope)
    } else {
        text
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
        check(name, args)
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
        Err(rejection) => {
            let mut envelope = failure(id, rejection.code);
            if let Some(message) = rejection.message {
                envelope["error"]["message"] = json!(message);
            }
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
            "trellis_references" | "trellis_rules_check" | "trellis_endpoints" => {
                json!({"schemaVersion":1})
            }
            "trellis_rules_explain" => json!({"schemaVersion":1,"rule":"rule-a",
                "target":{"sourceSetId":"app","language":"rust","path":"src/a.rs","sid":sid()}}),
            "trellis_declaration"
            | "trellis_outgoing_calls"
            | "trellis_incoming_calls"
            | "trellis_call_graph" => {
                json!({"schemaVersion":1,"symbolId":sid()})
            }
            "trellis_show" => json!({"schemaVersion":1,"view":"sequence","symbolId":sid()}),
            _ => json!({"schemaVersion":1,"path":"src/a.rs","startLine":1,"endLine":1}),
        }
    }
    fn check(name: &str, v: &Value, code: &str) {
        assert_eq!(validate(name, Some(v)), Err(code), "{name} {v}");
    }
    /// #769: every accepted field of every tool has its own static message, short enough
    /// for the 512-byte summary, and a bad value of it names it. Graph bounds name the
    /// constants they check. Other codes keep their generic message.
    #[test]
    fn invalid_fields_name_themselves_in_static_messages() {
        let message = |name: &str, v: &Value| super::check(name, Some(v)).unwrap_err().message;
        for name in TOOLS {
            let (accepted, required) = fields(name);
            for field in accepted {
                let text = field_message(name, field);
                assert_ne!(text, UNKNOWN_FIELD, "{name} {field}");
                assert!(text.contains(&format!("`{field}`")), "{text}");
                let summary = format!("{name} failed: invalid_request: {text}");
                assert!(summary.len() <= 512);
                if *field == "schemaVersion" {
                    continue;
                }
                // A value of the wrong type is rejected and names the field
                // (or the exclusive pair it belongs to).
                let mut bad = good(name);
                bad[*field] = json!({"wrong":"type"});
                if *field == "lookupKey" {
                    bad.as_object_mut().unwrap().remove("symbolId");
                    bad.as_object_mut().unwrap().remove("query");
                }
                let named = message(name, &bad).expect("invalid_request message");
                assert!(
                    named.contains(&format!("`{field}`")),
                    "{name} {field}: {named}"
                );
                if required.contains(field) {
                    let mut missing = good(name);
                    missing.as_object_mut().unwrap().remove(*field);
                    assert_eq!(message(name, &missing), Some(text), "{name} {field}");
                }
            }
            let mut unknown = good(name);
            unknown["unexpected"] = json!(true);
            assert_eq!(message(name, &unknown), Some(UNKNOWN_FIELD));
            assert_eq!(
                message(name, &json!([])),
                Some("Tool arguments must be a JSON object")
            );
        }
        use crate::graph_query::{MAX_CALLS, MAX_DEPTH, MAX_NODES};
        for (field, range) in [
            ("depth", format!("from 0 to {MAX_DEPTH}")),
            ("maxNodes", format!("from 1 to {MAX_NODES}")),
            ("maxCalls", format!("from 0 to {MAX_CALLS}")),
        ] {
            assert!(field_message("trellis_call_graph", field).ends_with(&range));
        }
        // An object field sent as a string (a stringifying client) says so.
        for (name, field) in [
            ("trellis_rules_explain", "target"),
            ("trellis_rules_explain", "destination"),
            ("trellis_rules_explain", "expectedBasis"),
            ("trellis_find_symbols", "expectedBasis"),
        ] {
            let mut stringified = good(name);
            let object = json!({"indexGeneration":"x"}).to_string();
            stringified[field] = json!(object);
            if field == "destination" {
                stringified["rule"] = json!("$layers");
            }
            let named = message(name, &stringified).unwrap();
            assert!(
                named.starts_with(&format!("`{field}` must be a JSON object (got a string)")),
                "{named}"
            );
        }
        // A numeric cap keeps its own code and generic message.
        let over = super::check(
            "trellis_rules_check",
            Some(&json!({"schemaVersion":1,"limit":51})),
        );
        assert_eq!(
            over,
            Err(Rejection {
                code: "range_too_large",
                message: None
            })
        );
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
    fn stable_lookup_selectors_are_exclusive_and_nonempty() {
        for name in [
            "trellis_find_symbols",
            "trellis_declaration",
            "trellis_outgoing_calls",
            "trellis_incoming_calls",
            "trellis_call_graph",
        ] {
            let other = if name == "trellis_find_symbols" {
                "query"
            } else {
                "symbolId"
            };
            let mut args = json!({"schemaVersion":1,"lookupKey":"src/store.rs::Store::read"});
            assert_eq!(validate(name, Some(&args)), Ok(()));
            // No new per-field cap: only the existing transport request bound applies.
            args["lookupKey"] = json!("x".repeat(9000));
            assert_eq!(validate(name, Some(&args)), Ok(()));
            for invalid in [json!(""), json!(null), json!(1), json!([])] {
                args["lookupKey"] = invalid;
                check(name, &args, "invalid_request");
            }
            args["lookupKey"] = json!("src/store.rs::Store::read");
            args[other] = good(name)[other].clone();
            check(name, &args, "invalid_request");
            check(name, &json!({"schemaVersion":1}), "invalid_request");
        }
        check(
            "trellis_search_symbols",
            &json!({"schemaVersion":1,"lookupKey":"src/a.rs::a"}),
            "invalid_request",
        );
    }

    #[test]
    fn candidate_summaries_precede_operation_fields() {
        for name in [
            "trellis_declaration",
            "trellis_outgoing_calls",
            "trellis_incoming_calls",
            "trellis_call_graph",
        ] {
            let envelope = json!({"schemaVersion":1,"requestId":1,"data":{"lookupKey":"src/a.rs::a","candidates":[{},{}]},"truncated":false});
            let summary = result(name, envelope, true);
            assert_eq!(summary["isError"], false);
            assert_eq!(
                summary["content"][0]["text"],
                format!("{name}: 2 candidates; full result in structuredContent.")
            );
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
            "workspaceLabel":"selected","indexState":"unavailable","progress":null,"watcherDegraded":false,"languages":[],"toolVersion":29,"schemaVersion":1,
            "searchIndex":{"backend":null,"unavailableReason":null},
            "apps":{"state":"unavailable","items":[],"noApp":null},
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
            (2, 8, 10, None)
        );
        // #665: compact answers are smaller, so their default limits are larger.
        let mut v = good(name);
        v["compact"] = json!(true);
        let compact = call_graph_query(&v);
        assert_eq!((compact.max_nodes, compact.max_calls), (50, 60));
        v["maxNodes"] = json!(150);
        v["maxCalls"] = json!(500);
        let compact = call_graph_query(&v);
        assert_eq!((compact.max_nodes, compact.max_calls), (150, 500));
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
    /// #528, #715: `compact` is an optional boolean on the call tools and on find and
    /// search (query and lookup-key find alike); other tools reject it.
    #[test]
    fn compact_is_a_closed_boolean_on_the_call_and_symbol_tools() {
        let lookup = json!({"schemaVersion":1,"lookupKey":"src/a.rs::a"});
        for (name, base) in [
            ("trellis_outgoing_calls", good("trellis_outgoing_calls")),
            ("trellis_incoming_calls", good("trellis_incoming_calls")),
            ("trellis_call_graph", good("trellis_call_graph")),
            ("trellis_find_symbols", good("trellis_find_symbols")),
            ("trellis_find_symbols", lookup),
            ("trellis_search_symbols", good("trellis_search_symbols")),
        ] {
            let mut valid = base;
            for compact in [json!(true), json!(false)] {
                valid["compact"] = compact;
                assert_eq!(validate(name, Some(&valid)), Ok(()), "{name}");
            }
            for compact in [Value::Null, json!(1), json!("true"), json!([true])] {
                valid["compact"] = compact;
                check(name, &valid, "invalid_request");
            }
        }
        for name in ["trellis_declaration", "trellis_read_source"] {
            let mut other = good(name);
            other["compact"] = json!(true);
            check(name, &other, "invalid_request");
        }
        // #665: outgoing calls also take a closed `includeExternal`; incoming does not.
        let name = "trellis_outgoing_calls";
        let mut valid = good(name);
        for include in [json!(true), json!(false)] {
            valid["includeExternal"] = include;
            assert_eq!(validate(name, Some(&valid)), Ok(()));
        }
        for include in [Value::Null, json!(1), json!("true")] {
            valid["includeExternal"] = include;
            check(name, &valid, "invalid_request");
        }
        let mut other = good("trellis_incoming_calls");
        other["includeExternal"] = json!(true);
        check("trellis_incoming_calls", &other, "invalid_request");
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
        // #725: check alone takes a nonempty, duplicate-free status filter.
        for status in [
            json!(["deviates"]),
            json!(["deviates", "unknown"]),
            json!(["conforms", "deviates", "unknown"]),
        ] {
            assert_eq!(
                validate(
                    "trellis_rules_check",
                    Some(&json!({"schemaVersion":1,"status":status}))
                ),
                Ok(())
            );
        }
        for status in [
            json!(null),
            json!("deviates"),
            json!([]),
            json!(["violates"]),
            json!(["deviates", "deviates"]),
            json!([1]),
        ] {
            assert_eq!(
                validate(
                    "trellis_rules_check",
                    Some(&json!({"schemaVersion":1,"status":status}))
                ),
                Err("invalid_request"),
                "{status}"
            );
        }
        for name in ["trellis_references", "trellis_rules_explain"] {
            let mut arguments = json!({"schemaVersion":1,"status":["deviates"]});
            if name == "trellis_rules_explain" {
                arguments["rule"] = json!("$layers");
                arguments["target"] = identity.clone();
            }
            assert_eq!(validate(name, Some(&arguments)), Err("invalid_request"));
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

    /// #818: each view takes its own target fields and nothing else.
    #[test]
    fn show_arguments_are_closed_per_view() {
        let n = "trellis_show";
        let ok = |v: Value| assert_eq!(validate(n, Some(&v)), Ok(()), "{v}");
        for view in ["sequence", "class"] {
            ok(json!({"schemaVersion":1,"view":view,"symbolId":sid()}));
            ok(
                json!({"schemaVersion":1,"view":view,"lookupKey":"src/a.rs::A","expectedBasis":pin()}),
            );
            for v in [
                json!({"schemaVersion":1,"view":view}),
                json!({"schemaVersion":1,"view":view,"symbolId":sid(),"lookupKey":"k"}),
                json!({"schemaVersion":1,"view":view,"symbolId":sid(),"line":1}),
                json!({"schemaVersion":1,"view":view,"symbolId":sid(),"path":"a.rs"}),
                json!({"schemaVersion":1,"view":view,"symbolId":"sid:v1:short"}),
                json!({"schemaVersion":1,"view":view,"lookupKey":""}),
            ] {
                check(n, &v, "invalid_request");
            }
        }
        ok(json!({"schemaVersion":1,"view":"source","path":"src/a.rs","line":1}));
        ok(
            json!({"schemaVersion":1,"view":"source","path":"src/a.rs","line":9007199254740991_u64}),
        );
        for v in [
            json!({"schemaVersion":1,"view":"source","path":"src/a.rs"}),
            json!({"schemaVersion":1,"view":"source","line":3}),
            json!({"schemaVersion":1,"view":"source","path":"src/a.rs","line":0}),
            json!({"schemaVersion":1,"view":"source","path":"src/a.rs","line":1.5}),
            json!({"schemaVersion":1,"view":"source","path":"/abs.rs","line":1}),
            json!({"schemaVersion":1,"view":"source","path":"../a.rs","line":1}),
            json!({"schemaVersion":1,"view":"source","path":"a.rs","line":1,"symbolId":sid()}),
            json!({"schemaVersion":1,"view":"source","path":"a.rs","line":1,"lookupKey":"k"}),
            json!({"schemaVersion":1,"view":"architecture","symbolId":sid()}),
            json!({"schemaVersion":1,"view":null,"symbolId":sid()}),
        ] {
            check(n, &v, "invalid_request");
        }
        let envelope = json!({"schemaVersion":1,"requestId":1,"truncated":false,
            "data":{"outcome":"no_viewer","view":"class","generation":null,"message":"m"}});
        assert_eq!(
            result(n, envelope, false)["content"][0]["text"],
            "trellis_show: no_viewer class; full result in structuredContent."
        );
    }

    /// #780: describe's `coverageDocuments` is an optional closed object; its bounds are
    /// typed, and a bad request never falls back to a plain describe.
    #[test]
    fn describe_coverage_documents_request_is_closed_and_bounded() {
        let n = "trellis_workspace_describe";
        let with = |request: Value| json!({"schemaVersion":1,"coverageDocuments":request});
        for request in [
            json!({}),
            json!({"language":"rust"}),
            json!({"language":"typescript","limit":50,"offset":9007199254740991_u64}),
            json!({"limit":1,"offset":0}),
        ] {
            assert_eq!(
                validate(n, Some(&with(request.clone()))),
                Ok(()),
                "{request}"
            );
        }
        for (request, code) in [
            (json!(null), "invalid_request"),
            (json!(true), "invalid_request"),
            (json!("rust"), "invalid_request"),
            (json!({"language":"go"}), "invalid_request"),
            (json!({"language":null}), "invalid_request"),
            (json!({"state":"failed"}), "invalid_request"),
            (json!({"limit":"5"}), "invalid_request"),
            (json!({"offset":-1}), "invalid_request"),
            (json!({"limit":0}), "range_too_large"),
            (json!({"limit":51}), "range_too_large"),
        ] {
            assert_eq!(
                validate(n, Some(&with(request.clone()))),
                Err(code),
                "{request}"
            );
        }
        assert!(
            super::check(n, Some(&with(json!({"language":"go"}))))
                .unwrap_err()
                .message
                .unwrap()
                .contains("coverageDocuments")
        );
    }

    /// #775: references `groupBy` is closed, and `depth` is bounded and only for
    /// `directory`. Check takes neither.
    #[test]
    fn references_group_by_is_closed_and_depth_needs_directory() {
        let n = "trellis_references";
        let with = |extra: Value| {
            let mut arguments = json!({"schemaVersion":1});
            for (key, value) in extra.as_object().unwrap() {
                arguments[key] = value.clone();
            }
            arguments
        };
        for ok in [
            json!({"groupBy":"file"}),
            json!({"groupBy":"directory"}),
            json!({"groupBy":"directory","depth":1}),
            json!({"groupBy":"directory","depth":32,"limit":50,"offset":3}),
            json!({"groupBy":"package","paths":["src/**"]}),
        ] {
            assert_eq!(validate(n, Some(&with(ok.clone()))), Ok(()), "{ok}");
        }
        for (bad, code) in [
            (json!({"groupBy":"module"}), "invalid_request"),
            (json!({"groupBy":null}), "invalid_request"),
            (json!({"groupBy":["directory"]}), "invalid_request"),
            (json!({"depth":2}), "invalid_request"),
            (json!({"groupBy":"file","depth":2}), "invalid_request"),
            (json!({"groupBy":"package","depth":2}), "invalid_request"),
            (
                json!({"groupBy":"directory","depth":"2"}),
                "invalid_request",
            ),
            (
                json!({"groupBy":"directory","depth":1.5}),
                "invalid_request",
            ),
            (json!({"groupBy":"directory","depth":0}), "range_too_large"),
            (json!({"groupBy":"directory","depth":33}), "range_too_large"),
        ] {
            assert_eq!(validate(n, Some(&with(bad.clone()))), Err(code), "{bad}");
        }
        assert_eq!(
            validate(
                "trellis_rules_check",
                Some(&json!({"schemaVersion":1,"groupBy":"directory"}))
            ),
            Err("invalid_request")
        );
    }

    /// #837: references `prefix` is a checkout-relative directory with every `groupBy`;
    /// anything else is a typed `invalid_request` that names it. Check takes no prefix.
    #[test]
    fn references_prefix_is_a_relative_directory() {
        let n = "trellis_references";
        let with = |extra: Value| {
            let mut arguments = json!({"schemaVersion":1});
            for (key, value) in extra.as_object().unwrap() {
                arguments[key] = value.clone();
            }
            arguments
        };
        let longest = "a/".repeat(2047) + "b";
        assert_eq!(longest.len(), 4095);
        for ok in [
            json!({"prefix":"src"}),
            json!({"prefix":"src/store/"}),
            json!({"prefix":"src/store","groupBy":"file"}),
            json!({"prefix":"src/","groupBy":"directory","depth":2}),
            json!({"prefix":"src/store","groupBy":"package","paths":["src/**"]}),
            json!({"prefix":"a b/$x.y"}),
            json!({"prefix":longest.clone() + "/"}),
        ] {
            assert_eq!(validate(n, Some(&with(ok.clone()))), Ok(()), "{ok}");
        }
        for bad in [
            json!(""),
            json!("/"),
            json!("/src"),
            json!("src//store"),
            json!("src/../etc"),
            json!(".."),
            json!("./src"),
            json!("."),
            json!("src\\store"),
            json!("c:/src"),
            json!("src\u{0}"),
            json!("src//"),
            json!(longest.clone() + "cc"),
            json!(null),
            json!(1),
            json!(["src"]),
        ] {
            let rejection =
                super::check(n, Some(&with(json!({"prefix":bad.clone()})))).unwrap_err();
            assert_eq!(rejection.code, "invalid_request", "{bad}");
            assert!(rejection.message.unwrap().contains("`prefix`"), "{bad}");
        }
        assert_eq!(
            validate(
                "trellis_rules_check",
                Some(&json!({"schemaVersion":1,"prefix":"src"}))
            ),
            Err("invalid_request")
        );
    }
}
