//! Pure connection reducer. An ID is a lookup key, never the authority to finish work.
use super::wire::{self, IdKey, ProtocolError, Request};
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Unselected,
    Modern,
    LegacyAwaitInitialized,
    LegacyReady,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Discover,
    Initialize,
    List,
    PromptList,
    Call {
        name: String,
        arguments: Option<Value>,
    },
    Unknown,
}
#[derive(Clone, Debug)]
pub struct Admission {
    pub token: u64,
    pub id: Value,
    pub action: Action,
}
#[derive(Clone, Debug)]
pub struct Prepared {
    pub token: u64,
    pub response: Value,
}
#[derive(Debug)]
pub enum Event {
    Ignore,
    Error(ProtocolError),
    Admitted(Admission),
}
pub struct Session {
    mode: Mode,
    next_token: u64,
    pending: HashMap<IdKey, Admission>,
}
impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
impl Session {
    pub fn new() -> Self {
        Self {
            mode: Mode::Unselected,
            next_token: 0,
            pending: HashMap::new(),
        }
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
    pub fn pending(&self, token: u64) -> Option<&Admission> {
        self.pending.values().find(|a| a.token == token)
    }
    pub fn eof(&mut self) {
        self.pending.clear();
    }
    pub fn cancel(&mut self, id: &Value) {
        if let Some(key) = key_for(id) {
            self.pending.remove(&key);
        }
    }
    /// Validate first. Only a fully admitted request can select a mode.
    pub fn accept(&mut self, request: Request, is_known_tool: impl Fn(&str) -> bool) -> Event {
        let Request {
            id,
            key,
            method,
            params,
        } = request;
        if key.is_none() {
            if method == "notifications/cancelled" {
                if let Some(p) = params.as_ref().and_then(Value::as_object)
                    && p.keys().all(|k| k == "requestId" || k == "reason")
                    && p.get("reason").is_none_or(Value::is_string)
                    && let Some(value) = p.get("requestId")
                {
                    self.cancel(value);
                }
            } else if method == "notifications/initialized"
                && self.mode == Mode::LegacyAwaitInitialized
                && params
                    .as_ref()
                    .is_none_or(|p| p.as_object().is_some_and(|o| o.is_empty()))
            {
                self.mode = Mode::LegacyReady;
            }
            return Event::Ignore;
        }
        let id = id.expect("key implies ID");
        let key = key.expect("key implies ID");
        if self.pending.contains_key(&key) {
            return Event::Error(ProtocolError::new(-32600, id));
        }
        let (target, action) = match self.validate(&method, params.as_ref(), &id, &is_known_tool) {
            Ok(v) => v,
            Err(err) => return Event::Error(err),
        };
        let Some(token) = self.next_token.checked_add(1) else {
            return Event::Error(ProtocolError::new(-32603, id));
        };
        self.next_token = token;
        self.mode = target;
        let admission = Admission { token, id, action };
        self.pending.insert(key, admission.clone());
        Event::Admitted(admission)
    }
    fn validate(
        &self,
        method: &str,
        params: Option<&Value>,
        id: &Value,
        known: &impl Fn(&str) -> bool,
    ) -> Result<(Mode, Action), ProtocolError> {
        let bad = |code| ProtocolError::new(code, id.clone());
        if method == "initialize" {
            if self.mode != Mode::Unselected {
                return Err(bad(-32600));
            }
            if !legacy_initialize(params) {
                return Err(bad(-32602));
            }
            return Ok((Mode::LegacyAwaitInitialized, Action::Initialize));
        }
        if self.mode == Mode::LegacyAwaitInitialized {
            return Err(bad(-32600));
        }
        let modern = self.mode != Mode::LegacyReady;
        if modern {
            let version = modern_metadata(params).map_err(|reason| match reason {
                Some(requested) => {
                    let mut err = bad(-32022);
                    err.data = Some(
                        json!({"supported":["2026-07-28","2025-11-25"],"requested":requested}),
                    );
                    err
                }
                None => bad(-32602),
            })?;
            debug_assert!(version);
        } else if method == "server/discover"
            || params
                .and_then(Value::as_object)
                .is_some_and(|p| p.contains_key("_meta"))
        {
            return Err(bad(-32600));
        }
        let mode = if modern {
            Mode::Modern
        } else {
            Mode::LegacyReady
        };
        let obj = params.and_then(Value::as_object);
        let action = match method {
            "server/discover" if modern && obj.is_some_and(|p| only(p, &["_meta"])) => {
                Action::Discover
            }
            "tools/list" if modern && obj.is_some_and(|p| only(p, &["_meta"])) => Action::List,
            "tools/list"
                if !modern
                    && params.is_none_or(|p| p.as_object().is_some_and(|o| o.is_empty())) =>
            {
                Action::List
            }
            "prompts/list" if modern && obj.is_some_and(|p| only(p, &["_meta"])) => {
                Action::PromptList
            }
            "prompts/list"
                if !modern
                    && params.is_none_or(|p| p.as_object().is_some_and(|o| o.is_empty())) =>
            {
                Action::PromptList
            }
            "prompts/get" => {
                let p = obj.ok_or_else(|| bad(-32602))?;
                let allowed = if modern {
                    &["name", "arguments", "_meta"][..]
                } else {
                    &["name", "arguments"][..]
                };
                if !only(p, allowed)
                    || p.get("name").and_then(Value::as_str)
                        != Some(super::architecture::PROMPT_NAME)
                    || !p
                        .get("arguments")
                        .is_none_or(|value| value.as_object().is_some_and(|args| args.is_empty()))
                {
                    return Err(bad(-32602));
                }
                Action::Call {
                    name: super::architecture::PROMPT_NAME.to_owned(),
                    arguments: Some(json!({"schemaVersion":1})),
                }
            }
            "tools/call" => {
                let p = obj.ok_or_else(|| bad(-32602))?;
                let allowed = if modern {
                    &["name", "arguments", "_meta"][..]
                } else {
                    &["name", "arguments"][..]
                };
                if !only(p, allowed) {
                    return Err(bad(-32602));
                }
                let name = p
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| bad(-32602))?;
                if !known(name) {
                    return Err(bad(-32602));
                }
                Action::Call {
                    name: name.to_owned(),
                    arguments: p.get("arguments").cloned(),
                }
            }
            "server/discover" | "tools/list" | "prompts/list" => return Err(bad(-32602)),
            _ => Action::Unknown,
        };
        Ok((mode, action))
    }
    /// Prepare checks the immutable admission token, never merely the reused ID.
    pub fn prepare(&self, token: u64, response: Value) -> Option<Prepared> {
        self.pending(token).map(|_| Prepared { token, response })
    }
    /// A stale completion cannot remove or alter a newer request sharing its ID.
    pub fn commit(&mut self, prepared: Prepared) -> Option<Value> {
        let key = self
            .pending
            .iter()
            .find_map(|(key, entry)| (entry.token == prepared.token).then(|| key.clone()))?;
        self.pending.remove(&key);
        Some(prepared.response)
    }
}
fn key_for(value: &Value) -> Option<IdKey> {
    match value {
        Value::String(s) if wire::compact_string_token_len(s) <= 256 => {
            Some(IdKey::String(s.clone()))
        }
        Value::Number(n) => wire::exact_safe_integer(&n.to_string()).map(IdKey::Integer),
        _ => None,
    }
}
fn only(p: &serde_json::Map<String, Value>, keys: &[&str]) -> bool {
    p.keys().all(|k| keys.contains(&k.as_str()))
}
fn object(v: &Value) -> bool {
    v.is_object()
}
type FieldValidator = (&'static str, fn(&Value) -> bool);
fn recognized(obj: &serde_json::Map<String, Value>, keys: &[FieldValidator]) -> bool {
    obj.iter().all(|(k, v)| {
        keys.iter()
            .find(|(name, _)| k == name)
            .is_none_or(|(_, check)| check(v))
    })
}
fn object_with(v: &Value, keys: &[FieldValidator]) -> bool {
    v.as_object().is_some_and(|o| recognized(o, keys))
}
fn object_values(v: &Value) -> bool {
    v.as_object().is_some_and(|o| o.values().all(object))
}
fn roots_legacy(v: &Value) -> bool {
    object_with(v, &[("listChanged", Value::is_boolean)])
}
fn sampling(v: &Value) -> bool {
    object_with(v, &[("context", object), ("tools", object)])
}
fn elicitation(v: &Value) -> bool {
    object_with(v, &[("form", object), ("url", object)])
}
fn tasks(v: &Value) -> bool {
    object_with(
        v,
        &[
            ("cancel", object),
            ("list", object),
            ("requests", task_requests),
        ],
    )
}
fn task_requests(v: &Value) -> bool {
    object_with(
        v,
        &[
            ("elicitation", task_elicitation),
            ("sampling", task_sampling),
        ],
    )
}
fn task_elicitation(v: &Value) -> bool {
    object_with(v, &[("create", object)])
}
fn task_sampling(v: &Value) -> bool {
    object_with(v, &[("createMessage", object)])
}
fn capabilities(v: &Value, modern: bool) -> bool {
    let Some(o) = v.as_object() else { return false };
    let mut fields: Vec<FieldValidator> = vec![
        ("elicitation", elicitation),
        ("experimental", object_values),
        ("sampling", sampling),
    ];
    if modern {
        fields.push(("roots", object));
        fields.push(("extensions", object_values));
    } else {
        fields.push(("roots", roots_legacy));
        fields.push(("tasks", tasks));
    }
    recognized(o, &fields)
}
fn uri(v: &Value) -> bool {
    v.as_str().is_some_and(|s| reqwest::Url::parse(s).is_ok())
}
fn str_array(v: &Value) -> bool {
    v.as_array().is_some_and(|a| a.iter().all(Value::is_string))
}
fn icon(v: &Value) -> bool {
    v.as_object().is_some_and(|o| {
        o.get("src").is_some_and(uri)
            && recognized(
                o,
                &[
                    ("src", uri),
                    ("mimeType", Value::is_string),
                    ("sizes", str_array),
                    ("theme", theme),
                ],
            )
    })
}
fn theme(v: &Value) -> bool {
    matches!(v.as_str(), Some("dark" | "light"))
}
fn icons(v: &Value) -> bool {
    v.as_array().is_some_and(|a| a.iter().all(icon))
}
fn client_info(v: &Value) -> bool {
    v.as_object().is_some_and(|o| {
        o.get("name").is_some_and(Value::is_string)
            && o.get("version").is_some_and(Value::is_string)
            && recognized(
                o,
                &[
                    ("name", Value::is_string),
                    ("version", Value::is_string),
                    ("title", Value::is_string),
                    ("description", Value::is_string),
                    ("websiteUrl", uri),
                    ("icons", icons),
                ],
            )
    })
}
fn legacy_initialize(params: Option<&Value>) -> bool {
    params.and_then(Value::as_object).is_some_and(|o| {
        o.get("protocolVersion").is_some_and(Value::is_string)
            && o.get("capabilities")
                .is_some_and(|v| capabilities(v, false))
            && o.get("clientInfo").is_some_and(client_info)
            && recognized(
                o,
                &[
                    ("protocolVersion", Value::is_string),
                    ("capabilities", |v| capabilities(v, false)),
                    ("clientInfo", client_info),
                    ("_meta", legacy_meta),
                ],
            )
    })
}
fn legacy_meta(v: &Value) -> bool {
    object_with(
        v,
        &[("progressToken", |v| {
            v.is_string()
                || v.as_number()
                    .is_some_and(|n| wire::exact_json_integer(&n.to_string()))
        })],
    )
}
/// Err(Some(version)) is reserved for a well-typed but unsupported revision.
fn modern_metadata(params: Option<&Value>) -> Result<bool, Option<String>> {
    let meta = params
        .and_then(Value::as_object)
        .and_then(|o| o.get("_meta"))
        .and_then(Value::as_object)
        .ok_or(None)?;
    let version = meta
        .get("io.modelcontextprotocol/protocolVersion")
        .and_then(Value::as_str)
        .ok_or(None)?;
    if !meta
        .get("io.modelcontextprotocol/clientCapabilities")
        .is_some_and(|v| capabilities(v, true))
        || !meta
            .get("io.modelcontextprotocol/clientInfo")
            .is_none_or(client_info)
    {
        return Err(None);
    }
    if version != "2026-07-28" {
        return Err(Some(version.to_owned()));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn req(id: &str, mode: Mode, method: &str, extra: &str) -> Request {
        let meta = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#;
        let params = if mode == Mode::Modern || mode == Mode::Unselected {
            if method == "tools/call" {
                format!("{{{meta}{extra}}}")
            } else {
                let mut inner = meta.to_owned();
                inner.pop();
                format!("{{{inner}{extra}}}}}")
            }
        } else {
            format!("{{{extra}}}")
        };
        let s = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#);
        wire::decode(wire::Frame::Line(s.into_bytes()))
            .unwrap()
            .unwrap()
    }
    fn admitted(event: Event) -> Admission {
        match event {
            Event::Admitted(a) => a,
            e => panic!("expected admission: {e:?}"),
        }
    }
    fn rejected(event: Event, code: i32, id: Value) {
        match event {
            Event::Error(e) => {
                assert_eq!(e.code, code);
                assert_eq!(e.id, id)
            }
            e => panic!("expected error: {e:?}"),
        }
    }
    #[test]
    fn legacy_progress_token_integer_boundary_and_recovery() {
        let mut s = Session::new();
        let invalid = req(
            "1",
            Mode::LegacyReady,
            "initialize",
            r#""protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"x","version":"1"},"_meta":{"progressToken":9007199254740992.5}"#,
        );
        rejected(s.accept(invalid, |_| false), -32602, json!(1));
        assert_eq!(s.mode(), Mode::Unselected);
        assert_eq!(s.pending_count(), 0);
        for token in [
            "9007199254740992",
            "1000000000000000000000000000000000",
            "9007199254740992.0",
        ] {
            let value = req(
                "1",
                Mode::LegacyReady,
                "initialize",
                &format!(
                    r#""protocolVersion":"2025-11-25","capabilities":{{}},"clientInfo":{{"name":"x","version":"1"}},"_meta":{{"progressToken":{token}}}"#
                ),
            );
            let admitted = admitted(s.accept(value, |_| false));
            assert_eq!(admitted.id, json!(1));
            assert_eq!(admitted.action, Action::Initialize);
            assert_eq!(s.mode(), Mode::LegacyAwaitInitialized);
            let ready = s.prepare(admitted.token, json!({"ready":true})).unwrap();
            assert_eq!(s.commit(ready), Some(json!({"ready":true})));
            s = Session::new();
        }
        let rejected_id = r#"{"jsonrpc":"2.0","id":9007199254740992,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"x","version":"1"}}}"#.to_string();
        let err = wire::decode(wire::Frame::Line(rejected_id.into_bytes())).unwrap_err();
        assert_eq!(err.code, -32600);
        assert_eq!(err.id, Value::Null);
        assert_eq!(wire::uint(&json!(9007199254740992_u64)), None);
    }
    #[test]
    fn client_info_title_validation() {
        let mut s = Session::new();
        let invalid = req(
            "7",
            Mode::Unselected,
            "server/discover",
            r#","io.modelcontextprotocol/clientInfo":{"name":"x","version":"1","title":42}"#,
        );
        rejected(s.accept(invalid, |_| false), -32602, json!(7));
        assert_eq!(s.mode(), Mode::Unselected);
        assert_eq!(s.pending_count(), 0);
        let valid = req(
            "8",
            Mode::Unselected,
            "server/discover",
            r#","io.modelcontextprotocol/clientInfo":{"name":"x","version":"1","title":"Valid","futureField":true}"#,
        );
        admitted(s.accept(valid, |_| false));
        assert_eq!(s.mode(), Mode::Modern);
        let mut l = Session::new();
        let invalid = req(
            "7",
            Mode::LegacyReady,
            "initialize",
            r#""protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"x","version":"1","title":42}"#,
        );
        rejected(l.accept(invalid, |_| false), -32602, json!(7));
        assert_eq!(l.mode(), Mode::Unselected);
        assert_eq!(l.pending_count(), 0);
        let valid = req(
            "8",
            Mode::LegacyReady,
            "initialize",
            r#""protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"x","version":"1","title":"Valid","futureField":true}"#,
        );
        admitted(l.accept(valid, |_| false));
        assert_eq!(l.mode(), Mode::LegacyAwaitInitialized);
    }
    #[test]
    fn oversized_id_does_not_select_or_register() {
        for legacy in [false, true] {
            let mut s = Session::new();
            let request = |id: &str| {
                if legacy {
                    req(
                        id,
                        Mode::LegacyReady,
                        "initialize",
                        r#""protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"x","version":"1"}"#,
                    )
                } else {
                    req(id, Mode::Unselected, "server/discover", "")
                }
            };
            let attempt = |id: &str| {
                let mut value = request("1");
                value.id = Some(json!(id));
                let frame = format!(
                    r#"{{"jsonrpc":"2.0","id":{},"method":"{}","params":{}}}"#,
                    serde_json::to_string(id).unwrap(),
                    value.method,
                    value.params.unwrap()
                );
                wire::decode(wire::Frame::Line(frame.into_bytes()))
            };
            let accepted = "a".repeat(254);
            let oversized = "a".repeat(255);
            assert_eq!(wire::compact_string_token_len(&accepted), 256);
            assert_eq!(wire::compact_string_token_len(&oversized), 257);
            let error = attempt(&oversized).unwrap_err();
            assert_eq!((error.code, error.id), (-32600, Value::Null));
            assert_eq!(s.mode(), Mode::Unselected);
            assert_eq!(s.pending_count(), 0);
            let admitted = admitted(s.accept(attempt(&accepted).unwrap().unwrap(), |_| false));
            assert_eq!(admitted.id, json!(accepted));
            assert_eq!(s.pending_count(), 1);
            assert_eq!(
                s.mode(),
                if legacy {
                    Mode::LegacyAwaitInitialized
                } else {
                    Mode::Modern
                }
            );
            let error = attempt(&oversized).unwrap_err();
            assert_eq!((error.code, error.id), (-32600, Value::Null));
            assert_eq!(s.pending_count(), 1);
            assert_eq!(s.pending(admitted.token).unwrap().id, admitted.id);
        }
    }
    #[test]
    fn normalized_pending_aliases() {
        for (a, b, id) in [
            (r#""a""#, r#""\u0061""#, json!("a")),
            ("1", "1e0", json!(1)),
        ] {
            let mut s = Session::new();
            let first =
                admitted(s.accept(req(a, Mode::Unselected, "server/discover", ""), |_| false));
            rejected(
                s.accept(req(b, Mode::Modern, "tools/list", ""), |_| false),
                -32600,
                id,
            );
            assert_eq!(s.pending(first.token).unwrap().id, first.id);
        }
    }
    /// Synthetic reducer fixture with a short text summary and separate structured envelope.
    fn tool_response(id: &Value, tag: &str, is_error: bool, modern: bool) -> Value {
        let envelope = if is_error {
            json!({"schemaVersion":1,"requestId":id,"error":{"code":"index_not_ready","message":"Index is not ready","retryable":true,"currentBasis":null,"currentContentHash":null}})
        } else {
            json!({"schemaVersion":1,"requestId":id,"evidenceBasis":null,"data":{"tag":tag},"warnings":[],"partial":true,"truncated":false,"truncationReason":null})
        };
        let text = if is_error {
            "probe failed: index_not_ready: Index is not ready"
        } else {
            "probe: 1 item; full result in structuredContent."
        };
        let mut result = json!({"isError":is_error,"structuredContent":envelope,"content":[{"type":"text","text":text}]});
        if modern {
            result["resultType"] = json!("complete");
        }
        json!({"jsonrpc":"2.0","id":id,"result":result})
    }
    #[test]
    fn cancel_reuse_stale_completion() {
        for legacy in [false, true] {
            for (old, new) in [(r#""a""#, r#""\u0061""#), ("1", "1e0")] {
                for old_is_error in [false, true] {
                    let mut s = Session::new();
                    let mode = if legacy {
                        let init=admitted(s.accept(req("99",Mode::LegacyReady,"initialize",r#""protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"x","version":"1"}"#), |_|false));
                        let init_result =
                            s.prepare(init.token, json!({"initialized":true})).unwrap();
                        assert!(s.commit(init_result).is_some());
                        let notification = wire::decode(wire::Frame::Line(
                            br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_vec(),
                        ))
                        .unwrap()
                        .unwrap();
                        assert!(matches!(s.accept(notification, |_| false), Event::Ignore));
                        assert_eq!(s.mode(), Mode::LegacyReady);
                        Mode::LegacyReady
                    } else {
                        Mode::Unselected
                    };
                    let arguments = |tag: &str| {
                        if legacy {
                            format!(r#""name":"probe","arguments":{{"tag":"{tag}"}}"#)
                        } else {
                            format!(r#", "name":"probe","arguments":{{"tag":"{tag}"}}"#)
                        }
                    };
                    let a = admitted(
                        s.accept(req(old, mode, "tools/call", &arguments("old")), |name| {
                            name == "probe"
                        }),
                    );
                    assert_eq!(
                        a.action,
                        Action::Call {
                            name: "probe".into(),
                            arguments: Some(json!({"tag":"old"}))
                        }
                    );
                    let old_response = tool_response(&a.id, "old", old_is_error, !legacy);
                    assert_eq!(old_response["result"]["isError"], json!(old_is_error));
                    let old_text = old_response["result"]["content"][0]["text"]
                        .as_str()
                        .unwrap();
                    assert_eq!(
                        old_text,
                        if old_is_error {
                            "probe failed: index_not_ready: Index is not ready"
                        } else {
                            "probe: 1 item; full result in structuredContent."
                        }
                    );
                    assert!(old_text.len() <= 512);
                    assert_eq!(
                        old_response["result"]["structuredContent"]["requestId"],
                        a.id
                    );
                    if old_is_error {
                        assert_eq!(
                            old_response["result"]["structuredContent"]["error"]["code"],
                            "index_not_ready"
                        );
                    }
                    let queued = s.prepare(a.token, old_response).unwrap();
                    s.cancel(&a.id);
                    assert_eq!(s.pending_count(), 0);
                    let b = admitted(s.accept(
                        req(
                            new,
                            if legacy {
                                Mode::LegacyReady
                            } else {
                                Mode::Modern
                            },
                            "tools/call",
                            &arguments("new"),
                        ),
                        |name| name == "probe",
                    ));
                    assert_ne!(a.token, b.token);
                    assert_eq!(b.id, a.id);
                    assert_eq!(
                        b.action,
                        Action::Call {
                            name: "probe".into(),
                            arguments: Some(json!({"tag":"new"}))
                        }
                    );
                    assert_eq!(s.pending(b.token).unwrap().action, b.action);
                    assert!(
                        s.prepare(a.token, tool_response(&a.id, "late", true, !legacy))
                            .is_none()
                    );
                    assert!(s.commit(queued.clone()).is_none());
                    assert_eq!(s.pending_count(), 1);
                    assert_eq!(s.pending(b.token).unwrap().id, b.id);
                    assert_eq!(s.pending(b.token).unwrap().action, b.action);
                    let expected = tool_response(&b.id, "new", !old_is_error, !legacy);
                    let own = s.prepare(b.token, expected.clone()).unwrap();
                    assert_eq!(s.commit(own), Some(expected.clone()));
                    assert_eq!(expected["id"], b.id);
                    assert_eq!(expected["result"]["structuredContent"]["requestId"], b.id);
                    let text = expected["result"]["content"][0]["text"].as_str().unwrap();
                    assert_eq!(
                        text,
                        if old_is_error {
                            "probe: 1 item; full result in structuredContent."
                        } else {
                            "probe failed: index_not_ready: Index is not ready"
                        }
                    );
                    assert!(text.len() <= 512);
                    assert_eq!(expected["result"]["structuredContent"]["requestId"], b.id);
                    assert!(s.commit(queued).is_none());
                    assert_eq!(s.pending_count(), 0);
                }
            }
        }
    }
    #[test]
    fn validation_and_lifecycle() {
        let mut s = Session::new();
        let bad = req(
            "1",
            Mode::Unselected,
            "server/discover",
            r#","io.modelcontextprotocol/clientCapabilities":{"roots":7}"#,
        );
        rejected(s.accept(bad, |_| false), -32602, json!(1));
        assert_eq!(s.mode(), Mode::Unselected);
        let mut wrong = req("2", Mode::Unselected, "server/discover", "");
        wrong.params.as_mut().unwrap()["_meta"]["io.modelcontextprotocol/protocolVersion"] =
            json!("future");
        match s.accept(wrong, |_| false) {
            Event::Error(e) => {
                assert_eq!(e.code, -32022);
                assert_eq!(
                    e.data.unwrap(),
                    json!({"supported":["2026-07-28","2025-11-25"],"requested":"future"})
                );
            }
            _ => panic!(),
        }
        assert_eq!(s.mode(), Mode::Unselected);
    }
    #[test]
    fn pinned_capability_fields_and_mode_switches() {
        let mut s = Session::new();
        let mut m = req("1", Mode::Unselected, "server/discover", "");
        m.params.as_mut().unwrap()["_meta"]["io.modelcontextprotocol/clientCapabilities"] = json!({"elicitation":{"form":{}},"sampling":{"tools":{}},"extensions":{"com.example/x":{}},"future":{"arbitrary":42}});
        admitted(s.accept(m, |_| false));
        assert_eq!(s.mode(), Mode::Modern);
        rejected(s.accept(req("2",Mode::LegacyReady,"initialize",r#""protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"x","version":"1"}"#), |_|false),-32600,json!(2));
        s.eof();
        assert_eq!(s.pending_count(), 0);
        let mut l = Session::new();
        let mut init = req(
            "3",
            Mode::LegacyReady,
            "initialize",
            r#""protocolVersion":"2024-01-01","capabilities":{},"clientInfo":{"name":"x","version":"1"}"#,
        );
        init.params.as_mut().unwrap()["capabilities"] = json!({"roots":{"listChanged":true},"tasks":{"requests":{"sampling":{"createMessage":{}}}}});
        admitted(l.accept(init, |_| false));
        assert_eq!(l.mode(), Mode::LegacyAwaitInitialized);
        rejected(
            l.accept(req("4", Mode::LegacyReady, "tools/list", ""), |_| false),
            -32600,
            json!(4),
        );
        let n = wire::decode(wire::Frame::Line(
            br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_vec(),
        ))
        .unwrap()
        .unwrap();
        assert!(matches!(l.accept(n, |_| false), Event::Ignore));
        admitted(l.accept(req("5", Mode::LegacyReady, "tools/list", ""), |_| false));
        rejected(
            l.accept(req("6", Mode::Modern, "server/discover", ""), |_| false),
            -32600,
            json!(6),
        );
    }
    #[test]
    fn known_tool_wrong_argument_type_is_application_validation() {
        let mut s = Session::new();
        let admitted = admitted(s.accept(
            req(
                "1",
                Mode::Unselected,
                "tools/call",
                r#","name":"probe","arguments":42"#,
            ),
            |name| name == "probe",
        ));
        assert!(matches!(
            admitted.action,
            Action::Call {
                arguments: Some(Value::Number(_)),
                ..
            }
        ));
        assert_eq!(s.mode(), Mode::Modern);
        rejected(
            s.accept(
                req(
                    "2",
                    Mode::Modern,
                    "tools/call",
                    r#","name":"unknown","arguments":{}"#,
                ),
                |name| name == "probe",
            ),
            -32602,
            json!(2),
        );
    }
    #[test]
    fn prompt_admission_both_modes_is_closed_and_cancellable() {
        for modern in [false, true] {
            let mut session = Session::new();
            if !modern {
                session.mode = Mode::LegacyReady;
            }
            let make = |id: i64, method: &str, mut params: Value| {
                if modern {
                    params["_meta"] = json!({
                    "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities":{}});
                }
                let bytes = serde_json::to_vec(
                    &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
                )
                .unwrap();
                wire::decode(wire::Frame::Line(bytes)).unwrap().unwrap()
            };
            let list = admitted(session.accept(make(1, "prompts/list", json!({})), |_| false));
            assert!(matches!(list.action, Action::PromptList));
            for (id, params) in [
                (2, json!({"name":"unknown"})),
                (
                    3,
                    json!({"name":super::super::architecture::PROMPT_NAME,"arguments":{"scope":"src"}}),
                ),
                (
                    4,
                    json!({"name":super::super::architecture::PROMPT_NAME,"arguments":null}),
                ),
                (
                    5,
                    json!({"name":super::super::architecture::PROMPT_NAME,"extra":true}),
                ),
            ] {
                rejected(
                    session.accept(make(id, "prompts/get", params), |_| false),
                    -32602,
                    json!(id),
                );
            }
            let get = admitted(session.accept(
                make(
                    6,
                    "prompts/get",
                    json!({"name":super::super::architecture::PROMPT_NAME,"arguments":{}}),
                ),
                |_| false,
            ));
            assert!(
                matches!(get.action, Action::Call { ref name, .. } if name == super::super::architecture::PROMPT_NAME)
            );
            session.cancel(&json!(6));
            assert!(session.prepare(get.token, json!({})).is_none());
            rejected(
                session.accept(
                    make(
                        7,
                        "tools/call",
                        json!({"name":super::super::architecture::PROMPT_NAME,"arguments":{}}),
                    ),
                    super::super::catalog::known,
                ),
                -32602,
                json!(7),
            );
        }
    }
}
