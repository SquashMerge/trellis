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
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Instances => "instances",
            Self::Pairs => "pairs",
            Self::Unresolved => "unresolved",
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
    pub path: Option<String>,
    pub source: Option<Value>,
    pub target: Option<Value>,
    pub unlayered: bool,
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
                "path",
            ],
            Kind::Pairs => &["role", "path", "source", "target", "layer", "unlayered"],
            Kind::Unresolved => &["path"],
        };
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
            path: fields.remove("path"),
            source: identity(fields.remove("source"))?,
            target: identity(fields.remove("target"))?,
            unlayered,
        })
    }
}

/// Layer membership for native files (authored assignment) and external
/// packages (the destination layer their `$layers` instances report).
pub struct Layers<'a> {
    native: &'a BTreeMap<&'a str, Option<&'a str>>,
    external: BTreeMap<String, Option<String>>,
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
        Self { native, external }
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
        "counts": references["counts"],
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
        "limitations": rules["limitations"],
    });
    Ok((references, rules))
}

/// One page of details. `total` counts every matching item; items keep the
/// evaluation's order, production before test.
pub fn page(
    query: &Query,
    references: &Value,
    rules: &Value,
    layers: &Layers<'_>,
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
                    if text_is(&row["rule"], &query.rule)
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
                &mut || Ok(()),
            )
            .unwrap();
            assert_eq!(page["total"], 5);
            pages.push(page["items"].as_array().unwrap().len());
            offset = page["nextOffset"].as_u64().map(|next| next as usize);
        }
        // The oversized item alone, then two thirds that fit, then the rest.
        assert_eq!(pages, vec![1, 2, 2]);
        assert!(page(&query(6, 1), &references, &rules, &layers, &mut || Ok(())).is_err());
        let end = page(&query(5, 1), &references, &rules, &layers, &mut || Ok(())).unwrap();
        assert_eq!(end["items"], json!([]));
        assert!(end["nextOffset"].is_null());
    }
}
