//! Browser Architecture summary and its paged detail reads (#448).
//!
//! The summary carries what the view draws plus exact counts. Rule instances,
//! reference pairs and unresolved sites are read on demand, one bounded page at
//! a time, from the same evaluation the summary and `trellis rules check` use.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::model::IndexPin;

/// Default and maximum items per detail page.
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 500;
/// Serialized item bytes per page. A page always holds at least one item, so one
/// oversized item still makes progress under the HTTP response cap.
pub const PAGE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Instances,
    Pairs,
    Unresolved,
    Interfaces,
    /// One summary edge's pairs (#917).
    Edge,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Instances => "instances",
            Self::Pairs => "pairs",
            Self::Unresolved => "unresolved",
            Self::Interfaces => "interfaces",
            Self::Edge => "edge",
        }
    }
}

/// One validated detail request. Every filter is an exact match; absent filters
/// select everything of that kind.
#[derive(Clone, Debug)]
pub struct Query {
    pub pin: IndexPin,
    pub rules_hash: String,
    pub kind: Kind,
    pub offset: usize,
    pub limit: usize,
    pub role: Option<String>,
    pub rule: Option<String>,
    pub status: Option<String>,
    pub excepted: Option<bool>,
    pub exception_reason: Option<String>,
    pub source_layer: Option<String>,
    pub destination_layer: Option<String>,
    pub layer: Option<String>,
    /// `instances`: the name of the app the instance counts under.
    pub app: Option<String>,
    pub path: Option<String>,
    pub source: Option<Value>,
    pub target: Option<Value>,
    pub unlayered: bool,
    /// `interfaces`: the client app's name.
    pub client: Option<String>,
    /// `interfaces`: the server app's name.
    pub server: Option<String>,
    /// `interfaces`: `matched`, `serverOnly` or `clientOnly`.
    pub list: Option<String>,
    /// `edge`: the summary edge's `key` (#917).
    pub key: Option<String>,
}

/// A malformed detail query; the HTTP route answers `invalid_request`.
#[derive(Debug)]
pub struct InvalidQuery;

impl std::fmt::Display for InvalidQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid_request: architecture detail query")
    }
}

impl std::error::Error for InvalidQuery {}

impl Query {
    /// Build a query from already percent-decoded, duplicate-free fields.
    /// Unknown fields, and filters that do not apply to the kind, are invalid.
    pub fn from_fields(mut fields: BTreeMap<String, String>) -> Result<Self, InvalidQuery> {
        let kind = match fields.remove("kind").as_deref() {
            Some("instances") => Kind::Instances,
            Some("pairs") => Kind::Pairs,
            Some("unresolved") => Kind::Unresolved,
            Some("interfaces") => Kind::Interfaces,
            Some("edge") => Kind::Edge,
            _ => return Err(InvalidQuery),
        };
        let allowed: &[&str] = match kind {
            Kind::Instances => &[
                "role",
                "rule",
                "status",
                "excepted",
                "exceptionReason",
                "sourceLayer",
                "destinationLayer",
                "layer",
                "app",
                "path",
            ],
            Kind::Pairs => &["role", "path", "source", "target", "layer", "unlayered"],
            Kind::Unresolved => &["path"],
            Kind::Interfaces => &["client", "server", "list"],
            Kind::Edge => &["key"],
        };
        // An edge is named by its bounded summary `key`, never by its reason text.
        let key = fields.remove("key");
        if (kind == Kind::Edge)
            != key.as_deref().is_some_and(|key| {
                key.len() == 64 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            })
        {
            return Err(InvalidQuery);
        }
        let common = [
            "indexGeneration",
            "indexRevision",
            "rulesHash",
            "offset",
            "limit",
        ];
        if fields
            .keys()
            .any(|name| !common.contains(&name.as_str()) && !allowed.contains(&name.as_str()))
            || fields
                .values()
                .any(|value| value.is_empty() || value.len() > 4096 || value.contains('\0'))
        {
            return Err(InvalidQuery);
        }
        let number = |value: Option<String>, default: usize, max: usize| match value {
            None => Ok(default),
            Some(value) if value.bytes().all(|b| b.is_ascii_digit()) => value
                .parse::<usize>()
                .ok()
                .filter(|value| *value <= max)
                .ok_or(InvalidQuery),
            Some(_) => Err(InvalidQuery),
        };
        let generation = fields.remove("indexGeneration").ok_or(InvalidQuery)?;
        let revision = number(
            Some(fields.remove("indexRevision").ok_or(InvalidQuery)?),
            0,
            usize::MAX,
        )?;
        let pin: IndexPin = serde_json::from_value(
            json!({"indexGeneration": generation, "indexRevision": revision}),
        )
        .map_err(|_| InvalidQuery)?;
        let rules_hash = fields.remove("rulesHash").ok_or(InvalidQuery)?;
        if rules_hash.len() > 128 {
            return Err(InvalidQuery);
        }
        let offset = number(fields.remove("offset"), 0, 1 << 53)?;
        let limit = number(fields.remove("limit"), DEFAULT_LIMIT, MAX_LIMIT)?;
        if limit == 0 {
            return Err(InvalidQuery);
        }
        let role = fields.remove("role");
        if role
            .as_deref()
            .is_some_and(|role| !matches!(role, "production" | "test"))
        {
            return Err(InvalidQuery);
        }
        let status = fields.remove("status");
        if status
            .as_deref()
            .is_some_and(|status| !matches!(status, "conforms" | "deviates" | "unknown"))
        {
            return Err(InvalidQuery);
        }
        let flag = |value: Option<String>| match value.as_deref() {
            None => Ok(None),
            Some("true") => Ok(Some(true)),
            Some("false") => Ok(Some(false)),
            Some(_) => Err(InvalidQuery),
        };
        let excepted = flag(fields.remove("excepted"))?;
        // A pair endpoint is an exact identity: a native target or external package object.
        let identity = |value: Option<String>| match value {
            None => Ok(None),
            Some(text) => match serde_json::from_str::<Value>(&text) {
                Ok(value @ Value::Object(_)) => Ok(Some(value)),
                _ => Err(InvalidQuery),
            },
        };
        let list = fields.remove("list");
        if list
            .as_deref()
            .is_some_and(|list| crate::interfaces::List::parse(list).is_none())
        {
            return Err(InvalidQuery);
        }
        let unlayered = match flag(fields.remove("unlayered"))? {
            None => false,
            Some(true) => true,
            Some(false) => return Err(InvalidQuery),
        };
        Ok(Self {
            pin,
            rules_hash,
            kind,
            offset,
            limit,
            role,
            rule: fields.remove("rule"),
            status,
            excepted,
            exception_reason: fields.remove("exceptionReason"),
            source_layer: fields.remove("sourceLayer"),
            destination_layer: fields.remove("destinationLayer"),
            layer: fields.remove("layer"),
            app: fields.remove("app"),
            path: fields.remove("path"),
            source: identity(fields.remove("source"))?,
            target: identity(fields.remove("target"))?,
            unlayered,
            client: fields.remove("client"),
            server: fields.remove("server"),
            list,
            key,
        })
    }
}

/// Layer membership for native files (authored assignment) and external
/// packages (the destination layer their `$layers` instances report).
pub struct Layers<'a> {
    native: &'a BTreeMap<&'a str, Option<&'a str>>,
    external: BTreeMap<String, Option<String>>,
    apps: Option<crate::rules::AppScope<'a>>,
}

impl<'a> Layers<'a> {
    pub fn new(native: &'a BTreeMap<&'a str, Option<&'a str>>, rules: &Value) -> Self {
        let mut external = BTreeMap::new();
        for field in ["instances", "testOnly"] {
            for row in rules[field].as_array().into_iter().flatten() {
                let destination = &row["destination"];
                if row["rule"] == "$layers" && destination["kind"] == "externalPackage" {
                    external
                        .entry(destination.to_string())
                        .or_insert_with(|| row["destinationLayer"].as_str().map(str::to_owned));
                }
            }
        }
        Self {
            native,
            external,
            apps: None,
        }
    }
    /// Let `app` filters resolve instances to their app.
    pub(crate) fn with_apps(mut self, apps: crate::rules::AppScope<'a>) -> Self {
        self.apps = Some(apps);
        self
    }
    /// Whether `row` counts under an app named `name` (a shared name matches each of its apps).
    fn counts_under(&self, name: &str, row: &Value) -> bool {
        self.apps.as_ref().is_some_and(|apps| {
            apps.bucket(self.native, row)
                .and_then(|index| apps.name(index))
                == Some(name)
        })
    }
    /// The layer of a native identity or external package; None when unassigned.
    pub fn of(&self, identity: &Value) -> Option<&str> {
        if identity["kind"] == "externalPackage" {
            return self
                .external
                .get(&identity.to_string())
                .and_then(|layer| layer.as_deref());
        }
        identity["path"]
            .as_str()
            .and_then(|path| self.native.get(path).copied().flatten())
    }
}

/// The summary edge a reference pair counts under (#917): its layers, verdict and exception.
/// The role is passed beside it.
pub(crate) struct EdgeKey<'a> {
    pub(crate) source: &'a str,
    pub(crate) destination: &'a str,
    pub(crate) status: &'a str,
    pub(crate) excepted: bool,
    pub(crate) reason: Option<&'a str>,
}

impl EdgeKey<'_> {
    /// The summary edge's bounded `key` (#917): SHA-256 hex of its whole identity, so a detail
    /// read names the edge without sending its layer names or exception reason.
    pub(crate) fn key(&self, role: &str) -> String {
        use sha2::{Digest, Sha256};
        let identity = json!([
            self.source,
            self.destination,
            role,
            self.status,
            self.excepted,
            self.reason
        ]);
        hex::encode(Sha256::digest(identity.to_string().as_bytes()))
    }
}

/// Visit every reference pair that a summary edge counts, production before test, in
/// evaluation order, with its role and edge. Same-layer, unassigned and cross-app pairs draw
/// no edge. The summary's `edges` and the details route's `edge` kind both come from here.
pub(crate) fn edge_pairs<'a>(
    references: &'a Value,
    rules: &'a Value,
    check: &mut (impl FnMut() -> Result<()> + Send),
    mut visit: impl FnMut(&'static str, EdgeKey<'a>, &'a Value) -> Result<()>,
) -> Result<()> {
    for (field, evaluated, role) in [
        ("production", "instances", "production"),
        ("tests", "testOnly", "test"),
    ] {
        let verdicts: BTreeMap<_, _> = rules[evaluated]
            .as_array()
            .context("store_unavailable: layer verdicts")?
            .iter()
            .filter(|row| row["rule"] == "$layers" && !row["destination"].is_null())
            .map(|row| {
                (
                    (row["target"].to_string(), row["destination"].to_string()),
                    row,
                )
            })
            .collect();
        for pair in references[field]
            .as_array()
            .context("store_unavailable: reference pairs")?
        {
            check()?;
            let verdict = verdicts
                .get(&(pair["source"].to_string(), pair["target"].to_string()))
                .context("store_unavailable: architecture verdict join")?;
            let (Some(source), Some(destination)) = (
                verdict["sourceLayer"].as_str(),
                verdict["destinationLayer"].as_str(),
            ) else {
                continue;
            };
            if source == destination
                || verdict["unresolved"]
                    .as_array()
                    .is_some_and(|reasons| reasons.iter().any(|reason| reason == "cross_app"))
            {
                continue;
            }
            let key = EdgeKey {
                source,
                destination,
                status: verdict["status"]
                    .as_str()
                    .context("store_unavailable: layer status")?,
                excepted: verdict["excepted"]
                    .as_bool()
                    .context("store_unavailable: layer exception")?,
                reason: verdict["exceptionReason"].as_str(),
            };
            visit(role, key, pair)?;
        }
    }
    Ok(())
}

/// The bounded default view fields: exact counts and limitations, without the
/// per-site, per-pair and per-instance arrays that detail pages serve.
pub fn summary(references: &Value, rules: &Value, layers: &Layers<'_>) -> Result<(Value, Value)> {
    let mut packages = Vec::new();
    for package in references["externalPackages"]
        .as_array()
        .context("store_unavailable: reference packages")?
    {
        let mut package = package.clone();
        package["layer"] = json!(layers.of(&package["identity"]));
        packages.push(package);
    }
    let references = json!({
        "referencesVersion": references["referencesVersion"],
        "indexGeneration": references["indexGeneration"],
        "indexRevision": references["indexRevision"],
        "evidenceTier": references["evidenceTier"],
        "semanticComplete": references["semanticComplete"],
        "semanticProducers": references["semanticProducers"],
        "counts": references["counts"],
        "partialFiles": references["partialFiles"],
        "limitations": references["limitations"],
        "externalPackages": packages,
    });
    let rules = json!({
        "rulesVersion": rules["rulesVersion"],
        "indexGeneration": rules["indexGeneration"],
        "indexRevision": rules["indexRevision"],
        "rulesFile": rules["rulesFile"],
        "counts": rules["counts"],
        "testCounts": rules["testCounts"],
        "unassignedCount": rules["unassigned"]
            .as_array()
            .context("store_unavailable: unassigned identities")?
            .len(),
        "partialFiles": rules["partialFiles"],
        "limitations": rules["limitations"],
    });
    Ok((references, rules))
}

/// One page of details. `total` counts every matching item; items keep the
/// evaluation's order, production before test.
pub(crate) fn page(
    query: &Query,
    references: &Value,
    rules: &Value,
    layers: &Layers<'_>,
    interfaces: Option<&crate::interfaces::Projection<'_>>,
    check: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<Value> {
    let path_is = |identity: &Value, path: &Option<String>| {
        path.as_deref()
            .is_none_or(|path| identity["path"].as_str() == Some(path))
    };
    let text_is = |value: &Value, expected: &Option<String>| {
        expected
            .as_deref()
            .is_none_or(|expected| value.as_str() == Some(expected))
    };
    let role_is = |role: &str| query.role.as_deref().is_none_or(|wanted| wanted == role);
    let mut matched: Vec<Value> = Vec::new();
    let mut total = 0usize;
    let mut bytes = 0usize;
    let mut next_offset = None;
    let mut admit = |item: &dyn Fn() -> Value| -> Result<()> {
        let index = total;
        total += 1;
        if index < query.offset || next_offset.is_some() {
            return Ok(());
        }
        let item = item();
        let size = serde_json::to_vec(&item)?.len() + 1;
        if matched.len() == query.limit || (!matched.is_empty() && bytes + size > PAGE_BYTES) {
            next_offset = Some(index);
            return Ok(());
        }
        bytes += size;
        matched.push(item);
        Ok(())
    };
    match query.kind {
        Kind::Instances => {
            for (field, role) in [("instances", "production"), ("testOnly", "test")] {
                if !role_is(role) {
                    continue;
                }
                for row in rules[field]
                    .as_array()
                    .context("store_unavailable: rule instances")?
                {
                    check()?;
                    let in_layer = query.layer.as_deref().is_none_or(|layer| {
                        row["sourceLayer"].as_str() == Some(layer)
                            || row["destinationLayer"].as_str() == Some(layer)
                    });
                    let on_path = query.path.as_deref().is_none_or(|path| {
                        row["target"]["path"].as_str() == Some(path)
                            || row["destination"]["path"].as_str() == Some(path)
                    });
                    let in_app = query
                        .app
                        .as_deref()
                        .is_none_or(|app| layers.counts_under(app, row));
                    if in_app
                        && text_is(&row["rule"], &query.rule)
                        && text_is(&row["status"], &query.status)
                        && query
                            .excepted
                            .is_none_or(|excepted| row["excepted"].as_bool() == Some(excepted))
                        && text_is(&row["exceptionReason"], &query.exception_reason)
                        && text_is(&row["sourceLayer"], &query.source_layer)
                        && text_is(&row["destinationLayer"], &query.destination_layer)
                        && in_layer
                        && on_path
                    {
                        admit(&|| json!({"role": role, "instance": row}))?;
                    }
                }
            }
        }
        Kind::Pairs => {
            for (field, role) in [("production", "production"), ("tests", "test")] {
                if !role_is(role) {
                    continue;
                }
                for pair in references[field]
                    .as_array()
                    .context("store_unavailable: reference pairs")?
                {
                    check()?;
                    let (source, target) = (layers.of(&pair["source"]), layers.of(&pair["target"]));
                    let on_path = query.path.as_deref().is_none_or(|path| {
                        pair["source"]["path"].as_str() == Some(path)
                            || pair["target"]["path"].as_str() == Some(path)
                    });
                    let in_layer = query
                        .layer
                        .as_deref()
                        .is_none_or(|layer| source == Some(layer) || target == Some(layer));
                    let unlayered = !query.unlayered
                        || source.is_none()
                        || target.is_none()
                        || source == target;
                    if on_path
                        && query
                            .source
                            .as_ref()
                            .is_none_or(|source| pair["source"] == *source)
                        && query
                            .target
                            .as_ref()
                            .is_none_or(|target| pair["target"] == *target)
                        && in_layer
                        && unlayered
                    {
                        admit(&|| {
                            json!({"role": role, "sourceLayer": source,
                            "targetLayer": target, "pair": pair})
                        })?;
                    }
                }
            }
        }
        Kind::Unresolved => {
            for item in references["unresolved"]
                .as_array()
                .context("store_unavailable: unresolved sites")?
            {
                check()?;
                if path_is(&item["source"], &query.path) {
                    admit(&|| item.clone())?;
                }
            }
        }
        Kind::Interfaces => {
            let interfaces =
                interfaces.context("store_unavailable: architecture interfaces part")?;
            let list = query
                .list
                .as_deref()
                .and_then(crate::interfaces::List::parse);
            for row in interfaces.rows(query.client.as_deref(), query.server.as_deref(), list) {
                check()?;
                admit(&|| interfaces.item(row))?;
            }
        }
        Kind::Edge => {
            edge_pairs(references, rules, check, |role, edge, pair| {
                if query.key.as_deref() == Some(edge.key(role).as_str()) {
                    admit(&|| pair.clone())?;
                }
                Ok(())
            })?;
            // Every summary edge counts at least one pair.
            ensure!(total > 0, "not_found: architecture edge");
        }
    }
    ensure!(query.offset <= total, "invalid_request: detail offset");
    Ok(json!({
        "indexGeneration": query.pin.index_generation.to_string(),
        "indexRevision": query.pin.index_revision,
        "rulesFile": rules["rulesFile"],
        "kind": query.kind.name(),
        "offset": query.offset,
        "total": total,
        "nextOffset": next_offset,
        "items": matched,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(offset: usize, limit: usize) -> Query {
        let fields = [
            ("kind", "unresolved".to_owned()),
            (
                "indexGeneration",
                "12345678-1234-4123-8123-123456789abc".to_owned(),
            ),
            ("indexRevision", "1".to_owned()),
            ("rulesHash", "hash".to_owned()),
            ("offset", offset.to_string()),
            ("limit", limit.to_string()),
        ];
        Query::from_fields(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()).unwrap()
    }

    /// Pages stop at PAGE_BYTES but always make progress with at least one item.
    #[test]
    fn pages_stop_at_the_byte_bound_and_always_progress() {
        let site = |size: usize| json!({"source":{"path":"a.rs"},"reason":"x".repeat(size)});
        let references = json!({"unresolved": [
            site(PAGE_BYTES * 2), site(PAGE_BYTES / 3), site(PAGE_BYTES / 3), site(PAGE_BYTES / 3), site(10)
        ]});
        let native = BTreeMap::new();
        let rules = json!({"rulesFile":{"contentHash":"hash"}});
        let layers = Layers::new(&native, &rules);
        let mut offset = Some(0);
        let mut pages = Vec::new();
        while let Some(at) = offset {
            let page = page(
                &query(at, MAX_LIMIT),
                &references,
                &rules,
                &layers,
                None,
                &mut || Ok(()),
            )
            .unwrap();
            assert_eq!(page["total"], 5);
            pages.push(page["items"].as_array().unwrap().len());
            offset = page["nextOffset"].as_u64().map(|next| next as usize);
        }
        // The oversized item alone, then two thirds that fit, then the rest.
        assert_eq!(pages, vec![1, 2, 2]);
        assert!(
            page(
                &query(6, 1),
                &references,
                &rules,
                &layers,
                None,
                &mut || Ok(())
            )
            .is_err()
        );
        let end = page(
            &query(5, 1),
            &references,
            &rules,
            &layers,
            None,
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(end["items"], json!([]));
        assert!(end["nextOffset"].is_null());
    }

    /// #785: instance pages pass `kindCounts` and the classified evidence through, and
    /// one-row pages concatenate to the whole list.
    #[test]
    fn instance_pages_carry_kind_counts_and_classified_evidence() {
        let row = |path: &str| {
            json!({"rule":"$layers","status":"deviates","excepted":false,"exceptionReason":null,
                "sourceLayer":"a","destinationLayer":"b","target":{"path":path},"destination":{"path":"d.rs"},
                "referenceCount":2,"kindCounts":{"call":1,"type":0,"import":1,"reference":0},
                "evidence":[{"relationKind":"call","spelling":"d::f","lines":{"start":3,"end":3},
                    "owner":{"syntaxId":"s","name":"run","stableLookupKey":"a.rs::run"},
                    "target":null,"targetReason":"structural_only"}]})
        };
        let rules = json!({"rulesFile":{"contentHash":"hash"},
            "instances":[row("a.rs"), row("b.rs"), row("c.rs")],"testOnly":[]});
        let native = BTreeMap::new();
        let layers = Layers::new(&native, &rules);
        let references = json!({});
        let mut query = query(0, 1);
        query.kind = Kind::Instances;
        let mut items = Vec::new();
        let mut offset = Some(0);
        while let Some(at) = offset {
            query.offset = at;
            let page = page(&query, &references, &rules, &layers, None, &mut || Ok(())).unwrap();
            assert_eq!(page["total"], 3);
            items.extend(page["items"].as_array().unwrap().iter().cloned());
            offset = page["nextOffset"].as_u64().map(|next| next as usize);
        }
        assert_eq!(items.len(), 3);
        for (item, path) in items.iter().zip(["a.rs", "b.rs", "c.rs"]) {
            assert_eq!(item["instance"], row(path));
            assert_eq!(item["instance"]["kindCounts"]["call"], 1);
            assert_eq!(item["instance"]["evidence"][0]["relationKind"], "call");
        }
    }

    /// #785: pair pages pass the edge's `kindCounts` and its sites' classified fields through.
    #[test]
    fn pair_pages_carry_kind_counts_and_site_classification() {
        let pair = json!({"source":{"path":"a.rs"},"target":{"path":"b.rs"},"count":2,
            "kindCounts":{"call":1,"type":0,"import":0,"reference":1},
            "sites":[{"relationKind":"call","owner":{"syntaxId":"s","name":"run","stableLookupKey":"a.rs::run"},
                    "target":{"syntaxId":"t","displayKey":"b.rs#f","document":{"path":"b.rs"},"lines":{"start":1,"end":1}},
                    "targetReason":null},
                {"relationKind":"reference","owner":null,"target":null,"targetReason":"unresolved"}]});
        let references = json!({"production":[pair.clone()],"tests":[]});
        let rules = json!({"rulesFile":{"contentHash":"hash"}});
        let native = BTreeMap::new();
        let layers = Layers::new(&native, &rules);
        let mut query = query(0, 1);
        query.kind = Kind::Pairs;
        let page = page(&query, &references, &rules, &layers, None, &mut || Ok(())).unwrap();
        assert_eq!(page["items"][0]["pair"], pair);
        assert_eq!(page["items"][0]["pair"]["kindCounts"]["call"], 1);
        assert_eq!(
            page["items"][0]["pair"]["sites"][1]["targetReason"],
            "unresolved"
        );
    }
    /// #917: the `edge` kind pages exactly the pairs one summary edge counts, named by its
    /// bounded `key`, in evaluation order; same-layer, unassigned and cross-app pairs belong to
    /// no edge, a key that names no edge is `not_found`, and reasons of any length (past the
    /// 4096-byte field bound, or multibyte) never travel in the request.
    #[test]
    fn edge_pages_hold_exactly_one_edges_pairs() {
        let long = "r".repeat(4097);
        let wide = "境".repeat(1000);
        let pair = |to: &str, count: u64| {
            json!({"source":{"path":"a.rs"},"target":{"path":to},"count":count,"semanticSites":0,
                "sites":[{"relationKind":"call"}],"omittedSites":count - 1})
        };
        let verdict = |to: &str, destination: Option<&str>, excepted: Option<&str>| {
            json!({"rule":"$layers","target":{"path":"a.rs"},"destination":{"path":to},
                "sourceLayer":"low","destinationLayer":destination,"status":"deviates",
                "excepted":excepted.is_some(),"exceptionReason":excepted,
                "unresolved": if to == "x.rs" { json!(["cross_app"]) } else { json!([]) }})
        };
        let references = json!({
            "production":[pair("b.rs", 2), pair("e.rs", 4), pair("c.rs", 1), pair("s.rs", 1),
                pair("u.rs", 1), pair("x.rs", 1), pair("l.rs", 1), pair("l2.rs", 2), pair("w.rs", 1)],
            "tests":[pair("b.rs", 3)],
        });
        let rules = json!({"rulesFile":{"contentHash":"hash"},
            "instances":[verdict("b.rs", Some("high"), None), verdict("e.rs", Some("high"), Some("ok")),
                verdict("c.rs", Some("high"), None), verdict("s.rs", Some("low"), None),
                verdict("u.rs", None, None), verdict("x.rs", Some("high"), None),
                verdict("l.rs", Some("high"), Some(&long)), verdict("l2.rs", Some("high"), Some(&long)),
                verdict("w.rs", Some("high"), Some(&wide))],
            "testOnly":[verdict("b.rs", Some("high"), None)]});
        let native = BTreeMap::new();
        let layers = Layers::new(&native, &rules);
        let key = |destination: &str, role: &str, status: &str, reason: Option<&str>| {
            EdgeKey {
                source: "low",
                destination,
                status,
                excepted: reason.is_some(),
                reason,
            }
            .key(role)
        };
        let query = |extra: &[(&str, &str)]| {
            let mut fields: BTreeMap<String, String> = [
                ("kind", "edge"),
                ("indexGeneration", "12345678-1234-4123-8123-123456789abc"),
                ("indexRevision", "1"),
                ("rulesHash", "hash"),
                ("limit", "1"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
            for (k, v) in extra {
                fields.insert((*k).to_owned(), (*v).to_owned());
            }
            Query::from_fields(fields)
        };
        let edge = |key: &str| query(&[("key", key)]).unwrap();
        let all = |query: Query| {
            let (mut query, mut items) = (query, Vec::new());
            loop {
                let page = page(&query, &references, &rules, &layers, None, &mut || Ok(()))?;
                assert_eq!(page["kind"], "edge");
                items.extend(page["items"].as_array().unwrap().iter().cloned());
                match page["nextOffset"].as_u64() {
                    Some(next) => query.offset = next as usize,
                    None => {
                        assert_eq!(page["total"], items.len());
                        return Ok::<_, anyhow::Error>(items);
                    }
                }
            }
        };
        let targets = |items: Vec<Value>| -> Vec<String> {
            items
                .iter()
                .map(|item| item["target"]["path"].as_str().unwrap().to_owned())
                .collect()
        };
        let plain = key("high", "production", "deviates", None);
        // One pair per page; the pages concatenate to the edge's pairs, items unchanged.
        let items = all(edge(&plain)).unwrap();
        assert_eq!(items[0], references["production"][0]);
        assert_eq!(targets(items), ["b.rs", "c.rs"]);
        let excepted = key("high", "production", "deviates", Some("ok"));
        assert_eq!(targets(all(edge(&excepted)).unwrap()), ["e.rs"]);
        let long_key = key("high", "production", "deviates", Some(&long));
        assert_eq!(targets(all(edge(&long_key)).unwrap()), ["l.rs", "l2.rs"]);
        let wide_key = key("high", "production", "deviates", Some(&wide));
        assert_eq!(targets(all(edge(&wide_key)).unwrap()), ["w.rs"]);
        let test = all(edge(&key("high", "test", "deviates", None))).unwrap();
        assert_eq!(test, [references["tests"][0].clone()]);
        // Every reference pair counts under the same edges as the summary, keyed alike.
        let mut keys = BTreeMap::new();
        edge_pairs(&references, &rules, &mut || Ok(()), |role, edge, _| {
            *keys.entry(edge.key(role)).or_insert(0) += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(keys.values().sum::<i32>(), 7, "{keys:?}");
        assert_eq!(keys.len(), 5);
        for known in [&plain, &excepted, &long_key, &wide_key] {
            assert!(keys.contains_key(known.as_str()));
        }
        for unknown in [
            key("high", "production", "conforms", None),
            key("low", "production", "deviates", None),
            key("high", "production", "deviates", Some(&long[1..])),
            "0".repeat(64),
        ] {
            let error = all(edge(&unknown)).unwrap_err().to_string();
            assert!(error.starts_with("not_found: architecture edge"), "{error}");
        }
        let mut past = edge(&plain);
        past.offset = 3;
        assert!(
            page(&past, &references, &rules, &layers, None, &mut || Ok(()))
                .unwrap_err()
                .to_string()
                .starts_with("invalid_request")
        );
        let upper = plain.to_uppercase();
        for invalid in [
            &[][..],
            &[("key", &plain[1..])],
            &[("key", upper.as_str())],
            &[("key", plain.as_str()), ("sourceLayer", "low")],
            &[("key", plain.as_str()), ("exceptionReason", "ok")],
            &[("key", plain.as_str()), ("path", "a.rs")],
        ] {
            assert!(query(invalid).is_err(), "{invalid:?}");
        }
        assert!(
            query(&[("kind", "pairs"), ("key", plain.as_str())]).is_err(),
            "a key belongs to the edge kind only"
        );
    }
    /// #784: the `app` filter keeps the rows whose bucket's name matches, so two apps that
    /// share a name answer to one filter value, and a row with no app matches none.
    #[test]
    fn the_app_filter_matches_every_app_of_that_name() {
        let app =
            |name: &str, root: &str| crate::apps::App::authored(name.into(), vec![root.into()]);
        let apps = [app("web", "a"), app("web", "b"), app("api", "c")];
        let config = crate::rules::parse("version=1\n[layers.l]\norder=1\npaths=['**']\n").unwrap();
        let row = |path: &str| {
            json!({"rule":"$external","status":"conforms","excepted":false,"exceptionReason":null,
                "sourceLayer":null,"destinationLayer":null,"target":{"path":path}})
        };
        let rules = json!({"rulesFile":{"contentHash":"hash"},
            "instances":[row("a/x.js"), row("b/x.js"), row("c/x.js"), row("d/x.js")],"testOnly":[]});
        let native = BTreeMap::new();
        let layers =
            Layers::new(&native, &rules).with_apps(crate::rules::AppScope::new(&config, &apps));
        let paths = |app: &str| {
            let mut query = query(0, 10);
            query.kind = Kind::Instances;
            query.app = Some(app.into());
            let page = page(&query, &json!({}), &rules, &layers, None, &mut || Ok(())).unwrap();
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| {
                    item["instance"]["target"]["path"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(paths("web"), ["a/x.js", "b/x.js"]);
        assert_eq!(paths("api"), ["c/x.js"]);
        assert!(paths("missing").is_empty());
    }
}
