use super::profiles::{self, Target};
use super::worktree::{self, RetainedInputs, RetainedSource};
use super::{ProducerName, TrustProfile};
use anyhow::Result;
use serde_json::{Value, json};
use std::{fs, sync::atomic::AtomicBool};

fn present(path: &str, bytes: &[u8]) -> Value {
    json!({"path": path, "type": "present", "bytes": hex::encode(bytes), "byteLength": bytes.len()})
}

/// An in-memory retained revision.
struct Retained {
    documents: Vec<(&'static str, &'static [u8])>,
    config: Vec<Value>,
    dependencies: Vec<Value>,
}

impl RetainedSource for Retained {
    fn each_document(&self, visit: &mut dyn FnMut(&str, Vec<u8>) -> Result<()>) -> Result<()> {
        for (path, bytes) in &self.documents {
            visit(path, bytes.to_vec())?;
        }
        Ok(())
    }

    fn components(&self) -> Result<(Vec<Value>, Vec<Value>)> {
        Ok((self.config.clone(), self.dependencies.clone()))
    }
}

fn retained() -> Retained {
    let dependencies = vec![
        present("package.json", b"{\"name\":\"x\"}"),
        present("Cargo.toml", b"[package]\n"),
        json!({"path": "Cargo.lock", "type": "absent"}),
        present("pom.xml", b"<project/>"),
        present("build.gradle", b"// g"),
        present("pyproject.toml", b"[project]\n"),
    ];
    let mut config = dependencies.clone();
    config.push(json!({"nativeAdmission": {"maxFileBytes": 2_097_152}}));
    config.push(present(".gitignore", b"target\n"));
    Retained {
        documents: vec![("src/a.js", b"a()\n"), ("pkg/b.py", b"b = 1\n")],
        config,
        dependencies,
    }
}

fn run(source: &Retained, producer: ProducerName, trust: TrustProfile) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let rules = profiles::rules(&profiles::row(Target::Real(producer), trust).unwrap());
    worktree::materialize(source, &rules, dir.path(), &AtomicBool::new(false)).unwrap();
    dir
}

fn exists(dir: &tempfile::TempDir, path: &str) -> bool {
    dir.path().join("worktree").join(path).exists()
}

#[test]
fn worktree_bytes_equal_retained_revision() {
    let source = retained();
    let dir = run(
        &source,
        ProducerName::ScipTypescript,
        TrustProfile::Restricted,
    );
    let tree = dir.path().join("worktree");
    for (path, bytes) in &source.documents {
        assert_eq!(fs::read(tree.join(path)).unwrap(), *bytes);
    }
    // Present in both config and dependencies, written once with retained bytes.
    assert_eq!(
        fs::read(tree.join("package.json")).unwrap(),
        b"{\"name\":\"x\"}"
    );
    assert_eq!(fs::read(tree.join("Cargo.toml")).unwrap(), b"[package]\n");
    assert_eq!(fs::read(tree.join(".gitignore")).unwrap(), b"target\n");
    assert!(!exists(&dir, "Cargo.lock"));
    let count = worktree::materialize(
        &source,
        &profiles::Rules::default(),
        tempfile::tempdir().unwrap().path(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(count, 2 + 6 - 1 + 1);
}

#[test]
fn restricted_java_omits_build_files_and_writes_config() {
    let dir = run(
        &retained(),
        ProducerName::ScipJava,
        TrustProfile::Restricted,
    );
    assert!(!exists(&dir, "pom.xml"));
    assert!(!exists(&dir, "build.gradle"));
    assert!(exists(&dir, "package.json"));
    assert_eq!(
        fs::read(dir.path().join("worktree/lsif-java.json")).unwrap(),
        br#"{"dependencies":[]}"#
    );
    let inputs = RetainedInputs::read(&retained()).unwrap();
    let rules = profiles::rules(
        &profiles::row(
            Target::Real(ProducerName::ScipJava),
            TrustProfile::Restricted,
        )
        .unwrap(),
    );
    let layout = worktree::layout(&inputs, &rules);
    let omitted: Vec<_> = layout.omitted.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(omitted, ["build.gradle", "pom.xml"]);
    assert!(layout.config.iter().all(|f| f.path != "pom.xml"));
    assert_eq!(layout.generated[0].path, "worktree/lsif-java.json");
}

#[test]
fn trusted_java_includes_build_files() {
    let dir = run(&retained(), ProducerName::ScipJava, TrustProfile::Trusted);
    assert!(exists(&dir, "pom.xml"));
    assert!(exists(&dir, "build.gradle"));
    assert!(!exists(&dir, "lsif-java.json"));
}

#[test]
fn restricted_python_omits_pyproject() {
    let dir = run(
        &retained(),
        ProducerName::ScipPython,
        TrustProfile::Restricted,
    );
    assert!(!exists(&dir, "pyproject.toml"));
    assert!(exists(&dir, "pkg/b.py"));
    assert_eq!(
        fs::read(dir.path().join("producer-config/environment.json")).unwrap(),
        b"[]"
    );
}

#[test]
fn trusted_python_includes_pyproject() {
    let dir = run(&retained(), ProducerName::ScipPython, TrustProfile::Trusted);
    assert!(exists(&dir, "pyproject.toml"));
    assert!(!dir.path().join("producer-config").exists());
}

#[test]
fn cancellation_stops_between_files() {
    let dir = tempfile::tempdir().unwrap();
    let error = worktree::materialize(
        &retained(),
        &profiles::Rules::default(),
        dir.path(),
        &AtomicBool::new(true),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
}
