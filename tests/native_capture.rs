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
    assert!(!capture["references"].as_array().unwrap().is_empty());
    let manifest: Value =
        serde_json::from_slice(&fs::read(root.join("fixture.json")).unwrap()).unwrap();
    let expected_reports = manifest["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|revision| {
            revision["documents"]
                .as_array()
                .unwrap()
                .iter()
                .map(|document| json!({"document":document["key"],"revisionId":revision["id"]}))
        })
        .collect::<Vec<_>>();
    let actual_reports = capture["capabilityReports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|report| json!({"document":report["document"],"revisionId":report["revisionId"]}))
        .collect::<Vec<_>>();
    assert_eq!(actual_reports, expected_reports);
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
        let references = capture["references"].as_array().unwrap();
        assert!(!references.is_empty(), "{language}");
        for row in references {
            let start = row["range"]["start"].as_u64().unwrap() as usize;
            let end = row["range"]["end"].as_u64().unwrap() as usize;
            assert_eq!(row["range"]["encoding"], "utf8");
            assert_eq!(row["spelling"], &source[start..end]);
            assert_witness(row, "spelling", source, start, end);
            assert!(declarations.iter().any(|d| d["ref"] == row["ownerRef"]));
        }
        let reports = capture["capabilityReports"].as_array().unwrap();
        assert_eq!(reports.len(), 1, "{language}");
        let families = reports[0]["families"].as_array().unwrap();
        assert_eq!(families.len(), 4, "{language}");
        for kind in ["declarationName", "callee", "invocation", "reference"] {
            let entries = families
                .iter()
                .filter(|f| f["kind"] == kind)
                .collect::<Vec<_>>();
            assert_eq!(entries.len(), 1, "{language} {kind}");
            assert_eq!(entries[0]["available"], true);
            assert_eq!(entries[0]["diagnostic"], Value::Null);
            assert_eq!(entries[0]["gaps"], json!([]));
        }
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
        "fn main() { ordinary(); some_macro!(hidden()); }",
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
    let calls = capture["calls"].as_array().unwrap();
    assert!(calls.iter().any(|row| row["spelling"] == "ordinary"));
    assert!(!calls.iter().any(|row| row["spelling"] == "hidden"));
    let references = capture["references"].as_array().unwrap();
    assert!(references.iter().any(|row| row["spelling"] == "ordinary"));
    assert!(!references.iter().any(|row| row["spelling"] == "hidden"));
    let reports = capture["capabilityReports"].as_array().unwrap();
    assert_eq!(reports.len(), 1);
    let families = reports[0]["families"].as_array().unwrap();
    assert_eq!(families.len(), 4);
    assert!(families.iter().all(|family| family["available"] == true));
    let gaps = families
        .iter()
        .flat_map(|f| f["gaps"].as_array().unwrap())
        .collect::<Vec<_>>();
    assert!(!gaps.is_empty());
    let source = "fn main() { ordinary(); some_macro!(hidden()); }";
    for gap in gaps {
        assert_eq!(gap["range"]["encoding"], "utf8");
        let start = gap["range"]["start"].as_u64().unwrap() as usize;
        let end = gap["range"]["end"].as_u64().unwrap() as usize;
        assert!(source[start..end].contains("hidden"));
        assert!(!source[start..end].contains("ordinary"));
        assert!(!gap["diagnostic"].as_str().unwrap().is_empty());
    }
    assert!(capture.get("coverage").is_none());
}

#[test]
fn callable_values_and_explicit_import_aliases_keep_exact_reference_sites() {
    let temp = tempfile::tempdir().unwrap();
    let samples = [
        (
            "java",
            "A.java",
            "class A { void target() {} void run() { scheduler(this::target); } }",
            "target);",
            None,
        ),
        (
            "rust",
            "a.rs",
            "use module::original as local; fn target() {} fn run() { scheduler(target); }",
            "target);",
            Some("local"),
        ),
        (
            "python",
            "a.py",
            "def target():\n    pass\ndef run():\n    scheduler(target)\n",
            "target)",
            None,
        ),
        (
            "javascript",
            "a.js",
            "import {original as local} from 'module'; function target() {} function run() { scheduler(target); }",
            "target);",
            Some("local"),
        ),
    ];
    for (language, path, source, marker, alias) in samples {
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
        let references = capture["references"].as_array().unwrap();
        let start = source.find(marker).unwrap();
        let candidate = references
            .iter()
            .find(|r| r["range"]["start"] == start && r["spelling"] == "target")
            .unwrap();
        assert_witness(candidate, "spelling", source, start, start + "target".len());
        let calls = capture["calls"].as_array().unwrap();
        assert!(
            calls.iter().any(|c| c["spelling"] == "scheduler"),
            "{language}"
        );
        assert!(
            !calls.iter().any(|c| c["spelling"] == "target"),
            "{language}"
        );
        if let Some(alias) = alias {
            let declaration = capture["declarations"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["kind"] == "alias" && d["name"] == alias)
                .unwrap();
            let reference = references
                .iter()
                .find(|r| r["range"] == declaration["nameRange"])
                .unwrap();
            assert_eq!(reference["ownerRef"], declaration["parentRef"]);
            assert_eq!(reference["spelling"], alias);
            assert!(references.iter().any(|r| r["spelling"] == "original"));
        }
    }
}

#[test]
fn java_varargs_capture_exports_unique_parameter_name_joins() {
    let temp = tempfile::tempdir().unwrap();
    let source = "class A { void f(int... rest) {} void f(String... rest) {} A(long... rest) {} }";
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
    let declarations = capture["declarations"].as_array().unwrap();
    let references = capture["references"].as_array().unwrap();
    for (kind, parameter_type) in [
        ("method", "int"),
        ("method", "String"),
        ("constructor", "long"),
    ] {
        let callable = declarations
            .iter()
            .find(|row| {
                row["kind"] == kind && row["signature"]["parameterTypes"] == json!([parameter_type])
            })
            .unwrap();
        assert_eq!(callable["signature"]["variadic"], true);
        assert_eq!(
            callable["header"]["parameters"],
            json!([{"name":"rest","type":parameter_type,"variadic":true}])
        );
        let parameters = declarations
            .iter()
            .filter(|row| row["parentRef"] == callable["ref"] && row["kind"] == "parameter")
            .collect::<Vec<_>>();
        assert_eq!(parameters.len(), 1, "{kind} {parameter_type}");
        let parameter = parameters[0];
        assert_eq!(parameter["name"], "rest");
        let marker = format!("{parameter_type}... rest");
        let type_start = source.find(&marker).unwrap();
        let name_start = type_start + parameter_type.len() + 4;
        assert_eq!(
            parameter["range"],
            json!({"start":type_start,"end":name_start+4,"encoding":"utf8"})
        );
        assert_witness(
            callable,
            "header.parameters[0].type",
            source,
            type_start,
            type_start + parameter_type.len(),
        );
        assert_witness(
            callable,
            "signature.parameterTypes[0]",
            source,
            type_start,
            type_start + parameter_type.len(),
        );
        assert_witness(
            callable,
            "header.parameters[0].name",
            source,
            name_start,
            name_start + 4,
        );
        assert_witness(parameter, "name", source, name_start, name_start + 4);
        let declaration_names = declarations
            .iter()
            .filter(|row| row["nameRange"] == parameter["nameRange"])
            .collect::<Vec<_>>();
        assert_eq!(
            declaration_names.len(),
            1,
            "unambiguous declarationName join"
        );
        let candidates = references
            .iter()
            .filter(|row| row["range"] == parameter["nameRange"])
            .collect::<Vec<_>>();
        assert_eq!(
            candidates.len(),
            1,
            "one native reference at parameter name"
        );
        assert_eq!(candidates[0]["ownerRef"], callable["ref"]);
        assert_eq!(candidates[0]["spelling"], "rest");
        assert_witness(
            candidates[0],
            "spelling",
            source,
            name_start,
            name_start + 4,
        );
    }
    assert_eq!(
        declarations
            .iter()
            .filter(|row| row["kind"] == "parameter")
            .count(),
        3
    );
}
