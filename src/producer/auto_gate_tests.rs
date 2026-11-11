use super::auto_config::AutoConfig;
use super::auto_gate::*;
use std::{fs, path::Path};

fn supply(root: &Path, name: &str, kind: &str, online: Option<&str>) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
    if let Some(online) = online {
        fs::write(dir.join("online"), format!("{online}\n")).unwrap();
    }
}

#[test]
fn mains_offline_is_on_battery() {
    let temp = tempfile::tempdir().unwrap();
    supply(temp.path(), "BAT0", "Battery", None);
    supply(temp.path(), "AC", "Mains", Some("0"));
    assert_eq!(power_from_dir(temp.path()), Some(true));
}

#[test]
fn any_mains_online_is_ac() {
    let temp = tempfile::tempdir().unwrap();
    supply(temp.path(), "AC0", "Mains", Some("0"));
    supply(temp.path(), "AC1", "Mains", Some("1"));
    assert_eq!(power_from_dir(temp.path()), Some(false));
}

#[test]
fn no_mains_supply_or_directory_is_unknown() {
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(power_from_dir(temp.path()), None);
    supply(temp.path(), "BAT0", "Battery", None);
    assert_eq!(power_from_dir(temp.path()), None);
    assert_eq!(power_from_dir(&temp.path().join("absent")), None);
}

#[test]
fn a_file_past_64_bytes_is_cut_off() {
    let temp = tempfile::tempdir().unwrap();
    supply(temp.path(), "AC", "Mains", None);
    // Within the cap the file reads "1" (trimmed); the 64-byte cut drops the trailing "x", which
    // would otherwise make the text "1...x" and not online.
    fs::write(
        temp.path().join("AC/online"),
        format!("1{}x", " ".repeat(63)),
    )
    .unwrap();
    assert_eq!(power_from_dir(temp.path()), Some(false));
}

#[test]
fn only_the_first_64_entries_are_read() {
    let temp = tempfile::tempdir().unwrap();
    for index in 0..70 {
        supply(temp.path(), &format!("s{index:02}"), "Battery", None);
    }
    // Make the 65th entry in the file system's own listing order the only mains supply. A walk
    // capped at 64 entries never reaches it (unknown); an uncapped walk finds it offline.
    let listed: Vec<_> = fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    supply(
        temp.path(),
        listed[64].to_str().unwrap(),
        "Mains",
        Some("0"),
    );
    assert_eq!(power_from_dir(temp.path()), None);
}

#[test]
fn saturated_compares_load_with_cpu_count() {
    assert!(saturated(4.0, 4));
    assert!(saturated(9.5, 4));
    assert!(!saturated(3.99, 4));
    assert!(!saturated(0.0, 1));
}

#[test]
fn macos_source_types_classify() {
    assert_eq!(classify_source("Battery Power"), Some(true));
    assert_eq!(classify_source("AC Power"), Some(false));
    assert_eq!(classify_source("UPS Power"), Some(false));
    assert_eq!(classify_source("Mystery"), None);
}

#[test]
fn system_probes_do_not_panic() {
    let _ = SystemProbes.on_battery();
    let _ = SystemProbes.load_saturated();
}

#[test]
fn probe_file_parses_booleans_and_nulls() {
    assert_eq!(
        parse_probe_file(r#"{"battery":true,"load":false}"#),
        (Some(true), Some(false))
    );
    assert_eq!(parse_probe_file(r#"{"battery":null}"#), (None, None));
    assert_eq!(
        parse_probe_file(r#"{"battery":"yes","load":1}"#),
        (None, None)
    );
    assert_eq!(parse_probe_file("not json"), (None, None));
}

#[test]
fn a_missing_probe_file_is_unknown_and_is_reread() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("probes.json");
    let probes = FileProbes(path.clone());
    assert_eq!((probes.on_battery(), probes.load_saturated()), (None, None));
    fs::write(&path, r#"{"battery":true,"load":null}"#).unwrap();
    assert_eq!(probes.on_battery(), Some(true));
    assert_eq!(probes.load_saturated(), None);
    fs::write(&path, r#"{"battery":false,"load":true}"#).unwrap();
    assert_eq!(probes.on_battery(), Some(false));
    assert_eq!(probes.load_saturated(), Some(true));
}

struct Fixed(Option<bool>, Option<bool>);

impl Probes for Fixed {
    fn on_battery(&self) -> Option<bool> {
        self.0
    }

    fn load_saturated(&self) -> Option<bool> {
        self.1
    }
}

#[test]
fn check_tries_battery_first_and_honours_the_switches() {
    let cfg = AutoConfig::default();
    assert_eq!(
        check(&cfg, &Fixed(Some(true), Some(true))),
        Some(Gate::Battery)
    );
    assert_eq!(
        check(&cfg, &Fixed(Some(false), Some(true))),
        Some(Gate::Load)
    );
    assert_eq!(check(&cfg, &Fixed(None, None)), None);
    let no_battery = AutoConfig {
        battery_gate: false,
        ..cfg
    };
    assert_eq!(
        check(&no_battery, &Fixed(Some(true), Some(true))),
        Some(Gate::Load)
    );
    let neither = AutoConfig {
        load_gate: false,
        ..no_battery
    };
    assert_eq!(check(&neither, &Fixed(Some(true), Some(true))), None);
}
