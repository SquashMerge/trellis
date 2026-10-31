use super::detect::{Declared, Environment, Exe, Resolution, Verification};
use super::profiles::{
    self, Component, Decision, Enforcement, GenerationTable, Row, Stub, Target, rows,
};
use super::{ProducerName, TrustProfile};
use std::path::{Path, PathBuf};

const RUN: &str = "/state/runs/{runId}";

fn exe(path: &str, dir: &str) -> Exe {
    Exe {
        path: PathBuf::from(path),
        sha256: "ab".repeat(32),
        dir: PathBuf::from(dir),
    }
}

fn resolution() -> Resolution {
    Resolution {
        launcher: PathBuf::from("/opt/launcher"),
        verification: Verification {
            method: "testStub",
            observed: String::new(),
        },
        node: Some(exe("/nodes/real/node", "/nodes/bin")),
        java: Some(exe("/jdk/bin/java", "/jdk/bin")),
        java_home: Some(PathBuf::from("/jdk")),
        rustup: Some(exe("/opt/launcher", "/cargo/bin")),
        python3: Some(exe("/py/bin/python3", "/py/bin")),
    }
}

fn declared() -> Declared {
    Declared::new(&Environment {
        path: "/nodes/bin:/usr/bin".into(),
        home: "/home/dev".into(),
        ..Environment::default()
    })
    .unwrap()
}

fn row(producer: ProducerName, trust: TrustProfile) -> Row {
    profiles::row(Target::Real(producer), trust).unwrap()
}

fn spec(row: &Row) -> profiles::CommandSpec {
    let stub = Stub {
        path: PathBuf::from("/stub"),
        args: vec!["--mode-file".into(), "/mode.json".into()],
    };
    let pin = row.target.pin().unwrap();
    profiles::command(row, &resolution(), &declared(), Some(&stub), &pin, RUN)
}

fn keys(row: &Row) -> Vec<String> {
    spec(row).env.into_keys().collect()
}

#[test]
fn profile_commands_are_closed_table() {
    let out = format!("{RUN}/out/index.scip");
    let ts = spec(&row(ProducerName::ScipTypescript, TrustProfile::Restricted));
    assert_eq!(
        ts.argv,
        [
            "/nodes/real/node",
            "/opt/launcher",
            "index",
            "--infer-tsconfig",
            "--output",
            &out,
            "--no-progress-bar"
        ]
    );
    let java = spec(&row(ProducerName::ScipJava, TrustProfile::Trusted));
    assert_eq!(java.argv, ["/opt/launcher", "index", "--output", &out]);
    let rust = spec(&row(ProducerName::RustAnalyzer, TrustProfile::Trusted));
    assert_eq!(
        rust.argv,
        [
            "/opt/launcher",
            "run",
            "1.98.1",
            "rust-analyzer",
            "scip",
            ".",
            "--output",
            &out
        ]
    );
    let stub = spec(&profiles::row(Target::Stub, TrustProfile::Restricted).unwrap());
    assert_eq!(
        stub.argv,
        [
            "/opt/launcher",
            "--mode-file",
            "/mode.json",
            "index",
            "--output",
            &out
        ]
    );
    let py = spec(&row(ProducerName::ScipPython, TrustProfile::Trusted));
    assert!(!py.argv.contains(&"--environment".to_owned()));
    let version = py
        .argv
        .iter()
        .position(|a| a == "--project-version")
        .unwrap();
    assert_eq!(py.argv[version + 1], "0");
}

#[test]
fn commands_are_headless_batch_subcommands() {
    for row in rows(true) {
        let argv = spec(&row).argv;
        assert!(argv.iter().any(|a| a == "index" || a == "scip"), "{argv:?}");
        assert!(argv.iter().any(|a| a == "--output"));
        assert!(!argv.iter().any(|a| a.contains("ide") || a.contains("open")));
    }
}

#[test]
fn restricted_profiles_set_offline_flags() {
    let restricted = spec(&row(ProducerName::ScipJava, TrustProfile::Restricted));
    assert_eq!(restricted.env["COURSIER_MODE"], "offline");
    assert!(!restricted.env.contains_key("GRADLE_OPTS"));
    let trusted = spec(&row(ProducerName::ScipJava, TrustProfile::Trusted));
    assert!(!trusted.env.contains_key("COURSIER_MODE"));
}

#[test]
fn trusted_java_disables_gradle_daemon() {
    let trusted = spec(&row(ProducerName::ScipJava, TrustProfile::Trusted));
    assert_eq!(trusted.env["GRADLE_OPTS"], "-Dorg.gradle.daemon=false");
    assert_eq!(trusted.env["JAVA_HOME"], "/jdk");
}

#[test]
fn rust_analyzer_disables_rustup_auto_install() {
    let rust = spec(&row(ProducerName::RustAnalyzer, TrustProfile::Trusted));
    assert_eq!(rust.env["RUSTUP_AUTO_INSTALL"], "0");
    assert_eq!(rust.env["CARGO_TARGET_DIR"], format!("{RUN}/target"));
    assert_eq!(rust.env["PATH"], "/cargo/bin:/usr/bin:/bin");
}

#[test]
fn rust_restricted_is_refused() {
    let error = profiles::row(
        Target::Real(ProducerName::RustAnalyzer),
        TrustProfile::Restricted,
    )
    .unwrap_err()
    .to_string();
    assert_eq!(
        error,
        "profile_unsupported: rust-analyzer cannot disable build scripts or proc macros"
    );
    assert!(
        profiles::refusal(
            Target::Real(ProducerName::RustAnalyzer),
            TrustProfile::Trusted
        )
        .is_none()
    );
    let trusted = profiles::policies(&row(ProducerName::RustAnalyzer, TrustProfile::Trusted));
    assert_eq!(trusted.rust_build_scripts.decision, Decision::Allow);
    assert_eq!(trusted.rust_proc_macros.decision, Decision::Allow);
}

#[test]
fn restricted_java_reports_processors_structural() {
    let restricted = profiles::policies(&row(ProducerName::ScipJava, TrustProfile::Restricted));
    for policy in [
        &restricted.annotation_processors,
        &restricted.java_build_logic,
    ] {
        assert_eq!(policy.decision, Decision::Deny);
        assert_eq!(policy.enforcement, Enforcement::Structural);
    }
    assert_eq!(restricted.network.enforcement, Enforcement::Advisory);
    let trusted = profiles::policies(&row(ProducerName::ScipJava, TrustProfile::Trusted));
    assert_eq!(trusted.annotation_processors.decision, Decision::Allow);
    assert!(trusted.network.detail.contains("download"));
}

#[test]
fn javascript_profiles_invoke_no_package_manager() {
    for producer in [ProducerName::ScipTypescript, ProducerName::ScipPython] {
        for trust in [TrustProfile::Restricted, TrustProfile::Trusted] {
            let row = row(producer, trust);
            let argv = spec(&row).argv;
            for manager in ["npm", "npx", "yarn", "pnpm", "bun"] {
                assert!(!argv.iter().any(|a| a == manager));
            }
            let policies = profiles::policies(&row);
            assert_eq!(
                policies.javascript_lifecycle_scripts.decision,
                Decision::Deny
            );
            assert_eq!(
                policies.javascript_lifecycle_scripts.enforcement,
                Enforcement::Structural
            );
            assert_eq!(policies.package_manager.decision, Decision::Deny);
        }
    }
}

#[test]
fn python_restricted_child_path_resolves_no_interpreter() {
    let row = row(ProducerName::ScipPython, TrustProfile::Restricted);
    let spec = spec(&row);
    assert_eq!(spec.env["PATH"], format!("{RUN}/bin"));
    assert_eq!(spec.bin_links.len(), 1);
    assert_eq!(spec.bin_links[0].name, "node");
    assert_eq!(spec.bin_links[0].target, Path::new("/nodes/real/node"));
    let rules = profiles::rules(&row);
    assert_eq!(rules.omit[0].path, "pyproject.toml");
    assert_eq!(rules.generated[0].bytes, b"[]");
    assert!(
        spec.argv
            .contains(&format!("{RUN}/producer-config/environment.json"))
    );
}

#[test]
fn python_trusted_exposes_declared_interpreter() {
    let row = row(ProducerName::ScipPython, TrustProfile::Trusted);
    let spec = spec(&row);
    assert_eq!(spec.env["PATH"], "/py/bin:/nodes/bin:/usr/bin:/bin");
    assert!(spec.bin_links.is_empty());
    assert!(profiles::rules(&row).omit.is_empty());
    assert_eq!(
        profiles::policies(&row)
            .python_environment_inspection
            .decision,
        Decision::Allow
    );
}

#[test]
fn child_env_keys_per_profile_are_closed() {
    let base = ["HOME", "LANG", "PATH", "TMPDIR"];
    let with = |extra: &[&str]| {
        let mut all: Vec<String> = base.iter().chain(extra).map(|s| (*s).to_owned()).collect();
        all.sort();
        all
    };
    let ts = row(ProducerName::ScipTypescript, TrustProfile::Restricted);
    assert_eq!(keys(&ts), with(&[]));
    assert_eq!(
        keys(&row(ProducerName::ScipPython, TrustProfile::Trusted)),
        with(&[])
    );
    assert_eq!(
        keys(&profiles::row(Target::Stub, TrustProfile::Trusted).unwrap()),
        with(&[])
    );
    assert_eq!(
        keys(&row(ProducerName::ScipJava, TrustProfile::Restricted)),
        with(&[
            "COURSIER_ARCHIVE_CACHE",
            "COURSIER_CACHE",
            "COURSIER_MODE",
            "JAVA_HOME"
        ])
    );
    assert_eq!(
        keys(&row(ProducerName::ScipJava, TrustProfile::Trusted)),
        with(&[
            "COURSIER_ARCHIVE_CACHE",
            "COURSIER_CACHE",
            "GRADLE_OPTS",
            "JAVA_HOME"
        ])
    );
    assert_eq!(
        keys(&row(ProducerName::RustAnalyzer, TrustProfile::Trusted)),
        with(&[
            "CARGO_HOME",
            "CARGO_TARGET_DIR",
            "RUSTUP_AUTO_INSTALL",
            "RUSTUP_HOME"
        ])
    );
    for row in rows(true) {
        let spec = spec(&row);
        assert_eq!(spec.env["HOME"], format!("{RUN}/home"));
        assert_eq!(spec.env["TMPDIR"], format!("{RUN}/tmp"));
        assert!(
            spec.env["PATH"].ends_with("/usr/bin:/bin") || spec.env["PATH"] == format!("{RUN}/bin")
        );
    }
}

fn consistent_tables() -> Vec<(String, GenerationTable)> {
    let mut tables: Vec<_> = rows(true)
        .iter()
        .map(|row| {
            (
                format!("{} {:?}", row.target.name(), row.trust),
                profiles::run_generation(row, &resolution()),
            )
        })
        .collect();
    tables.push(("pickup".into(), profiles::pickup_generation()));
    tables
}

#[test]
fn observed_and_asserted_partition_components() {
    for (name, table) in consistent_tables() {
        assert!(table.is_consistent(), "{name}");
        let mut all: Vec<_> = table.observed.iter().chain(&table.asserted).collect();
        all.sort();
        assert_eq!(all.len(), 7, "{name}");
    }
    let mut broken = profiles::pickup_generation();
    broken.observed.push(Component::Source);
    assert!(!broken.is_consistent());
}

#[test]
fn runtime_list_nonempty_iff_runtime_observed() {
    for (name, table) in consistent_tables() {
        assert_eq!(
            !table.runtime.is_empty(),
            table.observed.contains(&Component::Runtime),
            "{name}"
        );
    }
    let node = profiles::run_generation(
        &row(ProducerName::ScipTypescript, TrustProfile::Restricted),
        &resolution(),
    );
    assert_eq!(node.runtime.len(), 1);
    assert_eq!(node.runtime[0].name, "node");
    assert_eq!(
        node.observed,
        [
            Component::Source,
            Component::Config,
            Component::DependencyManifests,
            Component::ProducerVersion,
            Component::Runtime
        ]
    );
    for producer in [ProducerName::ScipJava, ProducerName::RustAnalyzer] {
        let table = profiles::run_generation(&row(producer, TrustProfile::Trusted), &resolution());
        assert!(table.runtime.is_empty());
        assert!(table.asserted.contains(&Component::Runtime));
        assert!(table.observed.contains(&Component::ProducerExecutable));
    }
    let stub = profiles::run_generation(
        &profiles::row(Target::Stub, TrustProfile::Restricted).unwrap(),
        &resolution(),
    );
    assert_eq!(stub.observed.len(), 3);
    assert!(profiles::pickup_generation().observed.is_empty());
}
