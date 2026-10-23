#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_trellis"))
        .args(args)
        .output()
        .unwrap()
}
fn success(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
fn node(mode: &str, root: &Path, descriptor: Option<&Path>) {
    let mut command = Command::new("node");
    command
        .arg("tools/semantic-contract/test/production-capture-check.mjs")
        .arg(mode)
        .arg(root);
    if let Some(path) = descriptor {
        command.arg(path);
    }
    success(command.output().unwrap());
}

#[test]
fn real_native_capture_normalizes_and_preserves_a_fresh_output() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("javascript");
    node("prepare", &root, None);
    let descriptor = temp.path().join("producer.json");
    fs::write(&descriptor, success(cli(&["native-capture", "--describe"]))).unwrap();
    let output = root.join("captures/native.json");
    let args = [
        "native-capture",
        "--fixture",
        root.to_str().unwrap(),
        "--output",
        output.to_str().unwrap(),
    ];
    success(cli(&args));
    let first = fs::read(&output).unwrap();
    let capture: Value = serde_json::from_slice(&first).unwrap();
    assert!(!capture["calls"].as_array().unwrap().is_empty());
    assert_eq!(capture["references"], json!([]));
    assert!(!cli(&args).status.success());
    assert_eq!(first, fs::read(&output).unwrap());
    let second = temp.path().join("second.json");
    success(
        Command::new(env!("CARGO_BIN_EXE_trellis"))
            .current_dir(&root)
            .args([
                "native-capture",
                "--fixture",
                "fixture.json",
                "--output",
                second.to_str().unwrap(),
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(first, fs::read(second).unwrap());
    node("check", &root, Some(&descriptor));
}

fn manifest(root: &Path, documents: Value) -> Value {
    let manifest = json!({"sourceSets":[{"id":"main", "rootId":"root", "languages":["java","rust","python","javascript"], "dependencies":[]}],
        "revisions":[{"id":"r0", "sourceSetId":"main", "documents":documents,
        "toolchainHash":"1".repeat(64), "configHash":"2".repeat(64), "dependencyHash":"3".repeat(64)}]});
    fs::write(
        root.join("fixture.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    manifest
}

#[test]
fn all_production_languages_emit_source_witnesses_and_test_roles() {
    let temp = tempfile::tempdir().unwrap();
    let samples = [
        (
            "java",
            "tests/SampleTest.java",
            "class SampleTest { public void run(int n) { if (n > 0) run(n - 1); } }",
        ),
        (
            "rust",
            "tests/sample.rs",
            "struct Worker; impl Worker { pub fn run(&self, n: usize) { if n > 0 { self.run(n - 1); } } }",
        ),
        (
            "python",
            "tests/test_sample.py",
            "def run(n):\n    if n > 0:\n        run(n - 1)\n",
        ),
        (
            "javascript",
            "tests/sample.test.js",
            "function run(n) { if (n > 0) run(n - 1); }",
        ),
    ];
    let descriptor = temp.path().join("producer.json");
    fs::write(&descriptor, success(cli(&["native-capture", "--describe"]))).unwrap();
    for (language, path, source) in samples {
        let root = temp.path().join(language);
        fs::create_dir(&root).unwrap();
        fs::write(root.join("source"), source).unwrap();
        manifest(
            &root,
            json!([{"key":{"sourceSetId":"main", "language":language, "path":path}, "revisionId":"r0", "sourceFile":"source"}]),
        );
        let output = root.join("native.json");
        success(cli(&[
            "native-capture",
            "--fixture",
            root.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ]));
        let capture: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
        let declarations = capture["declarations"].as_array().unwrap();
        assert!(declarations.len() > 1, "{language}");
        assert!(
            declarations.iter().all(|row| row["role"] == "test"),
            "{language}"
        );
        assert!(
            declarations.iter().any(|row| row["witnesses"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())),
            "{language}"
        );
        assert!(
            !capture["calls"].as_array().unwrap().is_empty(),
            "{language}"
        );
        assert!(
            !capture["controls"].as_array().unwrap().is_empty(),
            "{language}"
        );
        node("check-all", &root, Some(&descriptor));
    }
}

#[test]
fn invalid_snapshot_fails_before_creating_output() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("invalid"), [0xff]).unwrap();
    manifest(
        temp.path(),
        json!([{"key":{"sourceSetId":"main", "language":"javascript", "path":"a.js"}, "revisionId":"r0", "sourceFile":"invalid"}]),
    );
    let output = temp.path().join("native.json");
    let result = cli(&[
        "native-capture",
        "--fixture",
        temp.path().to_str().unwrap(),
        "--output",
        output.to_str().unwrap(),
    ]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("not UTF-8"));
    assert!(!output.exists());
}

#[cfg(unix)]
#[test]
fn nonregular_snapshot_fails_before_creating_output() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let temp = tempfile::tempdir().unwrap();
    let fifo = temp.path().join("source");
    let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    manifest(
        temp.path(),
        json!([{"key":{"sourceSetId":"main", "language":"javascript", "path":"a.js"}, "revisionId":"r0", "sourceFile":"source"}]),
    );
    let output = temp.path().join("native.json");
    let result = cli(&[
        "native-capture",
        "--fixture",
        temp.path().to_str().unwrap(),
        "--output",
        output.to_str().unwrap(),
    ]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("not a regular file"));
    assert!(!output.exists());
}
