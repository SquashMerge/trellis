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

fn assert_witness(row: &Value, field: &str, source: &str, start: usize, end: usize) {
    let witness = &row["witnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["field"] == field)
        .unwrap()["witness"];
    assert_eq!(
        witness["range"],
        json!({"start":start,"end":end,"encoding":"utf8"}),
        "{field}"
    );
    assert_eq!(witness["text"], &source[start..end], "{field}");
}

#[test]
fn header_witnesses_use_measured_nodes_not_first_matching_text() {
    let temp = tempfile::tempdir().unwrap();
    let samples = [
        (
            "java",
            "A.java",
            "class A { public void run(int n) { run(n); } }",
            "n",
            "(int n)",
            5,
        ),
        (
            "rust",
            "a.rs",
            "fn run(n: usize) { run(n); }",
            "n",
            "(n:",
            1,
        ),
        ("python", "a.py", "def run(n):\n    run(n)\n", "n", "(n)", 1),
        (
            "javascript",
            "a.js",
            "function run(n) { run(n); }",
            "n",
            "(n)",
            1,
        ),
    ];
    for (language, path, source, parameter, marker, delta) in samples {
        fs::write(temp.path().join("source"), source).unwrap();
        manifest(
            temp.path(),
            json!([{"key":{"sourceSetId":"main","language":language,"path":path},"revisionId":"r0","sourceFile":"source"}]),
        );
        let output = temp.path().join(format!("{language}.json"));
        success(cli(&[
            "native-capture",
            "--fixture",
            temp.path().to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ]));
        let capture: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
        let row = capture["declarations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "run")
            .unwrap();
        let start = source.find(marker).unwrap() + delta;
        assert_witness(
            row,
            "header.parameters[0].name",
            source,
            start,
            start + parameter.len(),
        );
    }
}

#[test]
fn repeated_header_types_names_and_utf8_have_distinct_exact_ranges() {
    let temp = tempfile::tempdir().unwrap();
    let source = "/* é */ class A<T> extends Base { public T run(T n, T run) { return n; } }";
    fs::write(temp.path().join("source"), source).unwrap();
    manifest(
        temp.path(),
        json!([{"key":{"sourceSetId":"main","language":"java","path":"A.java"},"revisionId":"r0","sourceFile":"source"}]),
    );
    let output = temp.path().join("native.json");
    success(cli(&[
        "native-capture",
        "--fixture",
        temp.path().to_str().unwrap(),
        "--output",
        output.to_str().unwrap(),
    ]));
    let capture: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    let rows = capture["declarations"].as_array().unwrap();
    let method = rows.iter().find(|row| row["name"] == "run").unwrap();
    let result = source.find("T run").unwrap();
    let first_type = source.find("(T n").unwrap() + 1;
    let second_type = source.find(", T run").unwrap() + 2;
    for (field, start, length) in [
        ("header.resultType", result, 1),
        ("header.parameters[0].type", first_type, 1),
        ("signature.parameterTypes[0]", first_type, 1),
        ("header.parameters[0].name", first_type + 2, 1),
        ("header.parameters[1].type", second_type, 1),
        ("signature.parameterTypes[1]", second_type, 1),
        ("header.parameters[1].name", second_type + 2, 3),
        ("header.modifiers[0]", source.find("public").unwrap(), 6),
    ] {
        assert_witness(method, field, source, start, start + length);
    }
    let class = rows.iter().find(|row| row["name"] == "A").unwrap();
    let type_parameter = source.find("<T>").unwrap() + 1;
    assert_witness(
        class,
        "header.typeParameters[0]",
        source,
        type_parameter,
        type_parameter + 1,
    );
    let base = source.find("extends Base").unwrap();
    assert_witness(
        class,
        "header.bases[0]",
        source,
        base,
        base + "extends Base".len(),
    );
}

#[test]
fn opaque_rust_capture_surfaces_partial_coverage_and_keeps_measured_output() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("source"),
        "fn main() { some_macro!(hidden()); }",
    )
    .unwrap();
    manifest(
        temp.path(),
        json!([{"key":{"sourceSetId":"main","language":"rust","path":"a.rs"},"revisionId":"r0","sourceFile":"source"}]),
    );
    let output = temp.path().join("native.json");
    let result = cli(&[
        "native-capture",
        "--fixture",
        temp.path().to_str().unwrap(),
        "--output",
        output.to_str().unwrap(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let diagnostic = String::from_utf8_lossy(&result.stderr);
    assert!(
        diagnostic.contains("partial main/rust/a.rs@r0"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("opaque Rust macro/attribute token trees skipped; calls and definitions inside were not measured"), "{diagnostic}");
    let capture: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    assert!(
        capture["declarations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "main")
    );
    assert_eq!(capture["calls"], json!([]));
    assert!(capture.get("coverage").is_none());
}
