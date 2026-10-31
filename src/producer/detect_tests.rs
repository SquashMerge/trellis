use super::detect::{
    Declared, Environment, Unavailable, resolve, resolve_base, resolve_rust_on, rustup_toolchain,
};
use super::profiles::{Stub, Target};
use super::{ProducerName, TrustProfile};
use crate::scip_admission::PinnedProducer;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

fn write_exe(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

const MACHO: [u8; 8] = [0xcf, 0xfa, 0xed, 0xfe, 0, 0, 0, 0];

fn declared(path: &str, home: &Path) -> Declared {
    Declared::new(&Environment {
        path: path.to_owned(),
        home: home.display().to_string(),
        ..Environment::default()
    })
    .unwrap()
}

fn pin(producer: ProducerName) -> PinnedProducer {
    Target::Real(producer).pin().unwrap()
}

/// `<dir>/bin/<package>` linked into `<dir>/lib/node_modules/@sourcegraph/<package>`.
fn npm(dir: &Path, package: &str, version: &str, manifest: bool) {
    let root = dir.join("lib/node_modules/@sourcegraph").join(package);
    write_exe(&root.join("dist/main.js"), b"//");
    if manifest {
        fs::write(
            root.join("package.json"),
            json!({"name": format!("@sourcegraph/{package}"), "version": version}).to_string(),
        )
        .unwrap();
    }
    fs::create_dir_all(dir.join("bin")).unwrap();
    std::os::unix::fs::symlink(root.join("dist/main.js"), dir.join("bin").join(package)).unwrap();
}

fn base(
    producer: ProducerName,
    declared: &Declared,
    executable: Option<&Path>,
) -> Result<super::detect::Resolution, Unavailable> {
    resolve_base(
        Target::Real(producer),
        &pin(producer),
        declared,
        executable,
        None,
    )
}

fn reason(result: Result<super::detect::Resolution, Unavailable>) -> String {
    result.unwrap_err().0
}

#[test]
fn npm_producer_verifies_package_version_and_prefers_adjacent_node() {
    let tmp = tempfile::tempdir().unwrap();
    let prefix = tmp.path().join("prefix");
    let pin = pin(ProducerName::ScipTypescript);
    npm(&prefix, "scip-typescript", &pin.version, true);
    write_exe(&prefix.join("bin/node"), &MACHO);
    let other = tmp.path().join("other");
    write_exe(&other.join("node"), &MACHO);
    let path = format!("{}:{}", other.display(), prefix.join("bin").display());
    let declared = declared(&path, tmp.path());
    let found = base(ProducerName::ScipTypescript, &declared, None).unwrap();
    assert_eq!(found.verification.method, "packageManifestVersion");
    assert_eq!(found.verification.observed, pin.version);
    assert!(found.launcher.ends_with("dist/main.js"));
    let node = found.node.unwrap();
    assert_eq!(node.dir, prefix.join("bin"));
    assert_eq!(node.sha256.len(), 64);
}

#[test]
fn npm_producer_falls_back_to_first_node_and_honors_executable() {
    let tmp = tempfile::tempdir().unwrap();
    let prefix = tmp.path().join("prefix");
    let pin = pin(ProducerName::ScipPython);
    npm(&prefix, "scip-python", &pin.version, true);
    let nodes = tmp.path().join("nodes");
    write_exe(&nodes.join("node"), &MACHO);
    let declared = declared(&nodes.display().to_string(), tmp.path());
    let main = prefix.join("lib/node_modules/@sourcegraph/scip-python/dist/main.js");
    let found = base(ProducerName::ScipPython, &declared, Some(&main)).unwrap();
    assert_eq!(found.node.unwrap().dir, nodes);
    assert!(reason(base(ProducerName::ScipPython, &declared, None)).contains("no scip-python"));
}

#[test]
fn npm_producer_refuses_wrong_version_shim_and_non_native_node() {
    let tmp = tempfile::tempdir().unwrap();
    let pin = pin(ProducerName::ScipTypescript);

    let old = tmp.path().join("old");
    npm(&old, "scip-typescript", "0.0.1", true);
    write_exe(&old.join("bin/node"), &MACHO);
    let found = base(
        ProducerName::ScipTypescript,
        &declared(&old.join("bin").display().to_string(), tmp.path()),
        None,
    );
    assert_eq!(
        reason(found),
        format!("version 0.0.1 != pinned {}", pin.version)
    );

    let shim = tmp.path().join("shim");
    npm(&shim, "scip-typescript", &pin.version, false);
    write_exe(&shim.join("bin/node"), &MACHO);
    let found = base(
        ProducerName::ScipTypescript,
        &declared(&shim.join("bin").display().to_string(), tmp.path()),
        None,
    );
    assert_eq!(
        reason(found),
        "launcher is not an npm package entry point (shim?)"
    );

    let script = tmp.path().join("script");
    npm(&script, "scip-typescript", &pin.version, true);
    write_exe(&script.join("bin/node"), b"#!/bin/sh\n");
    let found = base(
        ProducerName::ScipTypescript,
        &declared(&script.join("bin").display().to_string(), tmp.path()),
        None,
    );
    assert_eq!(reason(found), "node is not a native executable (shim?)");
}

#[test]
fn trusted_python_needs_a_declared_python3() {
    let tmp = tempfile::tempdir().unwrap();
    let prefix = tmp.path().join("prefix");
    let pin = pin(ProducerName::ScipPython);
    npm(&prefix, "scip-python", &pin.version, true);
    write_exe(&prefix.join("bin/node"), &MACHO);
    let bin = prefix.join("bin");
    let run = |trust, declared: &Declared| {
        resolve(
            Target::Real(ProducerName::ScipPython),
            trust,
            &pin,
            declared,
            None,
            None,
        )
    };
    let without = declared(&bin.display().to_string(), tmp.path());
    assert!(
        run(TrustProfile::Restricted, &without)
            .unwrap()
            .python3
            .is_none()
    );
    assert_eq!(
        run(TrustProfile::Trusted, &without).unwrap_err().0,
        "no python3 on the declared PATH"
    );
    write_exe(&bin.join("python3"), &MACHO);
    let found = run(TrustProfile::Trusted, &without).unwrap();
    assert_eq!(found.python3.unwrap().dir, bin);
}

#[test]
fn scip_java_verifies_the_executable_hash() {
    let tmp = tempfile::tempdir().unwrap();
    let launcher = tmp.path().join("bin/scip-java");
    write_exe(&launcher, b"launcher bytes");
    let jdk = tmp.path().join("jdk");
    write_exe(&jdk.join("bin/java"), &MACHO);
    let path = format!(
        "{}:{}",
        launcher.parent().unwrap().display(),
        jdk.join("bin").display()
    );
    let declared = declared(&path, tmp.path());
    let mut pinned = pin(ProducerName::ScipJava);
    let target = Target::Real(ProducerName::ScipJava);

    let error = resolve_base(target, &pinned, &declared, None, None).unwrap_err();
    assert!(error.0.starts_with("executable sha256 "), "{error}");

    pinned.executable_hash = hex::encode(Sha256::digest(b"launcher bytes"));
    let found = resolve_base(target, &pinned, &declared, None, None).unwrap();
    assert_eq!(found.verification.method, "executableSha256");
    assert_eq!(found.java.as_ref().unwrap().dir, jdk.join("bin"));
    assert!(found.java_home.is_none());
    fs::write(jdk.join("release"), "JAVA_VERSION=17\n").unwrap();
    let found = resolve_base(target, &pinned, &declared, None, None).unwrap();
    assert_eq!(found.java_home, Some(fs::canonicalize(&jdk).unwrap()));
}

const LINUX: &str = "x86_64-unknown-linux-gnu";
const MAC: &str = "aarch64-apple-darwin";

/// A rust-analyzer pin whose table holds `pins`.
fn rust_pin(pins: &[(&str, &[u8])]) -> PinnedProducer {
    let mut pinned = pin(ProducerName::RustAnalyzer);
    pinned.target_pins = pins
        .iter()
        .map(|(target, bytes)| ((*target).to_owned(), hex::encode(Sha256::digest(bytes))))
        .collect();
    pinned
}

/// A declared environment with rustup, and the `rust-analyzer` of `target`'s 1.98.1 toolchain.
fn rust_toolchain(tmp: &Path, target: &str, bytes: &[u8]) -> Declared {
    let rustup_dir = tmp.join("cargo/bin");
    write_exe(&rustup_dir.join("rustup"), b"proxy");
    write_exe(
        &tmp.join(format!(
            "home/.rustup/toolchains/1.98.1-{target}/bin/rust-analyzer"
        )),
        bytes,
    );
    declared(&rustup_dir.display().to_string(), &tmp.join("home"))
}

#[test]
fn rust_analyzer_selects_the_pin_of_the_target() {
    let tmp = tempfile::tempdir().unwrap();
    let pinned = rust_pin(&[(MAC, b"mac analyzer"), (LINUX, b"linux analyzer")]);

    let declared = rust_toolchain(tmp.path(), LINUX, b"linux analyzer");
    let found = resolve_rust_on(LINUX, &pinned, &declared, None).unwrap();
    assert_eq!(
        found.verification.observed,
        hex::encode(Sha256::digest(b"linux analyzer"))
    );

    let declared = rust_toolchain(tmp.path(), MAC, b"mac analyzer");
    let found = resolve_rust_on(MAC, &pinned, &declared, None).unwrap();
    assert_eq!(
        found.verification.observed,
        hex::encode(Sha256::digest(b"mac analyzer"))
    );
}

#[test]
fn rust_analyzer_refuses_another_targets_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let pinned = rust_pin(&[(MAC, b"mac analyzer"), (LINUX, b"linux analyzer")]);
    let declared = rust_toolchain(tmp.path(), LINUX, b"mac analyzer");
    let error = resolve_rust_on(LINUX, &pinned, &declared, None).unwrap_err();
    assert!(error.0.starts_with("executable sha256 "), "{error}");
}

#[test]
fn rust_analyzer_refuses_a_target_with_no_pin_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let pinned = rust_pin(&[(MAC, b"mac analyzer")]);
    // The binary is present and would match another target's pin: only the table decides.
    let declared = rust_toolchain(tmp.path(), LINUX, b"mac analyzer");
    let error = resolve_rust_on(LINUX, &pinned, &declared, None).unwrap_err();
    assert_eq!(
        error.into_error().to_string(),
        format!("producer_unavailable: no rust-analyzer pin for target {LINUX}")
    );
}

#[test]
fn rust_analyzer_hashes_the_real_binary_not_the_proxy() {
    let tmp = tempfile::tempdir().unwrap();
    let triple = crate::scip_admission::host_target();
    let mut pinned = rust_pin(&[(&triple, b"pinned analyzer")]);
    assert_eq!(rustup_toolchain(&pinned), "1.98.1");
    let rustup_dir = tmp.path().join("cargo/bin");
    write_exe(&rustup_dir.join("rustup"), b"proxy");
    let real = tmp.path().join(format!(
        "home/.rustup/toolchains/1.98.1-{triple}/bin/rust-analyzer"
    ));
    let declared = declared(&rustup_dir.display().to_string(), &tmp.path().join("home"));
    let target = Target::Real(ProducerName::RustAnalyzer);

    let error = resolve_base(target, &pinned, &declared, None, None).unwrap_err();
    assert!(error.0.starts_with("no rust-analyzer for host"), "{error}");

    write_exe(&real, b"analyzer");
    let error = resolve_base(target, &pinned, &declared, None, None).unwrap_err();
    assert!(error.0.starts_with("executable sha256 "));

    pinned = rust_pin(&[(&triple, b"analyzer")]);
    let found = resolve_base(target, &pinned, &declared, None, None).unwrap();
    assert_eq!(found.launcher, rustup_dir.join("rustup"));
    assert_eq!(found.rustup.unwrap().dir, rustup_dir);
}

#[test]
fn stub_is_unavailable_unless_registered() {
    let tmp = tempfile::tempdir().unwrap();
    let declared = declared("/usr/bin", tmp.path());
    let pinned = Target::Stub.pin().unwrap();
    assert!(resolve_base(Target::Stub, &pinned, &declared, None, None).is_err());
    let exe = tmp.path().join("stub");
    write_exe(&exe, b"x");
    let stub = Stub {
        path: exe,
        args: vec![],
    };
    let found = resolve_base(Target::Stub, &pinned, &declared, None, Some(&stub)).unwrap();
    assert_eq!(found.verification.method, "testStub");
}

#[test]
fn environment_is_bounded_and_defaults_come_from_home() {
    let good = Environment {
        path: "/a:/b:relative::".into(),
        home: "/home/dev".into(),
        ..Environment::default()
    };
    let declared = Declared::new(&good).unwrap();
    assert_eq!(
        declared.path_dirs,
        [PathBuf::from("/a"), PathBuf::from("/b")]
    );
    assert_eq!(declared.rustup_home, Path::new("/home/dev/.rustup"));
    assert_eq!(declared.cargo_home, Path::new("/home/dev/.cargo"));
    let coursier = if cfg!(target_os = "macos") {
        "/home/dev/Library/Caches/Coursier/v1"
    } else {
        "/home/dev/.cache/coursier/v1"
    };
    assert_eq!(declared.coursier_cache, Path::new(coursier));

    let long = Environment {
        path: "/a:".repeat(129),
        ..good.clone()
    };
    assert!(long.validate().is_err());
    let huge = Environment {
        path: "/".repeat(8193),
        ..good.clone()
    };
    assert!(huge.validate().is_err());
    let relative_home = Environment {
        home: "home".into(),
        ..good.clone()
    };
    assert!(relative_home.validate().is_err());
    let relative_cache = Environment {
        cargo_home: Some("cargo".into()),
        ..good
    };
    assert!(relative_cache.validate().is_err());
}
