use super::auto_config::{self, AutoConfig, ConfigState};
use std::{collections::BTreeMap, fs, os::unix::fs::symlink, path::Path, time::Duration};

fn known(name: &str) -> bool {
    matches!(name, "rust-analyzer" | "scip-python")
}

fn write(root: &Path, text: &str) {
    fs::create_dir_all(root.join(".trellis")).unwrap();
    fs::write(root.join(".trellis/producers.toml"), text).unwrap();
}

fn load_text(text: &str) -> ConfigState {
    let temp = tempfile::tempdir().unwrap();
    write(temp.path(), text);
    auto_config::load(temp.path(), known)
}

fn invalid(text: &str) -> &'static str {
    match load_text(text) {
        ConfigState::Invalid(reason) => reason,
        other => panic!("expected invalid, got {other:?}"),
    }
}

#[test]
fn defaults_apply_to_an_empty_table() {
    let ConfigState::Valid(tables) = load_text("[auto.rust-analyzer]\n") else {
        panic!("valid");
    };
    let config = tables["rust-analyzer"];
    assert_eq!(config, AutoConfig::default());
    assert!(!config.enabled);
    assert_eq!(config.quiet, Duration::from_secs(60));
    assert_eq!(config.min_interval, Duration::from_secs(600));
    assert!(config.battery_gate && config.load_gate);
}

#[test]
fn gate_switches_are_read_independently() {
    let ConfigState::Valid(tables) = load_text(
        "[auto.rust-analyzer]\nbatteryGate = false\n\
         [auto.scip-python]\nloadGate = false\n",
    ) else {
        panic!("valid");
    };
    assert!(!tables["rust-analyzer"].battery_gate);
    assert!(tables["rust-analyzer"].load_gate);
    assert!(tables["scip-python"].battery_gate);
    assert!(!tables["scip-python"].load_gate);
}

#[test]
fn a_non_boolean_gate_switch_is_a_schema_error() {
    for text in [
        "[auto.rust-analyzer]\nbatteryGate = \"off\"\n",
        "[auto.rust-analyzer]\nloadGate = 0\n",
    ] {
        assert_eq!(invalid(text), auto_config::SCHEMA, "{text}");
    }
}

#[test]
fn explicit_values_are_read() {
    let ConfigState::Valid(tables) = load_text(
        "[auto.rust-analyzer]\nenabled = true\nquietSecs = 3600\nminIntervalSecs = 86400\n\
         [auto.scip-python]\nenabled = false\nquietSecs = 1\nminIntervalSecs = 60\n",
    ) else {
        panic!("valid");
    };
    assert_eq!(
        tables["rust-analyzer"],
        AutoConfig {
            enabled: true,
            quiet: Duration::from_secs(3600),
            min_interval: Duration::from_secs(86_400),
            battery_gate: true,
            load_gate: true,
        }
    );
    assert_eq!(tables["scip-python"].quiet, Duration::from_secs(1));
    assert_eq!(tables["scip-python"].min_interval, Duration::from_secs(60));
}

#[test]
fn no_auto_table_is_valid_and_empty() {
    assert_eq!(load_text(""), ConfigState::Valid(BTreeMap::new()));
}

#[test]
fn missing_file_and_directory_read_as_missing() {
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(auto_config::load(temp.path(), known), ConfigState::Missing);
    fs::create_dir(temp.path().join(".trellis")).unwrap();
    assert_eq!(auto_config::load(temp.path(), known), ConfigState::Missing);
}

#[test]
fn a_file_over_64_kib_is_too_large() {
    let text = format!("# {}\n", "x".repeat(auto_config::PRODUCERS_CONFIG_BYTES));
    assert_eq!(invalid(&text), auto_config::TOO_LARGE);
}

#[test]
fn a_symlink_or_a_non_regular_file_is_unreadable() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join(".trellis")).unwrap();
    fs::write(temp.path().join("real.toml"), "").unwrap();
    symlink(
        temp.path().join("real.toml"),
        temp.path().join(".trellis/producers.toml"),
    )
    .unwrap();
    assert_eq!(
        auto_config::load(temp.path(), known),
        ConfigState::Invalid(auto_config::UNREADABLE)
    );

    let other = tempfile::tempdir().unwrap();
    fs::create_dir_all(other.path().join(".trellis/producers.toml")).unwrap();
    assert_eq!(
        auto_config::load(other.path(), known),
        ConfigState::Invalid(auto_config::UNREADABLE)
    );
}

#[test]
fn a_syntax_error_is_reported() {
    assert_eq!(invalid("[auto.rust-analyzer\n"), auto_config::SYNTAX);
}

#[test]
fn deep_nesting_is_a_syntax_error_without_a_panic() {
    let text = format!("a = {}1{}\n", "[".repeat(5000), "]".repeat(5000));
    assert_eq!(invalid(&text), auto_config::SYNTAX);
}

#[test]
fn unknown_keys_are_schema_errors() {
    for text in [
        "[auto.rust-analyzer]\nenabled = true\ntrustProfile = \"trusted\"\n",
        "[auto.rust-analyzer]\nbatterySaver = false\n",
        "[other]\nx = 1\n",
        "version = 1\n",
        "[auto.rust-analyzer]\nenabled = \"yes\"\n",
    ] {
        assert_eq!(invalid(text), auto_config::SCHEMA, "{text}");
    }
}

#[test]
fn a_negative_integer_is_a_schema_error() {
    assert_eq!(
        invalid("[auto.rust-analyzer]\nquietSecs = -1\n"),
        auto_config::SCHEMA
    );
}

#[test]
fn an_unknown_producer_is_rejected() {
    assert_eq!(
        invalid("[auto.not-a-producer]\nenabled = true\n"),
        auto_config::UNKNOWN_PRODUCER
    );
}

#[test]
fn quiet_secs_out_of_range_is_rejected_not_clamped() {
    for value in [0, 3601] {
        assert_eq!(
            invalid(&format!("[auto.rust-analyzer]\nquietSecs = {value}\n")),
            auto_config::QUIET_RANGE
        );
    }
}

#[test]
fn min_interval_secs_out_of_range_is_rejected_not_clamped() {
    for value in [59, 86_401] {
        assert_eq!(
            invalid(&format!(
                "[auto.rust-analyzer]\nminIntervalSecs = {value}\n"
            )),
            auto_config::INTERVAL_RANGE
        );
    }
}

#[test]
fn one_invalid_table_invalidates_every_producer() {
    assert_eq!(
        invalid("[auto.rust-analyzer]\nenabled = true\n[auto.scip-python]\nquietSecs = 0\n"),
        auto_config::QUIET_RANGE
    );
}
