//! `trellis api` (#817): the browser API routes the Herdr mod reads, answered through the
//! daemon socket instead of the loopback listener.
//!
//! The private per-user socket is the caller's authentication, as for every other CLI command,
//! so no serve token is read and no listener has to be provisioned. Each request runs the same
//! handler the provisioned browser runs for that route, with the same request bounds; anything
//! outside the allowlist is a typed `route_not_allowed`.
use super::{ApiError, ProvisionedBrowser, provisioned_checkouts, provisioned_core};
use axum::{
    body::{Bytes, to_bytes},
    extract::{Path, State},
    http::{HeaderMap, Method, Uri},
    response::{IntoResponse, Response},
};
use serde_json::Value;
use std::sync::Arc;

/// A POST body travels inside one daemon socket frame (1 MiB), so it is held below the
/// browser's 1 MiB body limit. The mod's sequence body is a few hundred bytes.
pub const CLI_API_BODY_LIMIT: usize = 64 * 1024;
/// The largest answer relayed. The browser listener streams without a transport cap; each of
/// these routes bounds its own answer, and this only stops a runaway reply.
pub const CLI_API_RESPONSE_LIMIT: usize = 16 * 1024 * 1024;
const TARGET_LIMIT: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliApiRoute {
    Checkouts,
    Checkout {
        root_key: String,
        suffix: &'static str,
    },
}

/// The allowlist: exactly the routes the Herdr mod uses, with the browser's methods. `target`
/// is the path and query, as the browser sends it.
pub fn cli_api_route(method: &str, target: &str, has_body: bool) -> anyhow::Result<CliApiRoute> {
    anyhow::ensure!(
        !target.is_empty()
            && target.len() <= TARGET_LIMIT
            && target.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
            && !target.contains('#'),
        "invalid_request: the route must be 1..{TARGET_LIMIT} bytes of printable ASCII"
    );
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    let refused =
        || anyhow::anyhow!("route_not_allowed: {method} {path} is not a route trellis api serves");
    let route = if path == "/api/checkouts" {
        (method == "GET" && query.is_none()).then_some(CliApiRoute::Checkouts)
    } else {
        let (root_key, suffix) = path
            .strip_prefix("/api/checkouts/")
            .and_then(|rest| rest.split_once('/'))
            .ok_or_else(refused)?;
        let suffix = match (method, suffix, query.is_some()) {
            ("GET", "status", false) => Some("status"),
            ("GET", "architecture", _) => Some("architecture"),
            ("GET", "methods", _) => Some("methods"),
            ("POST", "sequence", false) => Some("sequence"),
            _ => None,
        };
        suffix
            .filter(|_| {
                root_key.len() == 64
                    && root_key
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            .map(|suffix| CliApiRoute::Checkout {
                root_key: root_key.to_owned(),
                suffix,
            })
    }
    .ok_or_else(refused)?;
    anyhow::ensure!(
        has_body == (method == "POST"),
        "invalid_request: POST takes a JSON body and GET takes none"
    );
    Ok(route)
}

/// Answers one allowlisted request: the HTTP status and the JSON the browser would receive.
pub async fn cli_api(
    registry: Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    method: &str,
    target: &str,
    body: Option<&str>,
) -> anyhow::Result<(u16, Value)> {
    let route = cli_api_route(method, target, body.is_some())?;
    if let Some(body) = body {
        anyhow::ensure!(
            body.len() <= CLI_API_BODY_LIMIT,
            "body_too_large: the request body exceeds {CLI_API_BODY_LIMIT} bytes"
        );
        serde_json::from_str::<serde::de::IgnoredAny>(body)
            .map_err(|_| anyhow::anyhow!("invalid_request: the request body is not JSON"))?;
    }
    let browser = Arc::new(ProvisionedBrowser::for_socket(registry));
    let response: Response = match route {
        CliApiRoute::Checkouts => provisioned_checkouts(State(browser))
            .await
            .unwrap_or_else(ApiError::into_response),
        CliApiRoute::Checkout { root_key, suffix } => {
            let uri: Uri = target
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid_request: the route is not a valid URI"))?;
            let method = if method == "POST" {
                Method::POST
            } else {
                Method::GET
            };
            provisioned_core(
                State(browser),
                Path((root_key, suffix.to_owned())),
                method,
                uri,
                HeaderMap::new(),
                Bytes::from(body.unwrap_or_default().to_owned()),
            )
            .await
        }
    };
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), CLI_API_RESPONSE_LIMIT)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "response_too_large: the answer exceeds {} MiB",
                CLI_API_RESPONSE_LIMIT / (1024 * 1024)
            )
        })?;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid_response: the answer is not JSON"))?
    };
    Ok((status, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "ac9e11a3e2249dac06ff9805e1c51d81c1ccf10a3ae74eddbb9f441099566730";

    #[test]
    fn allowlist_is_exactly_the_mod_routes() {
        assert_eq!(
            cli_api_route("GET", "/api/checkouts", false).unwrap(),
            CliApiRoute::Checkouts
        );
        for (method, suffix, body) in [
            ("GET", "status", false),
            (
                "GET",
                "architecture?aggregate=max&role=production&workbench=true",
                false,
            ),
            ("GET", "methods?path=src%2Fmain.rs", false),
            ("POST", "sequence", true),
        ] {
            let target = format!("/api/checkouts/{KEY}/{suffix}");
            assert!(
                cli_api_route(method, &target, body).is_ok(),
                "{method} {target}"
            );
        }
        for (method, target) in [
            ("GET", format!("/api/checkouts/{KEY}/source?path=a")),
            ("GET", format!("/api/checkouts/{KEY}/git")),
            (
                "POST",
                format!("/api/checkouts/{KEY}/architecture/rules/apply"),
            ),
            ("POST", format!("/api/checkouts/{KEY}/index")),
            ("GET", format!("/api/checkouts/{KEY}/sequence")),
            ("GET", format!("/api/checkouts/{KEY}/status?x=1")),
            ("GET", "/api/checkouts?x=1".to_owned()),
            ("GET", "/api/daemon/status".to_owned()),
            ("GET", "/".to_owned()),
            ("GET", "/api/checkouts/ABC/status".to_owned()),
            ("GET", format!("/api/checkouts/{KEY}/../status")),
            ("DELETE", "/api/checkouts".to_owned()),
        ] {
            let error = cli_api_route(method, &target, method == "POST").unwrap_err();
            assert!(
                error.to_string().starts_with("route_not_allowed: "),
                "{method} {target}: {error}"
            );
        }
    }

    #[test]
    fn malformed_targets_and_bodies_are_typed() {
        for target in ["", "/api/checkouts two", "/api/checkouts#x"] {
            let error = cli_api_route("GET", target, false).unwrap_err();
            assert!(
                error.to_string().starts_with("invalid_request: "),
                "{target}"
            );
        }
        let long = format!(
            "/api/checkouts/{KEY}/methods?path={}",
            "a".repeat(TARGET_LIMIT)
        );
        assert!(cli_api_route("GET", &long, false).is_err());
        let error = cli_api_route("GET", "/api/checkouts", true).unwrap_err();
        assert!(error.to_string().starts_with("invalid_request: "));
        let target = format!("/api/checkouts/{KEY}/sequence");
        let error = cli_api_route("POST", &target, false).unwrap_err();
        assert!(error.to_string().starts_with("invalid_request: "));
    }
}
