//! Metrics options decoded from a daemon request, and the test-support metrics hooks.
use crate::validate_cli_metrics_options;
use anyhow::{Context, Result, ensure};
#[cfg(feature = "test-support")]
use std::time::Duration;

#[cfg(feature = "test-support")]
pub(super) fn metrics_test_duration_file(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
}

#[cfg(feature = "test-support")]
pub(super) fn metrics_test_phase(mark: &[u8]) {
    use std::io::Write;
    if let Ok(path) = std::env::var("TRELLIS_TEST_METRICS_PHASE_ORDER_FILE")
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = file.write_all(mark);
    }
}

pub(super) fn metrics_options_from_request(
    args: &serde_json::Value,
) -> Result<trellis::metrics_read::MetricsOptions> {
    fn field<T: serde::de::DeserializeOwned>(args: &serde_json::Value, name: &str) -> Result<T> {
        serde_json::from_value(args[name].clone())
            .map_err(|error| anyhow::anyhow!("invalid_argument: {name}: {error}"))
    }
    let fields = args
        .as_object()
        .context("invalid_argument: metrics args object")?;
    ensure!(
        fields.keys().all(|key| matches!(
            key.as_str(),
            "scope"
                | "aggregate"
                | "role"
                | "sourceSetId"
                | "language"
                | "path"
                | "sid"
                | "metrics"
                | "pin"
        )),
        "invalid_argument: unknown metrics option"
    );
    ensure!(
        fields
            .get("metrics")
            .is_none_or(|value| value.as_array().is_some_and(|names| !names.is_empty())),
        "invalid_argument: empty metrics selection"
    );
    let options = trellis::metrics_read::MetricsOptions {
        workbench: false,
        scope: args["scope"]
            .as_str()
            .context("invalid_argument: scope")?
            .to_owned(),
        aggregate: args["aggregate"]
            .as_str()
            .context("invalid_argument: aggregate")?
            .to_owned(),
        role: if fields.contains_key("role") {
            field(args, "role")?
        } else {
            "all".into()
        },
        source_set_id: field(args, "sourceSetId")?,
        language: field(args, "language")?,
        path: field(args, "path")?,
        sid: field(args, "sid")?,
        metrics: if args.get("metrics").is_some() {
            field(args, "metrics")?
        } else {
            vec![]
        },
        pin: field(args, "pin")?,
        top_cognitive: None,
    };
    validate_cli_metrics_options(&options)?;
    Ok(options)
}

#[cfg(test)]
mod metrics_request_tests {
    use super::metrics_options_from_request;
    use serde_json::json;

    #[test]
    fn role_selection_defaults_and_strict_daemon_option_validation() {
        let args = json!({"scope": "revision", "aggregate": "sum"});
        assert_eq!(metrics_options_from_request(&args).unwrap().role, "all");
        for role in ["production", "all", "test"] {
            let mut selected = args.clone();
            selected["role"] = json!(role);
            assert_eq!(metrics_options_from_request(&selected).unwrap().role, role);
        }
        for role in [
            json!(""),
            json!("unknown"),
            json!("Production"),
            json!(null),
            json!(true),
            json!(["all"]),
        ] {
            let mut invalid = args.clone();
            invalid["role"] = role;
            let error = metrics_options_from_request(&invalid).unwrap_err();
            assert!(error.to_string().contains("invalid_argument"), "{error}");
        }
        let mut unknown = args;
        unknown["roles"] = json!("production");
        assert!(
            metrics_options_from_request(&unknown)
                .unwrap_err()
                .to_string()
                .contains("unknown metrics option")
        );
    }

    /// The shared identifier rules: both published source-set forms (#984)
    /// and the first failing rule's message.
    #[test]
    fn identifier_rules_are_shared_and_accept_both_source_set_forms() {
        let path_id = format!("source-set:v1:path-{}", "a".repeat(64));
        let uuid_id = "source-set:v1:6f1f0a52-0d0c-4d6e-9a43-0c1a3e3a8b11";
        let file = |id: &str| {
            json!({"scope": "file", "aggregate": "sum", "sourceSetId": id,
                   "path": "src/a.rs", "language": "rust"})
        };
        for id in [path_id.as_str(), uuid_id] {
            let options = metrics_options_from_request(&file(id)).unwrap();
            assert_eq!(options.source_set_id.as_deref(), Some(id));
        }
        let rejected =
            |args: serde_json::Value| metrics_options_from_request(&args).unwrap_err().to_string();
        for invalid in [
            "garbage",
            "source-set:v2:6f1f0a52-0d0c-4d6e-9a43-0c1a3e3a8b11",
            "source-set:v1:6F1F0A52-0D0C-4D6E-9A43-0C1A3E3A8B11",
            "source-set:v1:6f1f0a520d0c4d6e9a430c1a3e3a8b11",
            "source-set:v1:path-abc",
        ] {
            assert_eq!(
                rejected(file(invalid)),
                "invalid_argument: source set ID",
                "{invalid}"
            );
        }
        let mut args = file(&path_id);
        args["path"] = json!("src/a:b.rs");
        assert_eq!(rejected(args.clone()), "invalid_argument: path");
        // Source set ID is reported ahead of a bad path or language.
        args["sourceSetId"] = json!(path_id.to_uppercase());
        assert_eq!(rejected(args), "invalid_argument: source set ID");
        let mut args = file(&path_id);
        args["language"] = json!("go");
        assert_eq!(rejected(args), "invalid_argument: language");
        let mut args = file(&path_id);
        args["scope"] = json!("declaration");
        args.as_object_mut().unwrap().remove("language");
        args["sid"] = json!(format!("sid:v1:{}", "A".repeat(32)));
        assert_eq!(rejected(args), "invalid_argument: sid");
    }
}
