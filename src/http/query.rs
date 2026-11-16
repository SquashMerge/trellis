//! Query-string decoding for the architecture and metrics browser routes.

use super::*;

/// The Architecture options. Edges never carry their per-file pairs: the details route's
/// `edge` kind pages them (#917, owner 2026-11-11). `pairs=false`, which asked for that
/// answer under #915, stays accepted once and changes nothing; `pairs=true` is refused.
pub(super) fn architecture_query(
    uri: &axum::http::Uri,
) -> Result<crate::metrics_read::MetricsOptions, ApiError> {
    let mut pairs = false;
    let mut rest = Vec::new();
    for pair in uri.query().into_iter().flat_map(|query| query.split('&')) {
        match pair.split_once('=') {
            Some(("pairs", "false")) if !pairs => pairs = true,
            Some(("pairs", _)) => return Err(invalid()),
            _ => rest.push(pair),
        }
    }
    let uri = if pairs {
        let rest = if rest.is_empty() {
            "/".to_owned()
        } else {
            format!("/?{}", rest.join("&"))
        };
        rest.parse::<axum::http::Uri>().map_err(|_| invalid())?
    } else {
        uri.clone()
    };
    let uri = &uri;
    let mut options = metrics_query(uri)?;
    // metrics_query already decodes and rejects duplicate/unknown fields. Architecture
    // accepts only the complete pin, aggregation and role, not metrics selectors.
    if let Some(query) = uri.query() {
        for pair in query.split('&') {
            let name = pair.split_once('=').ok_or_else(invalid)?.0;
            if !matches!(
                name,
                "indexGeneration" | "indexRevision" | "aggregate" | "role" | "workbench"
            ) {
                return Err(invalid());
            }
        }
    }
    if !uri
        .query()
        .is_some_and(|query| query.split('&').any(|pair| pair.starts_with("aggregate=")))
    {
        options.aggregate = "max".into();
    }
    Ok(options)
}

/// Detail pages require the summary's full pin and rules hash; see
/// `architecture_details::Query` for the closed filter set.
pub(super) fn architecture_details_query(
    uri: &axum::http::Uri,
) -> Result<crate::architecture_details::Query, ApiError> {
    let query = uri.query().ok_or_else(invalid)?;
    if query.len() > 8192 {
        return Err(invalid());
    }
    let mut fields = std::collections::BTreeMap::new();
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').ok_or_else(invalid)?;
        if fields
            .insert(decode_query(name)?, decode_query(value)?)
            .is_some()
        {
            return Err(invalid());
        }
    }
    crate::architecture_details::Query::from_fields(fields).map_err(|_| invalid())
}

/// Percent-decode one bounded query component.
fn decode_query(value: &str) -> Result<String, ApiError> {
    if value.len() > 8192 {
        return Err(invalid());
    }
    let mut bytes = Vec::with_capacity(value.len());
    let input = value.as_bytes();
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'%' => {
                let hex = input.get(i + 1..i + 3).ok_or_else(invalid)?;
                let hex = std::str::from_utf8(hex).map_err(|_| invalid())?;
                bytes.push(u8::from_str_radix(hex, 16).map_err(|_| invalid())?);
                i += 3;
            }
            b'+' => {
                bytes.push(b' ');
                i += 1;
            }
            byte => {
                bytes.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(bytes).map_err(|_| invalid())
}

pub(super) fn metrics_query(
    uri: &axum::http::Uri,
) -> Result<crate::metrics_read::MetricsOptions, ApiError> {
    use std::collections::BTreeMap;
    let mut fields = BTreeMap::new();
    if let Some(query) = uri.query() {
        if query.len() > 8192 {
            return Err(invalid());
        }
        for pair in query.split('&') {
            let (name, value) = pair.split_once('=').ok_or_else(invalid)?;
            let name = decode_query(name)?;
            if !matches!(
                name.as_str(),
                "scope"
                    | "path"
                    | "language"
                    | "sourceSetId"
                    | "sid"
                    | "indexGeneration"
                    | "indexRevision"
                    | "aggregate"
                    | "role"
                    | "metrics"
                    | "topCognitive"
                    | "workbench"
            ) || fields.insert(name, decode_query(value)?).is_some()
            {
                return Err(invalid());
            }
        }
    }
    let mut options = crate::metrics_read::MetricsOptions::default();
    if let Some(value) = fields.remove("scope") {
        options.scope = value;
    }
    options.path = fields.remove("path");
    options.language = fields.remove("language");
    options.source_set_id = fields.remove("sourceSetId");
    options.sid = fields.remove("sid");
    if let Some(value) = fields.remove("aggregate") {
        options.aggregate = value;
    }
    if let Some(value) = fields.remove("role") {
        options.role = value;
    }
    if let Some(value) = fields.remove("workbench") {
        options.workbench = match value.as_str() {
            "true" => true,
            "false" => false,
            _ => return Err(invalid()),
        };
    }
    if let Some(value) = fields.remove("topCognitive") {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        options.top_cognitive = Some(value.parse().map_err(|_| invalid())?);
    }
    let generation = fields.remove("indexGeneration");
    let revision = fields.remove("indexRevision");
    match (generation, revision) {
        (None, None) => (),
        (Some(generation), Some(revision))
            if !revision.is_empty() && revision.bytes().all(|b| b.is_ascii_digit()) =>
        {
            let revision = revision.parse::<u64>().map_err(|_| invalid())?;
            options.pin = Some(
                serde_json::from_value(json!({
                    "indexGeneration": generation, "indexRevision": revision
                }))
                .map_err(|_| invalid())?,
            );
        }
        _ => return Err(invalid()),
    }
    if let Some(names) = fields.remove("metrics") {
        if names.is_empty() {
            return Err(invalid());
        }
        options.metrics = names.split(',').map(str::to_owned).collect();
    }
    options.validate_transport().map_err(|_| invalid())?;
    Ok(options)
}
