use super::*;
use crate::{indexer::IndexOptions, store::Store};
use std::{
    cell::RefCell,
    fs,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

struct Fixture {
    _state: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    root: PathBuf,
    root_id: String,
    capture: Capture,
    cancel: CancelFlag,
}

fn fixture(files: &[(&str, String)]) -> Fixture {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let path = workspace.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let root_id = store.root_id().to_owned();
    let root = fs::canonicalize(workspace.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(
        &IndexOptions::new(workspace.path().to_owned()),
        &cancel,
        &|_| {},
    )
    .unwrap();
    Fixture {
        _state: state,
        _workspace: workspace,
        root,
        root_id,
        capture,
        cancel,
    }
}

fn build(
    f: &Fixture,
    plan: ExtractionPlan,
    on_extract: &impl Fn(&DocumentKey),
    before_file: &(dyn Fn(&SourceFile) -> Result<()> + Sync),
) -> Result<Artifact> {
    build_native_with(
        &f.capture,
        &f.root,
        &f.root_id,
        &f.cancel,
        on_extract,
        plan,
        before_file,
    )
}

#[test]
fn changed_batch_extracts_exact_selection_and_keeps_full_header() {
    let f = fixture(&[
        (
            "a.java",
            "class A { int value; int read(){ return value; } }\n".into(),
        ),
        ("b.py", "def broken(:\n".into()),
        ("c.js", "function c(){ return value; }\n".into()),
        (
            "d.rs",
            "fn read(value: i32) -> i32 { opaque!(value); value }\n".into(),
        ),
    ]);
    let full = from_capture(&f.capture, &f.root, &f.root_id, &f.cancel).unwrap();
    assert!(!full.references.is_empty());
    assert!(
        full.capability_reports
            .iter()
            .any(|report| report.document.path == "d.rs"
                && report.families.iter().any(|family| !family.gaps.is_empty()))
    );
    let all: BTreeSet<_> = full
        .revision
        .documents
        .iter()
        .map(|document| document.key.path.clone())
        .collect();
    for paths in [
        BTreeSet::new(),
        BTreeSet::from(["b.py".to_owned()]),
        BTreeSet::from(["c.js".to_owned(), "d.rs".to_owned()]),
        all,
    ] {
        let mut extracted = Vec::new();
        let artifact =
            measure_captured_documents(&f.capture, &f.root, &f.root_id, &paths, &f.cancel, |key| {
                extracted.push(key.path.clone())
            })
            .unwrap();
        let expected_order: Vec<_> = full
            .revision
            .documents
            .iter()
            .filter(|document| paths.contains(&document.key.path))
            .map(|document| document.key.path.clone())
            .collect();
        assert_eq!(extracted, expected_order);
        assert_eq!(artifact.revision, full.revision);
        assert_eq!(artifact.coverage.len(), paths.len());
        assert_eq!(artifact.capability_reports.len(), paths.len());
        assert!(
            artifact
                .declarations
                .iter()
                .all(|row| paths.contains(&row.document.path))
        );
        assert_eq!(
            artifact.references,
            full.references
                .iter()
                .filter(|row| paths.contains(&row.document.path))
                .cloned()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            artifact.capability_reports,
            full.capability_reports
                .iter()
                .filter(|row| paths.contains(&row.document.path))
                .cloned()
                .collect::<Vec<_>>()
        );
        if paths.contains("b.py") {
            assert_eq!(
                artifact
                    .coverage
                    .iter()
                    .find(|c| c.document_path == "b.py")
                    .unwrap()
                    .state,
                "partial"
            );
        }
        let mut selected = artifact.clone();
        selected
            .revision
            .documents
            .retain(|document| paths.contains(&document.key.path));
        let files: Vec<_> = f
            .capture
            .files
            .iter()
            .filter(|file| paths.contains(&file.path))
            .cloned()
            .collect();
        selected.validate_structure(&files).unwrap();
        if !paths.is_empty() {
            let mut missing_report = selected.clone();
            missing_report.capability_reports.pop();
            assert!(missing_report.validate_structure(&files).is_err());
        }
        if !selected.references.is_empty() {
            selected.references[0].spelling.push('x');
            assert!(selected.validate_structure(&files).is_err());
        }
    }
    assert_eq!(f.capture.graph_projection_count(), 0);
}

const SEQUENTIAL: ExtractionPlan = ExtractionPlan {
    workers: 1,
    chunk: usize::MAX,
};
const PARALLEL: ExtractionPlan = ExtractionPlan {
    workers: 4,
    chunk: 3,
};

fn java(names: &[&str]) -> Vec<(String, String)> {
    names
        .iter()
        .map(|n| {
            (
                format!("src/{n}.java"),
                format!("class {n} {{ void go() {{ helper(); }} void helper() {{}} }}\n"),
            )
        })
        .collect()
}

fn file_name(file: &SourceFile) -> &str {
    file.path.rsplit('/').next().unwrap()
}

#[test]
fn plain_artifact_validation_does_not_extract_again() {
    let f = fixture(&[("main.js", "// No identifier occurrences.\n".into())]);
    let artifact = from_capture(&f.capture, &f.root, &f.root_id, &f.cancel).unwrap();
    assert_eq!(artifact.declarations.len(), 1);
    assert_eq!(artifact.declarations[0].kind, "module");
    assert!(
        artifact.references.is_empty()
            && artifact.calls.is_empty()
            && artifact.control_regions.is_empty()
    );
    // Extraction always authenticates its context. With no occurrences,
    // structural validation has no reason to request an extraction context.
    let _fault = inject_extraction_auth_fault(
        &artifact.revision.id,
        "javascript",
        ExtractionAuthFault::Undeclared,
    );
    assert!(build_native(&f.capture, &f.root, &f.root_id, &f.cancel).is_err());
    artifact
        .validate(&f.capture, &f.root, &f.root_id, &f.cancel)
        .unwrap();
    let mut invalid = artifact.clone();
    invalid.declarations[0].range.end = f.capture.files[0].text.len() + 1;
    assert!(
        invalid
            .validate(&f.capture, &f.root, &f.root_id, &f.cancel)
            .is_err()
    );
}

/// Several files in each of the five languages, with calls, control
/// regions and nested declarations, spread over several chunks.
#[test]
fn parallel_and_sequential_builds_are_identical() {
    let mut files = Vec::new();
    for i in 0..4 {
        files.push((
            format!("java/C{i}.java"),
            format!("class C{i} {{ int go(int x) {{ if (x > {i}) {{ return helper(x); }} for (int j = 0; j < x; j++) {{ helper(j); }} return 0; }} int helper(int y) {{ return y; }} class Inner {{ void run() {{ go(1); }} }} }}\n"),
        ));
        files.push((
            format!("rust/m{i}.rs"),
            format!("pub fn go{i}(x: u32) -> u32 {{ if x > {i} {{ helper(x) }} else {{ 0 }} }}\nfn helper(y: u32) -> u32 {{ y }}\nimpl S {{ fn run(&self) {{ go{i}(1); }} }}\nstruct S;\n#[cfg(test)]\nmod tests {{ #[test] fn t() {{ super::go{i}(2); }} }}\n"),
        ));
        files.push((
            format!("py/p{i}.py"),
            format!("def go{i}(x):\n    if x > {i}:\n        return helper(x)\n    return 0\n\ndef helper(y):\n    return y\n\nclass K:\n    def run(self):\n        go{i}(1)\n"),
        ));
        files.push((
            format!("js/j{i}.js"),
            format!("function go{i}(x) {{ if (x > {i}) {{ return helper(x); }} return 0; }}\nfunction helper(y) {{ return y; }}\nclass K {{ run() {{ go{i}(1); }} }}\n"),
        ));
        files.push((
            format!("ts/t{i}.ts"),
            format!("function go{i}(x: number): number {{ if (x > {i}) {{ return helper(x); }} return 0; }}\nfunction helper(y: number): number {{ return y; }}\n"),
        ));
    }
    let files: Vec<(&str, String)> = files.iter().map(|(p, t)| (p.as_str(), t.clone())).collect();
    let f = fixture(&files);
    let observed = |plan| {
        let keys = RefCell::new(Vec::new());
        let artifact = build(
            &f,
            plan,
            &|key: &DocumentKey| keys.borrow_mut().push(key.clone()),
            &|_| Ok(()),
        )
        .unwrap();
        (artifact, keys.into_inner())
    };
    let (sequential, sequential_keys) = observed(SEQUENTIAL);
    let (parallel, parallel_keys) = observed(PARALLEL);
    assert_eq!(sequential.revision.documents.len(), 20);
    for language in LANGUAGES {
        assert!(
            sequential
                .calls
                .iter()
                .any(|c| c.document.language == language)
                && sequential
                    .control_regions
                    .iter()
                    .any(|r| r.document.language == language),
            "fixture measures calls and regions in {language}"
        );
    }
    assert_eq!(parallel, sequential);
    assert_eq!(
        serde_json::to_vec(&parallel).unwrap(),
        serde_json::to_vec(&sequential).unwrap()
    );
    // The Measured observer fires once per document, in revision order.
    let order: Vec<_> = sequential
        .revision
        .documents
        .iter()
        .map(|d| d.key.clone())
        .collect();
    assert_eq!(sequential_keys, order);
    assert_eq!(parallel_keys, order);
    // The production path agrees and still validates.
    let production = from_capture(&f.capture, &f.root, &f.root_id, &f.cancel).unwrap();
    assert_eq!(production, sequential);
}

/// The earlier failing file is held until a later one has failed and
/// been stored. With two workers, one is held on B.java and the other
/// measures every other file in turn, so once F.java starts, D.java's
/// failure has completed. B.java's error is still the one reported.
#[test]
fn the_first_failing_file_in_order_is_reported() {
    let files = java(&["A", "B", "C", "D", "E", "F"]);
    let files: Vec<(&str, String)> = files.iter().map(|(p, t)| (p.as_str(), t.clone())).collect();
    let f = fixture(&files);
    let events = (Mutex::new(Vec::<String>::new()), Condvar::new());
    let record = |event: String| {
        events.0.lock().unwrap().push(event);
        events.1.notify_all();
    };
    let hook = |file: &SourceFile| -> Result<()> {
        let name = file_name(file).to_owned();
        match name.as_str() {
            "B.java" => {
                // The timeout is a liveness backstop only.
                let (guard, waited) = events
                    .1
                    .wait_timeout_while(
                        events.0.lock().unwrap(),
                        std::time::Duration::from_secs(60),
                        |events| !events.iter().any(|e| e == "started F.java"),
                    )
                    .unwrap();
                assert!(!waited.timed_out(), "F.java never started");
                drop(guard);
            }
            "D.java" => {}
            _ => {
                record(format!("started {name}"));
                return Ok(());
            }
        }
        record(format!("failed {name}"));
        anyhow::bail!("injected extraction failure: {name}")
    };
    let error = build(
        &f,
        ExtractionPlan {
            workers: 2,
            chunk: 6,
        },
        &|_| {},
        &hook,
    )
    .unwrap_err();
    let events = events.0.into_inner().unwrap();
    let at = |event: &str| events.iter().position(|e| e == event).unwrap();
    assert!(at("failed D.java") < at("started F.java"));
    assert!(at("started F.java") < at("failed B.java"));
    assert_eq!(error.to_string(), "injected extraction failure: B.java");
    let sequential = build(&f, SEQUENTIAL, &|_| {}, &|file| {
        ensure!(
            !matches!(file_name(file), "B.java" | "D.java"),
            "injected extraction failure: {}",
            file_name(file)
        );
        Ok(())
    })
    .unwrap_err();
    assert_eq!(sequential.to_string(), error.to_string());
}

/// Cancelling during the first chunk stops extraction before any later
/// chunk starts, with the typed cancellation error.
#[test]
fn cancellation_stops_extraction() {
    let files = java(&["A", "B", "C", "D", "E", "F"]);
    let files: Vec<(&str, String)> = files.iter().map(|(p, t)| (p.as_str(), t.clone())).collect();
    let f = fixture(&files);
    let started = AtomicUsize::new(0);
    let measured = AtomicUsize::new(0);
    let error = build(
        &f,
        ExtractionPlan {
            workers: 2,
            chunk: 2,
        },
        &|_| {
            measured.fetch_add(1, Ordering::SeqCst);
        },
        &|file| {
            started.fetch_add(1, Ordering::SeqCst);
            if file_name(file) == "A.java" {
                f.cancel.store(true, Ordering::SeqCst);
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("indexing cancelled"),
        "{error:#}"
    );
    assert!(started.load(Ordering::SeqCst) <= 2, "a later chunk started");
    assert!(measured.load(Ordering::SeqCst) <= 2);
}
