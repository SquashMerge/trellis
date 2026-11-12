//! `.trellis/producers.toml` (#660, SD1, SD3): the per-producer opt-in to automatic re-runs.
//! The file can only enable and tune a re-run. It has no trust, executable or limit key: what a
//! run may do comes from the approval a manual run left (`approval.rs`). A bound that is out of
//! range rejects the whole file; nothing is clamped.

use serde::Deserialize;
use std::{collections::BTreeMap, io::ErrorKind, path::Path, time::Duration};

/// Where the file lives, relative to the checkout root.
const PATH: &str = ".trellis/producers.toml";
/// The most bytes read from the file.
pub(crate) const PRODUCERS_CONFIG_BYTES: usize = 65_536;

const DEFAULT_QUIET_SECS: u64 = 60;
const DEFAULT_MIN_INTERVAL_SECS: u64 = 600;
const QUIET_SECS: std::ops::RangeInclusive<u64> = 1..=3600;
const MIN_INTERVAL_SECS: std::ops::RangeInclusive<u64> = 60..=86_400;

pub(crate) const TOO_LARGE: &str = "producers.toml too large";
pub(crate) const UNREADABLE: &str = "producers.toml unreadable";
pub(crate) const SYNTAX: &str = "producers.toml syntax error";
pub(crate) const SCHEMA: &str = "producers.toml schema error";
pub(crate) const UNKNOWN_PRODUCER: &str = "producers.toml unknown producer";
pub(crate) const QUIET_RANGE: &str = "producers.toml quietSecs out of range";
pub(crate) const INTERVAL_RANGE: &str = "producers.toml minIntervalSecs out of range";

/// One producer's `[auto.<producer>]` table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AutoConfig {
    pub enabled: bool,
    /// How long the head must stay unchanged before a run may start.
    pub quiet: Duration,
    /// The least time between two automatic starts.
    pub min_interval: Duration,
    /// Defer a start while the host runs on battery.
    pub battery_gate: bool,
    /// Defer a start while the host's load is saturated.
    pub load_gate: bool,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            quiet: Duration::from_secs(DEFAULT_QUIET_SECS),
            min_interval: Duration::from_secs(DEFAULT_MIN_INTERVAL_SECS),
            battery_gate: true,
            load_gate: true,
        }
    }
}

/// What reading the file found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConfigState {
    /// No file: every producer is off.
    Missing,
    /// The tables of the producers that have one.
    Valid(BTreeMap<String, AutoConfig>),
    /// The file was refused, for one of the closed reasons above.
    Invalid(&'static str),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(default)]
    auto: BTreeMap<String, RawAuto>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawAuto {
    enabled: Option<bool>,
    quiet_secs: Option<u64>,
    min_interval_secs: Option<u64>,
    battery_gate: Option<bool>,
    load_gate: Option<bool>,
}

/// Read and validate the file of the checkout at `root`. `known` says whether a producer name
/// is one this daemon runs.
pub(crate) fn load(root: &Path, known: impl Fn(&str) -> bool) -> ConfigState {
    let text =
        match crate::file_tree::SourceDir::open(root).and_then(|source| source.read_file(PATH)) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return ConfigState::Missing,
            Err(error) if error.kind() == ErrorKind::FileTooLarge => {
                return ConfigState::Invalid(TOO_LARGE);
            }
            Err(_) => return ConfigState::Invalid(UNREADABLE),
        };
    match parse(&text, known) {
        Ok(tables) => ConfigState::Valid(tables),
        Err(reason) => ConfigState::Invalid(reason),
    }
}

fn parse(
    text: &str,
    known: impl Fn(&str) -> bool,
) -> Result<BTreeMap<String, AutoConfig>, &'static str> {
    if text.len() > PRODUCERS_CONFIG_BYTES {
        return Err(TOO_LARGE);
    }
    let table = text.parse::<toml::Table>().map_err(|_| SYNTAX)?;
    let raw: Raw = table.try_into().map_err(|_| SCHEMA)?;
    let mut tables = BTreeMap::new();
    for (name, auto) in raw.auto {
        if !known(&name) {
            return Err(UNKNOWN_PRODUCER);
        }
        let quiet = auto.quiet_secs.unwrap_or(DEFAULT_QUIET_SECS);
        if !QUIET_SECS.contains(&quiet) {
            return Err(QUIET_RANGE);
        }
        let interval = auto.min_interval_secs.unwrap_or(DEFAULT_MIN_INTERVAL_SECS);
        if !MIN_INTERVAL_SECS.contains(&interval) {
            return Err(INTERVAL_RANGE);
        }
        tables.insert(
            name,
            AutoConfig {
                enabled: auto.enabled.unwrap_or(false),
                quiet: Duration::from_secs(quiet),
                min_interval: Duration::from_secs(interval),
                battery_gate: auto.battery_gate.unwrap_or(true),
                load_gate: auto.load_gate.unwrap_or(true),
            },
        );
    }
    Ok(tables)
}
