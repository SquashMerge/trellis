//! `trellis producer doctor` and `trellis producer install` (#880) through the real CLI.
//!
//! Every producer, runtime and package manager here is a fake on an isolated `PATH`, under a
//! temporary `HOME`; no test runs a real package manager, rustup or coursier.
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use trellis::producer::{ProducerName, doctor};

/// A Mach-O header: resolution accepts it as a native `node` without running it.
const MACHO: [u8; 8] = [0xcf, 0xfa, 0xed, 0xfe, 0, 0, 0, 0];
const TS_INSTALL: &str = "npm install -g @sourcegraph/scip-typescript@0.4.0";

fn write_exe(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A host with nothing installed: an empty declared `PATH` directory, a temporary `HOME` and
/// a checkout holding the given sources.
struct Host {
    dir: tempfile::TempDir,
}

impl Host {
    fn new(sources: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let host = Self { dir };
        fs::create_dir_all(host.bin()).unwrap();
        fs::create_dir_all(host.home()).unwrap();
        fs::create_dir_all(host.checkout().join(".git")).unwrap();
        for source in sources {
            let path = host.checkout().join(source);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "\n").unwrap();
        }
        host
    }

    fn root(&self) -> PathBuf {
        fs::canonicalize(self.dir.path()).unwrap()
    }
    fn bin(&self) -> PathBuf {
        self.root().join("prefix/bin")
    }
    fn home(&self) -> PathBuf {
        self.root().join("home")
    }
    fn checkout(&self) -> PathBuf {
        self.root().join("checkout")
    }
    fn record(&self) -> PathBuf {
        self.root().join("record")
    }

    /// `prefix/bin/<package>` linked into the npm global package at `version`.
    fn npm_package(&self, package: &str, version: &str) {
        let root = self
            .root()
            .join("prefix/lib/node_modules/@sourcegraph")
            .join(package);
        write_exe(&root.join("dist/src/main.js"), b"//");
        fs::write(
            root.join("package.json"),
            json!({"name": format!("@sourcegraph/{package}"), "version": version}).to_string(),
        )
        .unwrap();
        std::os::unix::fs::symlink(root.join("dist/src/main.js"), self.bin().join(package))
            .unwrap();
    }

    fn node(&self) {
        write_exe(&self.bin().join("node"), &MACHO);
    }

    /// A fake tool that appends its argv, one per line, then `--`, to the record file, and
    /// then runs `then`.
    fn recording_tool(&self, name: &str, then: &str) {
        let script = format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{record}'; done\n\
             printf '%s RUSTUP_AUTO_INSTALL=%s\\n' '--' \"$RUSTUP_AUTO_INSTALL\" >> '{record}'\n{then}\n",
            record = self.record().display()
        );
        write_exe(&self.bin().join(name), script.as_bytes());
    }

    fn recorded(&self) -> Option<String> {
        fs::read_to_string(self.record()).ok()
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_trellis"))
            .args(args)
            .env_clear()
            .env("PATH", self.bin())
            .env("HOME", self.home())
            .current_dir(self.checkout())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn doctor(&self) -> Value {
        let output = self.cli(&["producer", "doctor", "--json"]);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn entry<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["producers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn pinned_install_commands_come_from_the_profile_pins() {
    let command = |producer| doctor::pinned_install(producer).unwrap().display();
    assert_eq!(command(ProducerName::ScipTypescript), TS_INSTALL);
    assert_eq!(
        command(ProducerName::ScipPython),
        "npm install -g @sourcegraph/scip-python@0.6.6"
    );
    assert_eq!(
        command(ProducerName::ScipJava),
        "coursier bootstrap --standalone com.sourcegraph:scip-java_2.13:0.12.3"
    );
    assert_eq!(
        command(ProducerName::RustAnalyzer),
        "rustup component add rust-analyzer --toolchain 1.98.1"
    );
}

#[test]
fn nothing_installed_lists_every_relevant_producer_missing_with_its_command() {
    let host = Host::new(&["src/a.ts", "b.py", "C.java", "lib.rs"]);
    let output = host.cli(&["producer", "doctor"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = text(&output.stdout);
    assert!(stdout.contains("native (syntax) evidence only"), "{stdout}");
    assert!(
        stdout.contains(&format!("install: {TS_INSTALL}")),
        "{stdout}"
    );

    let report = host.doctor();
    assert_eq!(
        report["languages"],
        json!(["java", "python", "rust", "typescript"])
    );
    for producer in ProducerName::ALL {
        let entry = entry(&report, producer.as_str());
        assert_eq!(entry["relevant"], true, "{entry}");
        assert_eq!(entry["state"], "missing", "{entry}");
        assert_eq!(
            entry["installCommand"],
            doctor::pinned_install(producer).unwrap().display(),
            "{entry}"
        );
    }
    assert_eq!(entry(&report, "rust-analyzer")["trustProfile"], "trusted");
    assert_eq!(entry(&report, "scip-java")["trustProfile"], "restricted");
}

#[test]
fn only_the_checkouts_languages_are_relevant() {
    let host = Host::new(&["src/a.ts"]);
    let report = host.doctor();
    assert_eq!(report["languages"], json!(["typescript"]));
    assert_eq!(entry(&report, "scip-typescript")["relevant"], true);
    for other in ["scip-python", "scip-java", "rust-analyzer"] {
        assert_eq!(entry(&report, other)["relevant"], false, "{other}");
    }
    let stdout = text(&host.cli(&["producer", "doctor"]).stdout);
    assert!(
        stdout.contains("Not needed for this checkout: scip-java, scip-python, rust-analyzer"),
        "{stdout}"
    );
    assert!(!stdout.contains("scip-python (python"), "{stdout}");
}

#[test]
fn a_pinned_package_is_ready_and_needs_no_command() {
    let host = Host::new(&["a.ts"]);
    host.npm_package("scip-typescript", "0.4.0");
    host.node();
    let report = host.doctor();
    let entry = entry(&report, "scip-typescript");
    assert_eq!(entry["state"], "ready", "{entry}");
    assert_eq!(entry["installCommand"], Value::Null);
    assert_eq!(entry["reason"], Value::Null);
}

#[test]
fn another_version_is_wrong_version_with_both_versions() {
    let host = Host::new(&["a.py"]);
    host.npm_package("scip-python", "0.6.5");
    host.node();
    let report = host.doctor();
    let entry = entry(&report, "scip-python");
    assert_eq!(entry["state"], "wrongVersion", "{entry}");
    assert_eq!(entry["found"], "0.6.5");
    assert_eq!(entry["pinned"], "0.6.6");
    let stdout = text(&host.cli(&["producer", "doctor"]).stdout);
    assert!(
        stdout.contains("wrongVersion (found 0.6.5, pinned 0.6.6)"),
        "{stdout}"
    );
}

#[test]
fn a_shim_or_hash_mismatch_is_unverifiable() {
    let host = Host::new(&["a.ts", "A.java"]);
    // A version-manager shim: a script, not an npm package entry point.
    write_exe(
        &host.bin().join("scip-typescript"),
        b"#!/bin/sh\nexec mise x -- scip-typescript \"$@\"\n",
    );
    host.node();
    // A scip-java launcher whose bytes are not the pinned hash.
    write_exe(&host.bin().join("scip-java"), b"#!/bin/sh\n");
    let report = host.doctor();
    let ts = entry(&report, "scip-typescript");
    assert_eq!(ts["state"], "unverifiable", "{ts}");
    assert!(ts["reason"].as_str().unwrap().contains("shim"), "{ts}");
    assert_eq!(ts["installCommand"], TS_INSTALL);
    let java = entry(&report, "scip-java");
    assert_eq!(java["state"], "unverifiable", "{java}");
    assert!(
        java["reason"]
            .as_str()
            .unwrap()
            .starts_with("executable sha256"),
        "{java}"
    );
}

#[test]
fn a_missing_runtime_is_prerequisite_missing() {
    let host = Host::new(&["a.ts", "lib.rs"]);
    host.npm_package("scip-typescript", "0.4.0");
    // A rust-analyzer binary for the pinned toolchain, but no rustup to run it.
    let target = format!(
        "{}-{}",
        "1.98.1",
        match std::env::consts::OS {
            "macos" => format!("{}-apple-darwin", std::env::consts::ARCH),
            _ => format!("{}-unknown-linux-gnu", std::env::consts::ARCH),
        }
    );
    write_exe(
        &host
            .home()
            .join(".rustup/toolchains")
            .join(target)
            .join("bin/rust-analyzer"),
        b"#!/bin/sh\n",
    );
    let report = host.doctor();
    let ts = entry(&report, "scip-typescript");
    assert_eq!(ts["state"], "prerequisiteMissing", "{ts}");
    assert_eq!(ts["reason"], "no node on the declared PATH");
    let node = &ts["prerequisites"][0];
    assert_eq!(node["name"], "node");
    assert_eq!(node["present"], false);
    let rust = entry(&report, "rust-analyzer");
    assert_eq!(rust["state"], "prerequisiteMissing", "{rust}");
    assert_eq!(rust["reason"], "no rustup on the declared PATH");
}

#[test]
fn install_without_a_terminal_or_yes_refuses_and_runs_nothing() {
    let host = Host::new(&["a.ts"]);
    host.node();
    host.recording_tool("npm", "");
    let output = host.cli(&["producer", "install", "scip-typescript"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(text(&output.stdout).contains(TS_INSTALL), "{output:?}");
    assert!(
        text(&output.stderr).contains("confirmation_required"),
        "{output:?}"
    );
    assert_eq!(host.recorded(), None, "nothing ran");
}

#[test]
fn install_yes_runs_only_the_pinned_command_then_rechecks() {
    let host = Host::new(&["a.ts"]);
    host.node();
    // The fake npm installs the pinned package where resolution looks for it.
    let package = host
        .root()
        .join("prefix/lib/node_modules/@sourcegraph/scip-typescript");
    let then = format!(
        "/bin/mkdir -p '{package}/dist/src' && printf '//' > '{package}/dist/src/main.js' && \
         /bin/chmod 755 '{package}/dist/src/main.js' && \
         printf '%s' '{manifest}' > '{package}/package.json' && \
         /bin/ln -s '{package}/dist/src/main.js' '{bin}/scip-typescript'",
        package = package.display(),
        manifest = json!({"name": "@sourcegraph/scip-typescript", "version": "0.4.0"}),
        bin = host.bin().display(),
    );
    host.recording_tool("npm", &then);
    let output = host.cli(&["producer", "install", "scip-typescript", "--yes", "--json"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        host.recorded().unwrap(),
        "install\n-g\n@sourcegraph/scip-typescript@0.4.0\n-- RUSTUP_AUTO_INSTALL=\n",
        "exactly the pinned argv, once"
    );
    assert!(text(&output.stderr).contains(TS_INSTALL), "{output:?}");
    let after: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(after["name"], "scip-typescript");
    assert_eq!(after["state"], "ready", "{after}");

    // Ready now: a second install runs nothing.
    fs::remove_file(host.record()).unwrap();
    let again = host.cli(&["producer", "install", "scip-typescript", "--yes"]);
    assert!(again.status.success(), "{again:?}");
    assert!(text(&again.stdout).contains("already ready"), "{again:?}");
    assert_eq!(host.recorded(), None);
}

#[test]
fn install_rust_analyzer_adds_the_component_without_auto_installing() {
    let host = Host::new(&["lib.rs"]);
    let target = match std::env::consts::OS {
        "macos" => format!("{}-apple-darwin", std::env::consts::ARCH),
        _ => format!("{}-unknown-linux-gnu", std::env::consts::ARCH),
    };
    fs::create_dir_all(
        host.home()
            .join(".rustup/toolchains")
            .join(format!("1.98.1-{target}")),
    )
    .unwrap();
    host.recording_tool("rustup", "");
    let output = host.cli(&["producer", "install", "rust-analyzer", "--yes", "--json"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        host.recorded().unwrap(),
        "component\nadd\nrust-analyzer\n--toolchain\n1.98.1\n-- RUSTUP_AUTO_INSTALL=0\n"
    );
    let after: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(after["state"], "missing", "the fake added nothing: {after}");
}

#[test]
fn install_with_a_missing_prerequisite_refuses_typed_and_runs_nothing() {
    // npm is present, node is not.
    let host = Host::new(&["a.ts", "lib.rs"]);
    host.recording_tool("npm", "");
    let output = host.cli(&["producer", "install", "scip-typescript", "--yes"]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = text(&output.stderr);
    assert!(stderr.contains("prerequisite_missing: node"), "{stderr}");
    assert!(
        stderr.contains("Trellis does not install prerequisites"),
        "{stderr}"
    );
    assert_eq!(host.recorded(), None, "nothing ran");

    // rustup is present, the pinned toolchain is not: adding the component would install it.
    host.recording_tool("rustup", "");
    let output = host.cli(&["producer", "install", "rust-analyzer", "--yes"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        text(&output.stderr).contains("prerequisite_missing: rust toolchain"),
        "{output:?}"
    );
    assert_eq!(host.recorded(), None, "nothing ran");

    // No coursier at all.
    let output = host.cli(&["producer", "install", "scip-java", "--yes"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        text(&output.stderr).contains("prerequisite_missing: coursier"),
        "{output:?}"
    );
}
