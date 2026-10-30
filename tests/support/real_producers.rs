//! Helpers for the real-producer end-to-end binaries (`producer_e2e*`). Include next to
//! `support/producer.rs`, named `support`: `#[path = "support/real_producers.rs"] mod real;`.
//! Later slices only add items; binaries owned by earlier slices include this file.
//!
//! Nothing here installs anything. The declared environment is computed from the real `HOME`
//! and the locations `docs/semantic-evidence/producer-provisioning.md` records, never from
//! shell activation: mise and similar shims are refused by detection (D3).
#![allow(dead_code)]
use super::support::{Checkout, start_checkout, workspace_tree_digest};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Output,
};

/// What a test does when the producer is not provisioned on this host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Availability {
    /// The default variant: print `SKIP producer_unavailable: <reason>` and pass.
    SkipIfUnavailable,
    /// The strict variant: fail with the typed reason.
    Required,
}

/// One real-producer end-to-end case.
pub struct Case {
    pub producer: &'static str,
    /// A directory under `tests/fixtures/producers/`.
    pub fixture: &'static str,
    /// The artifact language and the extension of its fixture documents.
    pub extension: &'static str,
    pub trust: &'static str,
}

/// The directory of `fixture` under `tests/fixtures/producers/`.
pub fn fixture_dir(fixture: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/producers")
        .join(fixture)
}

/// Copy `from` into the existing directory `to`.
pub fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Paths of the fixture's documents with `extension`, relative to the fixture, sorted.
pub fn fixture_documents(fixture: &str, extension: &str) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, extension: &str, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, extension, out);
            } else if path.extension().is_some_and(|e| e == extension) {
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let root = fixture_dir(fixture);
    let mut out = Vec::new();
    walk(&root, &root, extension, &mut out);
    out.sort();
    out
}

fn real_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is set"))
}

/// Directories that may hold a producer launcher, its `node` or its JVM: the caller's `PATH`
/// without shim directories (where the `rustup` proxy is found), then the mise node and java
/// installs recorded in the provisioning doc, newest first. Nothing here is executed.
fn declared_path_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    dirs.retain(|dir| {
        dir.is_absolute() && !dir.components().any(|part| part.as_os_str() == "shims")
    });
    for tool in ["node", "java"] {
        let installs = home.join(".local/share/mise/installs").join(tool);
        if let Ok(entries) = fs::read_dir(installs) {
            let mut versions: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path().join("bin"))
                .collect();
            versions.sort();
            versions.reverse();
            dirs.extend(versions);
        }
    }
    for dir in ["/usr/bin", "/bin"] {
        dirs.push(PathBuf::from(dir));
    }
    dirs
}

/// The declared producer environment (brief D12), computed from the real `HOME` before a test
/// overrides it: `PATH`, `RUSTUP_HOME`, `CARGO_HOME`, `COURSIER_CACHE` and
/// `COURSIER_ARCHIVE_CACHE`.
pub fn real_producer_env() -> Vec<(String, String)> {
    let home = real_home();
    let coursier = if cfg!(target_os = "macos") {
        home.join("Library/Caches/Coursier")
    } else {
        home.join(".cache/coursier")
    };
    let path = declared_path_dirs(&home)
        .iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(":");
    let text = |path: PathBuf| path.display().to_string();
    vec![
        ("PATH".into(), path),
        ("RUSTUP_HOME".into(), text(home.join(".rustup"))),
        ("CARGO_HOME".into(), text(home.join(".cargo"))),
        ("COURSIER_CACHE".into(), text(coursier.join("v1"))),
        ("COURSIER_ARCHIVE_CACHE".into(), text(coursier.join("arc"))),
    ]
}

/// `trellis producer <verb> --workspace <root> --producer <name> ...` with the declared
/// environment.
pub fn producer_command(
    checkout: &Checkout,
    verb: &str,
    producer: &str,
    extra: &[&str],
) -> std::process::Command {
    let mut command = checkout.cli();
    for (name, value) in real_producer_env() {
        command.env(name, value);
    }
    command
        .args(["producer", verb, "--workspace"])
        .arg(&checkout.root)
        .args(["--producer", producer])
        .args(extra);
    command
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The plan, or `Err(reason)` when planning refused with `producer_unavailable: <reason>`.
/// Any other failure panics.
pub fn plan(
    checkout: &Checkout,
    producer: &str,
    trust: &str,
    extra: &[&str],
) -> Result<Value, String> {
    let mut args = vec!["--trust", trust];
    args.extend_from_slice(extra);
    let output = producer_command(checkout, "plan", producer, &args)
        .output()
        .unwrap();
    if output.status.success() {
        return Ok(serde_json::from_slice(&output.stdout).unwrap());
    }
    let error = stderr_of(&output);
    match error.split_once("producer_unavailable: ") {
        Some((_, reason)) => Err(reason.trim().to_owned()),
        None => panic!("plan failed without a typed unavailability: {error}"),
    }
}

impl Availability {
    /// The plan, or `None` after printing the typed skip. The strict variant panics instead.
    pub fn resolve(self, producer: &str, planned: Result<Value, String>) -> Option<Value> {
        match planned {
            Ok(plan) => Some(plan),
            Err(reason) => {
                assert!(
                    self == Self::SkipIfUnavailable,
                    "{producer}: producer_unavailable: {reason}"
                );
                println!("SKIP producer_unavailable: {producer}: {reason}");
                None
            }
        }
    }
}

/// Index a copy of the fixture, plan, run the producer and assert the import. Returns the
/// printed evidence line, or `None` when the producer is unavailable and the variant skips.
pub fn run_case(case: &Case, availability: Availability) -> Option<String> {
    let checkout = start_checkout(|root| copy_tree(&fixture_dir(case.fixture), root), &[]);
    let plan = availability.resolve(
        case.producer,
        plan(&checkout, case.producer, case.trust, &[]),
    )?;
    assert_eq!(plan["producer"]["verification"]["result"], "verified");
    assert_eq!(plan["trustProfile"], case.trust);
    let documents = fixture_documents(case.fixture, case.extension);
    assert!(documents.len() >= 2);
    assert_eq!(plan["inputs"]["documents"]["requested"], documents.len());
    let hash = plan["planSha256"].as_str().unwrap().to_owned();

    let before = workspace_tree_digest(&checkout.root);
    let output = producer_command(
        &checkout,
        "run",
        case.producer,
        &["--trust", case.trust, "--plan-sha256", &hash],
    )
    .output()
    .unwrap();
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!("no JSON on stdout; stderr: {}", stderr_of(&output));
    });
    assert!(
        output.status.success() && reply["state"] == "done",
        "{} run failed: {reply} {}",
        case.producer,
        stderr_of(&output)
    );
    assert_eq!(reply["outcome"], "imported", "{reply}");
    assert_eq!(reply["producer"], case.producer);
    let published = &reply["import"]["publishedRevision"];
    assert!(published.is_object(), "{reply}");
    let report = &reply["import"]["report"];
    let (covered, facts) = report_totals(report, &documents);
    assert!(facts > 0, "no facts: {report}");
    assert_eq!(
        workspace_tree_digest(&checkout.root),
        before,
        "the run changed the workspace"
    );
    let line = format!(
        "E2E-EVIDENCE producer={} version={} verification=verified trustProfile={} state=done publishedRevision={} documents={covered} facts={facts} treeDigestUnchanged=true",
        case.producer,
        plan["producer"]["version"].as_str().unwrap(),
        case.trust,
        published,
    );
    println!("{line}");
    Some(line)
}

/// Documents the import covered and its total claim count, from the report's per-artifact
/// totals. Every fixture document must be covered by an imported artifact.
fn report_totals(report: &Value, documents: &[String]) -> (u64, u64) {
    let artifacts = report["artifacts"].as_array().unwrap_or_else(|| {
        panic!("report has no artifacts: {report}");
    });
    let sum = |key: &str| -> u64 { artifacts.iter().filter_map(|a| a[key].as_u64()).sum() };
    let covered = sum("documents");
    assert!(
        covered >= documents.len() as u64,
        "{covered} documents covered, expected {}: {report}",
        documents.len()
    );
    (covered, sum("claims"))
}
