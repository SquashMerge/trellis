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
type Args = serde_json::Map<String, Value>;
/// How a successful result is counted in its text summary: the count and its plural noun.
enum Summary {
    /// Always one item.
    One(&'static str),
    /// The summed lengths of arrays under `data`, each a path and what it holds.
    Count(
        &'static [(&'static [&'static str], &'static str)],
        &'static str,
    ),
    /// A count computed from the envelope.
    Custom(fn(&Value) -> usize, &'static str),
}
impl Summary {
    fn count(&self, envelope: &Value) -> (usize, &'static str) {
        match self {
            Summary::One(noun) => (1, noun),
            Summary::Count(arrays, noun) => (
                arrays
                    .iter()
                    .map(|(path, what)| {
                        path.iter()
                            .fold(&envelope["data"], |value, key| &value[*key])
                            .as_array()
                            .expect(what)
                            .len()
                    })
                    .sum(),
                noun,
            ),
            Summary::Custom(count, noun) => (count(envelope), noun),
        }
    }
}
fn source_lines(envelope: &Value) -> usize {
    (envelope["data"]["endLine"]
        .as_u64()
        .expect("source end line")
        - envelope["data"]["startLine"]
            .as_u64()
            .expect("source start line")
        + 1) as usize
}
/// One known tool: its accepted and required argument fields, the validator of its own
/// fields, and how its result is summarized.
struct ToolSpec {
    name: &'static str,
    fields: &'static [&'static str],
    required: &'static [&'static str],
    validate: fn(&Args, &Reject) -> Result<(), Rejection>,
    summary: Summary,
}
/// Every tool of the catalog, in catalog order.
const SPECS: [ToolSpec; TOOLS.len()] = [
    ToolSpec {
        name: "trellis_workspace_describe",
        fields: &["schemaVersion", "workspace", "coverageDocuments"],
        required: &["schemaVersion"],
        validate: describe_args,
        summary: Summary::One("workspace"),
    },
    ToolSpec {
        name: "trellis_find_symbols",
        fields: &[
            "schemaVersion",
            "query",
            "lookupKey",
            "limit",
            "compact",
            "expectedBasis",
            "workspace",
            "app",
        ],
        required: &["schemaVersion"],
        validate: find_args,
        summary: Summary::Count(&[(&["symbols"], "find symbols")], "symbols"),
    },
    ToolSpec {
        name: "trellis_search_symbols",
        fields: &[
            "schemaVersion",
            "query",
            "limit",
            "role",
            "compact",
            "expectedBasis",
            "workspace",
            "app",
        ],
        required: &["schemaVersion", "query"],
        validate: search_args,
        summary: Summary::Count(&[(&["hits"], "search hits")], "hits"),
    },
    ToolSpec {
        name: "trellis_declaration",
        fields: &[
            "schemaVersion",
            "symbolId",
            "lookupKey",
            "expectedBasis",
            "workspace",
        ],
        required: &["schemaVersion"],
        validate: declaration_args,
        summary: Summary::One("declaration"),
    },
    ToolSpec {
        name: "trellis_outgoing_calls",
        fields: &[
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
        required: &["schemaVersion"],
        validate: calls_args,
        summary: Summary::Count(&[(&["calls"], "outgoing calls")], "calls"),
    },
    ToolSpec {
        name: "trellis_incoming_calls",
        fields: &[
            "schemaVersion",
            "symbolId",
            "lookupKey",
            "limit",
            "semanticProducerId",
            "compact",
            "expectedBasis",
            "workspace",
        ],
        required: &["schemaVersion"],
        validate: calls_args,
        summary: Summary::Count(&[(&["callers"], "incoming callers")], "calls"),
    },
    ToolSpec {
        name: "trellis_read_source",
        fields: &[
            "schemaVersion",
            "path",
            "startLine",
            "endLine",
            "expectedBasis",
            "expectedContentHash",
            "workspace",
        ],
        required: &["schemaVersion", "path", "startLine", "endLine"],
        validate: source_args,
        summary: Summary::Custom(source_lines, "lines"),
    },
    ToolSpec {
        name: "trellis_call_graph",
        fields: &[
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
        required: &["schemaVersion"],
        validate: call_graph_args,
        summary: Summary::Count(&[(&["result", "edges"], "call graph edges")], "edges"),
    },
    ToolSpec {
        name: "trellis_references",
        fields: &[
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
        required: &["schemaVersion"],
        validate: reference_args,
        summary: Summary::Count(
            &[
                (&["production"], "production edges"),
                (&["tests"], "test edges"),
            ],
            "edges",
        ),
    },
    ToolSpec {
        name: "trellis_rules_check",
        fields: &[
            "schemaVersion",
            "workspace",
            "expectedBasis",
            "paths",
            "limit",
            "offset",
            "status",
            "app",
        ],
        required: &["schemaVersion"],
        validate: reference_args,
        summary: Summary::Count(&[(&["instances"], "rule instances")], "instances"),
    },
    ToolSpec {
        name: "trellis_rules_explain",
        fields: &[
            "schemaVersion",
            "workspace",
            "expectedBasis",
            "rule",
            "target",
            "destination",
            "limit",
            "offset",
        ],
        required: &["schemaVersion", "rule", "target"],
        validate: explain_args,
        summary: Summary::Count(&[(&["instances"], "rule instances")], "instances"),
    },
    ToolSpec {
        name: "trellis_show",
        fields: &[
            "schemaVersion",
            "view",
            "symbolId",
            "lookupKey",
            "path",
            "line",
            "expectedBasis",
            "workspace",
        ],
        required: &["schemaVersion", "view"],
        validate: show_args,
        // Its summary names the outcome instead (`show::summary`).
        summary: Summary::Count(&[], "targets"),
    },
    ToolSpec {
        name: "trellis_endpoints",
        fields: &[
            "schemaVersion",
            "workspace",
            "expectedBasis",
            "app",
            "method",
            "pathTemplate",
            "limit",
            "offset",
        ],
        required: &["schemaVersion"],
        validate: endpoint_args,
        summary: Summary::Count(&[(&["endpoints"], "endpoint rows")], "endpoints"),
    },
];
fn spec(name: &str) -> Option<&'static ToolSpec> {
    SPECS.iter().find(|spec| spec.name == name)
}
/// The accepted and required argument fields of a known tool.
#[cfg(test)]
fn fields(name: &str) -> (&'static [&'static str], &'static [&'static str]) {
    let spec = spec(name).expect("known tool name");
    (spec.fields, spec.required)
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
/// The tool and arguments under validation, for building its rejections.
struct Reject<'a> {
    name: &'a str,
    args: Option<&'a Value>,
}
impl Reject<'_> {
    /// An `invalid_request` names its field.
    fn bad(&self, field: &str) -> Rejection {
        Rejection {
            code: "invalid_request",
            message: Some(field_message(self.name, field)),
        }
    }
    /// An object field that arrived as a string says so (#769).
    fn object(&self, field: &'static str, stringified: &'static str) -> Rejection {
        if string_field(self.args, field) {
            self.bad(stringified)
        } else {
            self.bad(field)
        }
    }
    /// An `invalid_request` names its field; another code keeps its own message.
    fn coded(&self, field: &'static str, code: &'static str) -> Rejection {
        if code == "invalid_request" {
            self.bad(field)
        } else {
            Rejection {
                code,
                message: None,
            }
        }
    }
}
/// [`validate`], with the failing field's message (#769). The order is the contract:
/// unknown field, missing field, the shared fields, the tool's own, then `workspace`.
pub fn check(name: &str, args: Option<&Value>) -> Result<(), Rejection> {
    let Some(spec) = spec(name) else {
        return Err(Rejection {
            code: "invalid_request",
            message: None,
        });
    };
    let r = Reject { name, args };
    let Some(o) = args.and_then(Value::as_object) else {
        return Err(r.bad("arguments"));
    };
    // An unknown field fails even once a missing one is added, so it is named first.
    if !o.keys().all(|key| spec.fields.contains(&key.as_str())) {
        return Err(r.bad(""));
    }
    if let Some(missing) = spec.required.iter().find(|field| !o.contains_key(**field)) {
        return Err(r.bad(missing));
    }
    common_fields(o, &r)?;
    (spec.validate)(o, &r)?;
    if workspace_field(o).is_err() {
        return Err(r.bad("workspace"));
    }
    Ok(())
}
/// The fields that mean the same wherever a tool accepts them.
fn common_fields(o: &Args, r: &Reject) -> Result<(), Rejection> {
    if o.get("schemaVersion").and_then(wire::uint) != Some(1) {
        return Err(r.bad("schemaVersion"));
    }
    if !expected_basis(o) {
        return Err(r.object("expectedBasis", "expectedBasis:string"));
    }
    // `compact` (#528, #665, #715) is a closed boolean wherever a tool accepts it.
    if o.get("compact").is_some_and(|value| !value.is_boolean()) {
        return Err(r.bad("compact"));
    }
    // #851: `app` is one nonempty string wherever a tool accepts it.
    if !o.get("app").is_none_or(|app| bounded_text(Some(app))) {
        return Err(r.bad("app"));
    }
    Ok(())
}
/// `limit`, then `offset`.
fn page(o: &Args, r: &Reject) -> Result<(), Rejection> {
    limit(o).map_err(|code| r.coded("limit", code))?;
    offset(o).map_err(|code| r.coded("offset", code))
}
fn describe_args(o: &Args, r: &Reject) -> Result<(), Rejection> {
    let Some(request) = o.get("coverageDocuments") else {
        return Ok(());
    };
    let request = request
        .as_object()
        .ok_or_else(|| r.bad("coverageDocuments"))?;
    if !request
        .keys()
        .all(|key| matches!(key.as_str(), "language" | "limit" | "offset"))
        || !request.get("language").is_none_or(|language| {
            language.as_str().is_some_and(|language| {
                crate::semantic_evidence::LANGUAGE_ORDER.contains(&language)
            })
        })
    {
        return Err(r.bad("coverageDocuments"));
    }
    limit(request).map_err(|code| r.coded("coverageDocuments", code))?;
    offset(request).map_err(|code| r.coded("coverageDocuments", code))
}
/// `trellis_references` and `trellis_rules_check`: each accepts a subset of these fields.
fn reference_args(o: &Args, r: &Reject) -> Result<(), Rejection> {
    group_by(o).map_err(|code| r.coded("groupBy", code))?;
    // #837: a checkout-relative directory, normalized but for one trailing `/`.
    if !o.get("prefix").is_none_or(|prefix| {
        prefix
            .as_str()
            .and_then(crate::references::directory_prefix)
            .is_some()
    }) {
        return Err(r.bad("prefix"));
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
        return Err(r.bad("paths"));
    }
    if !status_filter(o) {
        return Err(r.bad("status"));
    }
    page(o, r)
}
fn endpoint_args(o: &Args, r: &Reject) -> Result<(), Rejection> {
    if !o.get("method").is_none_or(|method| {
        string(Some(method)).is_some_and(|method| {
            matches!(
                method,
                "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS"
            )
        })
    }) {
        return Err(r.bad("method"));
    }
    if !o
        .get("pathTemplate")
        .is_none_or(|template| bounded_text(Some(template)))
    {
        return Err(r.bad("pathTemplate"));
    }
    page(o, r)
}
fn explain_args(o: &Args, r: &Reject) -> Result<(), Rejection> {
    let rule = string(o.get("rule")).ok_or_else(|| r.bad("rule"))?;
    if rule.is_empty() || rule.len() > 256 {
        return Err(r.bad("rule"));
    }
    if !o
        .get("target")
        .is_some_and(super::architecture::valid_identity)
    {
        return Err(r.object("target", "target:string"));
    }
    if !o
        .get("destination")
        .is_none_or(super::architecture::valid_destination)
        || (!matches!(rule, "$layers" | "$external") && o.contains_key("destination"))
    {
        return Err(r.object("destination", "destination:string"));
    }
    page(o, r)
}
/// A present `query` of 1 to `max_bytes` bytes.
fn query(o: &Args, r: &Reject, max_bytes: usize) -> Result<(), Rejection> {
    let q = string(o.get("query")).ok_or_else(|| r.bad("query"))?;
    if q.is_empty() || q.len() > max_bytes {
        return Err(r.bad("query"));
    }
    Ok(())
}
fn find_args(o: &Args, r: &Reject) -> Result<(), Rejection> {
    if o.contains_key("lookupKey") {
        if o.contains_key("query") {
            return Err(r.bad("query|lookupKey"));
        }
        if !valid_lookup_key(o.get("lookupKey")) {
            return Err(r.bad("lookupKey"));
        }
    } else {
        query(o, r, 256)?;
    }
    limit(o).map_err(|code| r.coded("limit", code))
}
fn search_args(o: &Args, r: &Reject) -> Result<(), Rejection> {
    query(o, r, 1024)?;
    if !o
        .get("role")
        .is_none_or(|role| matches!(role.as_str(), Some("any" | "production" | "test")))
    {
        return Err(r.bad("role"));
    }
    limit(o).map_err(|code| r.coded("limit", code))
}
fn show_args(o: &Args, r: &Reject) -> Result<(), Rejection> {
    match string(o.get("view")) {
        Some("sequence" | "class") => {
            if o.contains_key("path")
                || o.contains_key("line")
                || o.contains_key("symbolId") == o.contains_key("lookupKey")
            {
                return Err(r.bad("view:symbol"));
            }
            selector(o).map_err(|field| r.bad(field))
        }
        Some("source") => {
            if o.contains_key("symbolId") || o.contains_key("lookupKey") {
                return Err(r.bad("view:source"));
            }
            let path = string(o.get("path")).ok_or_else(|| r.bad("path"))?;
            if !valid_path(path) {
                return Err(r.bad("path"));
            }
            if !o
                .get("line")
                .and_then(wire::uint)
                .is_some_and(|line| line >= 1)
            {
                return Err(r.bad("line"));
            }
            Ok(())
        }
        _ => Err(r.bad("view")),
    }
}
/// Exactly one of `symbolId` and `lookupKey`, and that one valid.
fn one_selector(o: &Args, r: &Reject) -> Result<(), Rejection> {
    if o.contains_key("symbolId") == o.contains_key("lookupKey") {
        return Err(r.bad("symbolId|lookupKey"));
    }
    selector(o).map_err(|field| r.bad(field))
}
fn declaration_args(o: &Args, r: &Reject) -> Result<(), Rejection> {
    one_selector(o, r)
}
/// `trellis_outgoing_calls` and `trellis_incoming_calls`.
fn calls_args(o: &Args, r: &Reject) -> Result<(), Rejection> {
    one_selector(o, r)?;
    semantic_producer(o).map_err(|code| r.coded("semanticProducerId", code))?;
    if let Some(field) = ["compact", "includeExternal"]
        .into_iter()
        .find(|field| o.get(*field).is_some_and(|value| !value.is_boolean()))
    {
        return Err(r.bad(field));
    }
    limit(o).map_err(|code| r.coded("limit", code))
}
fn call_graph_args(o: &Args, r: &Reject) -> Result<(), Rejection> {
    one_selector(o, r)?;
    call_graph_arguments(o).map_err(|field| r.bad(field))
}
fn source_args(o: &Args, r: &Reject) -> Result<(), Rejection> {
    let path = string(o.get("path")).ok_or_else(|| r.bad("path"))?;
    if !valid_path(path) {
        return Err(r.bad("path"));
    }
    if !o
        .get("expectedContentHash")
        .is_none_or(|v| string(Some(v)).is_some_and(hash))
    {
        return Err(r.bad("expectedContentHash"));
    }
    let start = o
        .get("startLine")
        .and_then(wire::uint)
        .ok_or_else(|| r.bad("startLine"))?;
    let end = o
        .get("endLine")
        .and_then(wire::uint)
        .ok_or_else(|| r.bad("endLine"))?;
    if start == 0 || end == 0 || end < start {
        return Err(r.bad("endLine"));
    }
    if end - start >= 200 {
        return Err(Rejection {
            code: "range_too_large",
            message: None,
        });
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
            spec(name)
                .expect("known tool name")
                .summary
                .count(&envelope)
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
#[path = "tools_tests.rs"]
mod tests;
