//! `trellis api`: one authenticated request to the daemon's browser API, and its output.
use super::transport::{daemon_request, send_daemon};
use crate::ApiArgs;
use anyhow::{Context, Result, ensure};

/// `trellis api`: refuses before reaching the daemon, so a refused route starts nothing.
pub(crate) fn api_command(args: &ApiArgs) -> i32 {
    use std::io::{Read, Write};
    let outcome = (|| -> Result<(u16, serde_json::Value)> {
        let body = match args.body.as_deref() {
            None => None,
            Some(text) if text != "-" => {
                let limit = trellis::http::CLI_API_BODY_LIMIT;
                ensure!(
                    text.len() <= limit,
                    "body_too_large: the request body exceeds {limit} bytes"
                );
                Some(text.to_owned())
            }
            Some(_) => {
                let limit = trellis::http::CLI_API_BODY_LIMIT;
                let mut bytes = Vec::new();
                std::io::stdin()
                    .lock()
                    .take(limit as u64 + 1)
                    .read_to_end(&mut bytes)
                    .context("invalid_request: cannot read the body from stdin")?;
                ensure!(
                    bytes.len() <= limit,
                    "body_too_large: the request body exceeds {limit} bytes"
                );
                Some(String::from_utf8(bytes).context("invalid_request: the body is not UTF-8")?)
            }
        };
        trellis::http::cli_api_route(&args.method, &args.target, body.is_some())?;
        let reply = send_daemon(
            &daemon_request(
                "api",
                serde_json::json!({"method": args.method, "target": args.target, "body": body}),
            ),
            true,
        )
        .map_err(|error| {
            // A daemon started before `api` existed reads it as a checkout operation.
            if error.to_string() == "missing workspace" {
                anyhow::anyhow!(
                    "daemon_outdated: the running daemon predates trellis api; restart it (trellis daemon stop, then trellis serve)"
                )
            } else {
                error
            }
        })?;
        let status = reply["status"]
            .as_u64()
            .filter(|status| (100..600).contains(status))
            .context("invalid_response: the daemon's answer has no status")?;
        Ok((status as u16, reply["body"].clone()))
    })();
    let (value, code) = match outcome {
        Ok((status, body)) => (body, i32::from(!(200..300).contains(&status))),
        Err(error) => {
            let text = format!("{error:#}");
            let (code, message) = match text.split_once(": ") {
                Some((code, message))
                    if !code.is_empty()
                        && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
                {
                    (code.to_owned(), message.to_owned())
                }
                _ if text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') => {
                    (text.clone(), format!("The daemon call failed ({text})."))
                }
                _ => ("api_failed".to_owned(), text),
            };
            (
                serde_json::json!({"error": {"code": code, "message": message}}),
                2,
            )
        }
    };
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(&api_output(&value, args.gzip));
    let _ = stdout.flush();
    code
}

/// What `trellis api` prints: one line of JSON, or with `--gzip` one line of base64 of that
/// line gzip-compressed (#915).
fn api_output(value: &serde_json::Value, gzip: bool) -> Vec<u8> {
    use base64::Engine;
    use std::io::Write;
    let mut line = serde_json::to_vec(value).unwrap_or_default();
    line.push(b'\n');
    if !gzip {
        return line;
    }
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let compressed = encoder
        .write_all(&line)
        .and_then(|()| encoder.finish())
        .unwrap_or_default();
    let mut text = base64::engine::general_purpose::STANDARD
        .encode(compressed)
        .into_bytes();
    text.push(b'\n');
    text
}
