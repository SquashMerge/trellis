//! Offline oracle evidence. These tests never run or download an oracle.
use serde_json::Value;
use std::{collections::HashSet, fs, path::Path};

#[derive(Clone, Debug)]
struct Native<'a> {
    name: Option<&'a str>,
    start_line: u64,
    end_line: u64,
    source_set_id: &'a str,
    path: &'a str,
    sid: &'a str,
}

#[derive(Debug, PartialEq)]
enum Match<'a> {
    Unique(&'a str, &'a str, &'a str),
    Unavailable(&'static str),
}

// Correlation is by exact name and inclusive physical lines, never byte offsets.
fn correlate<'a>(
    name: Option<&str>,
    start: u64,
    end: u64,
    natives: &'a [Native<'a>],
    oracle_matches: usize,
) -> Match<'a> {
    let Some(name) = name else {
        return Match::Unavailable("anonymous");
    };
    if name == "<anonymous>" {
        return Match::Unavailable("anonymous");
    }
    let named: Vec<_> = natives.iter().filter(|n| n.name == Some(name)).collect();
    if named.is_empty() || oracle_matches == 0 {
        return Match::Unavailable("missing");
    }
    let lines: Vec<_> = named
        .iter()
        .filter(|n| n.start_line == start && n.end_line == end)
        .collect();
    if lines.is_empty() {
        return Match::Unavailable("lineMismatch");
    }
    if lines.len() != 1 || oracle_matches != 1 {
        return Match::Unavailable("ambiguous");
    }
    let n = lines[0];
    Match::Unique(n.source_set_id, n.path, n.sid)
}

#[test]
fn named_tuple_requires_unique_name_and_inclusive_lines() {
    let natives = [Native {
        name: Some("calc"),
        start_line: 2,
        end_line: 5,
        source_set_id: "s",
        path: "a.py",
        sid: "sid:v1:one",
    }];
    assert_eq!(
        correlate(Some("calc"), 2, 5, &natives, 1),
        Match::Unique("s", "a.py", "sid:v1:one")
    );
    assert_eq!(
        correlate(None, 2, 5, &natives, 0),
        Match::Unavailable("anonymous")
    );
    assert_eq!(
        correlate(Some("absent"), 2, 5, &natives, 1),
        Match::Unavailable("missing")
    );
    assert_eq!(
        correlate(Some("calc"), 2, 6, &natives, 1),
        Match::Unavailable("lineMismatch")
    );
    assert_eq!(
        correlate(Some("calc"), 2, 5, &natives, 2),
        Match::Unavailable("ambiguous")
    );
    let overloaded = [
        natives[0].clone(),
        Native {
            sid: "sid:v1:two",
            ..natives[0].clone()
        },
    ];
    assert_eq!(
        correlate(Some("calc"), 2, 5, &overloaded, 1),
        Match::Unavailable("ambiguous")
    );
}

#[test]
fn committed_arborist_goldens_are_complete_and_correlatable() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixtures = root.join("tests/fixtures/metrics");
    let mut seen = HashSet::new();
    for language in ["java", "python", "javascript", "rust"] {
        for entry in fs::read_dir(fixtures.join(language)).unwrap() {
            let file = entry.unwrap().path();
            if !file.is_file() {
                continue;
            }
            let relative = file.strip_prefix(root).unwrap().to_str().unwrap();
            let golden = fixtures.join("golden").join(format!(
                "{}.arborist.json",
                file.file_name().unwrap().to_str().unwrap()
            ));
            let data: Value = serde_json::from_slice(&fs::read(golden).unwrap()).unwrap();
            assert_eq!(data["path"], relative);
            assert!(
                !data["functions"].as_array().unwrap().is_empty(),
                "{relative}"
            );
            assert!(seen.insert(relative.to_string()));
            for function in data["functions"].as_array().unwrap() {
                let name = function["name"].as_str().unwrap();
                let start = function["start_line"].as_u64().unwrap();
                let end = function["end_line"].as_u64().unwrap();
                assert!(
                    !name.is_empty() && start > 0 && end >= start,
                    "{relative}: {name}"
                );
                for metric in ["cyclomatic", "cognitive", "sloc"] {
                    assert!(
                        function[metric].as_u64().is_some(),
                        "{relative}: {name}: {metric}"
                    );
                }
                let count = data["functions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|row| {
                        row["name"] == name && row["start_line"] == start && row["end_line"] == end
                    })
                    .count();
                assert_eq!(count, 1, "duplicate oracle tuple: {relative}: {name}");
            }
        }
    }
    assert!(seen.len() >= 12);
}

#[test]
fn rca_goldens_have_emitted_file_granularity_and_named_spaces() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for language in ["java", "python", "javascript", "rust"] {
        for entry in fs::read_dir(root.join("tests/fixtures/metrics").join(language)).unwrap() {
            let file = entry.unwrap().path();
            if !file.is_file() {
                continue;
            }
            let relative = file.strip_prefix(root).unwrap().to_str().unwrap();
            let golden = root.join("tests/fixtures/metrics/golden").join(format!(
                "{}.rca.json",
                file.file_name().unwrap().to_str().unwrap()
            ));
            let report: Value = serde_json::from_slice(&fs::read(golden).unwrap()).unwrap();
            assert_eq!(report["name"], relative);
            assert_eq!(report["kind"], "unit");
            assert!(report["metrics"]["cyclomatic"].is_object());
            // A unit-level value is not a sum of declaration metrics.
            for space in report["spaces"].as_array().unwrap() {
                assert!(space["start_line"].as_u64().is_some());
                assert!(space["end_line"].as_u64().is_some());
                assert!(space["kind"].as_str().is_some());
            }
        }
    }
}

#[test]
fn existing_extraction_and_medium_templates_have_real_oracle_records() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let gold = root.join("tests/fixtures/metrics/golden");
    for (source, golden) in [
        (
            "tests/fixtures/extraction/fixture/flow.js",
            "extraction-flow.js",
        ),
        (
            "tests/fixtures/extraction/edge-fixture/cases.js",
            "extraction-cases.js",
        ),
        (
            "tools/synthetic-cohorts/templates/medium/java.java",
            "medium-java.java",
        ),
        (
            "tools/synthetic-cohorts/templates/medium/python.py",
            "medium-python.py",
        ),
        (
            "tools/synthetic-cohorts/templates/medium/javascript.js",
            "medium-javascript.js",
        ),
        (
            "tools/synthetic-cohorts/templates/medium/rust.rs",
            "medium-rust.rs",
        ),
    ] {
        assert!(root.join(source).is_file());
        let arborist: Value = serde_json::from_slice(
            &fs::read(gold.join(format!("{golden}.arborist.json"))).unwrap(),
        )
        .unwrap();
        let rca: Value =
            serde_json::from_slice(&fs::read(gold.join(format!("{golden}.rca.json"))).unwrap())
                .unwrap();
        assert_eq!(arborist["path"], source);
        assert_eq!(rca["name"], source);
        assert!(arborist["functions"].as_array().is_some());
        assert!(rca["spaces"].as_array().is_some());
    }
}

fn fixture_sources() -> Vec<(&'static str, &'static str)> {
    vec![
        ("java", "tests/fixtures/metrics/java/CallbackTest.java"),
        ("java", "tests/fixtures/metrics/java/Controls.java"),
        ("java", "tests/fixtures/metrics/java/Example.java"),
        ("python", "tests/fixtures/metrics/python/controls.py"),
        ("python", "tests/fixtures/metrics/python/example.py"),
        ("python", "tests/fixtures/metrics/python/test_callback.py"),
        (
            "javascript",
            "tests/fixtures/metrics/javascript/callback.test.js",
        ),
        (
            "javascript",
            "tests/fixtures/metrics/javascript/controls.js",
        ),
        ("javascript", "tests/fixtures/metrics/javascript/example.js"),
        ("rust", "tests/fixtures/metrics/rust/callback.rs"),
        ("rust", "tests/fixtures/metrics/rust/controls.rs"),
        ("rust", "tests/fixtures/metrics/rust/example.rs"),
        ("javascript", "tests/fixtures/extraction/fixture/flow.js"),
        (
            "javascript",
            "tests/fixtures/extraction/edge-fixture/cases.js",
        ),
        ("java", "tools/synthetic-cohorts/templates/medium/java.java"),
        (
            "python",
            "tools/synthetic-cohorts/templates/medium/python.py",
        ),
        (
            "javascript",
            "tools/synthetic-cohorts/templates/medium/javascript.js",
        ),
        ("rust", "tools/synthetic-cohorts/templates/medium/rust.rs"),
    ]
}

#[test]
fn inspect_real_native_oracle_inventory() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        capture::Capture, indexer::IndexOptions, model::CancelFlag, native_evidence::from_capture,
    };
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = tempfile::tempdir().unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(dir.path()), dir.path())
            .unwrap();
    for (_, relative) in fixture_sources() {
        let target = dir.path().join(relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(repo.join(relative), target).unwrap();
    }
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(dir.path().into()), &cancel, &|_| {}).unwrap();
    let artifact = from_capture(&capture, dir.path(), &identity.record_id, &cancel).unwrap();
    let mut records = Vec::new();
    for (language, relative) in fixture_sources() {
        let source = fs::read_to_string(repo.join(relative)).unwrap();
        let declarations: Vec<_> = artifact
            .declarations
            .iter()
            .filter(|d| d.document.path == relative && trellis::metrics::callable(&d.kind))
            .cloned()
            .collect();
        let calls: Vec<_> = artifact
            .calls
            .iter()
            .filter(|c| c.document.path == relative)
            .cloned()
            .collect();
        let regions: Vec<_> = artifact
            .control_regions
            .iter()
            .filter(|r| r.document.path == relative)
            .cloned()
            .collect();
        let rows = trellis::metrics::compute(
            language,
            &source,
            &declarations,
            &calls,
            &regions,
            &mut || Ok(()),
        )
        .unwrap();
        for d in declarations {
            let start = source.as_bytes()[..d.range.start]
                .iter()
                .filter(|b| **b == b'\n')
                .count()
                + 1;
            let end = source.as_bytes()[..d.range.end]
                .iter()
                .filter(|b| **b == b'\n')
                .count()
                + usize::from(d.range.end > 0 && source.as_bytes()[d.range.end - 1] != b'\n');
            let score = rows.iter().find(|r| r.sid == d.syntax_id).unwrap();
            let identity = |source_set: &str| {
                serde_json::json!({
                    "sourceSet":source_set,"path":relative,"language":language,
                    "ancestors":d.ancestors,"declaration":d.key
                })
            };
            let mut registry = trellis::native_ids::IdentityRegistry::default();
            assert_eq!(
                registry
                    .stable(&identity(&d.document.source_set_id))
                    .unwrap(),
                d.syntax_id
            );
            let stable_set = "source-set:v1:oracle-fixtures";
            let stable_sid = registry.stable(&identity(stable_set)).unwrap();
            records.push(serde_json::json!({"path":relative,"language":language,"name":d.name,"startLine":start,"endLine":end,"kind":d.kind,"sid":stable_sid,"sourceSetId":stable_set,"score":score.metrics,"key":d.key,"ancestors":d.ancestors}));
        }
    }
    if let Ok(path) = std::env::var("TRELLIS_ORACLE_INVENTORY") {
        fs::write(path, serde_json::to_vec_pretty(&records).unwrap()).unwrap();
    }
    assert!(!records.is_empty());
    assert_real_oracle_comparisons(&records);
}

fn golden_stem(path: &str) -> String {
    match path {
        "tests/fixtures/extraction/fixture/flow.js" => "extraction-flow.js".to_string(),
        "tests/fixtures/extraction/edge-fixture/cases.js" => "extraction-cases.js".to_string(),
        "tools/synthetic-cohorts/templates/medium/java.java" => "medium-java.java".to_string(),
        "tools/synthetic-cohorts/templates/medium/python.py" => "medium-python.py".to_string(),
        "tools/synthetic-cohorts/templates/medium/javascript.js" => {
            "medium-javascript.js".to_string()
        }
        "tools/synthetic-cohorts/templates/medium/rust.rs" => "medium-rust.rs".to_string(),
        _ => Path::new(path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string(),
    }
}

fn rca_functions<'a>(spaces: &'a [Value], result: &mut Vec<&'a Value>) {
    for space in spaces {
        if matches!(space["kind"].as_str(), Some("function" | "closure")) {
            result.push(space);
        }
        rca_functions(space["spaces"].as_array().unwrap(), result);
    }
}

fn dotted<'a>(value: &'a Value, field: &str) -> &'a Value {
    field.split('.').fold(value, |node, key| &node[key])
}

fn assert_real_oracle_comparisons(records: &[Value]) {
    use std::collections::HashMap;
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let gold = repo.join("tests/fixtures/metrics/golden");
    let register: Value =
        serde_json::from_slice(&fs::read(gold.join("comparisons.json")).unwrap()).unwrap();
    assert_eq!(register["version"], "metrics:v1");
    assert_eq!(register["sourceSetId"], "source-set:v1:oracle-fixtures");
    let mut cache: HashMap<String, Value> = HashMap::new();
    let mut matched = 0;
    let mut diverged = 0;
    let mut unavailable = 0;
    for entry in register["comparisons"].as_array().unwrap() {
        let path = entry["path"].as_str().unwrap();
        let oracle = entry["oracle"].as_str().unwrap();
        let filename = format!("{}.{}.json", golden_stem(path), oracle);
        let raw = cache.entry(filename.clone()).or_insert_with(|| {
            serde_json::from_slice(&fs::read(gold.join(&filename)).unwrap()).unwrap()
        });
        let mut functions = Vec::new();
        if oracle == "arborist" {
            functions.extend(raw["functions"].as_array().unwrap().iter());
        } else {
            rca_functions(raw["spaces"].as_array().unwrap(), &mut functions);
        }
        let name = entry["name"].as_str();
        let start = entry["startLine"].as_u64().unwrap();
        let end = entry["endLine"].as_u64().unwrap();
        let matching_oracle: Vec<_> = functions
            .iter()
            .filter(|f| {
                f["name"].as_str() == name
                    && f["start_line"].as_u64() == Some(start)
                    && f["end_line"].as_u64() == Some(end)
            })
            .collect();
        let native: Vec<_> = records
            .iter()
            .filter(|row| row["path"] == path && row["kind"] != "anonymousFunction")
            .map(|row| Native {
                name: row["name"].as_str(),
                start_line: row["startLine"].as_u64().unwrap(),
                end_line: row["endLine"].as_u64().unwrap(),
                source_set_id: row["sourceSetId"].as_str().unwrap(),
                path: row["path"].as_str().unwrap(),
                sid: row["sid"].as_str().unwrap(),
            })
            .collect();
        if let Some(reason) = entry["oracleUnavailable"].as_str() {
            unavailable += 1;
            assert!(
                entry["unavailableJustification"]
                    .as_str()
                    .is_some_and(|text| text.len() > 50)
            );
            assert!(entry["matchedNative"].is_null());
            assert!(entry["cells"].as_array().unwrap().is_empty());
            if let Some(sid) = entry["nativeSid"].as_str() {
                assert_eq!(reason, "anonymous");
                assert!(records.iter().any(|row| row["path"] == path
                    && row["kind"] == "anonymousFunction"
                    && row["sid"] == sid));
            } else if reason == "granularityMismatch" {
                assert_eq!(matching_oracle.len(), 1);
                assert!(!matching_oracle[0]["spaces"].as_array().unwrap().is_empty());
            } else {
                match correlate(name, start, end, &native, matching_oracle.len()) {
                    Match::Unavailable(found) => assert_eq!(found, reason),
                    other => panic!("expected oracle unavailability {reason}, got {other:?}"),
                }
            }
            continue;
        }
        assert_eq!(matching_oracle.len(), 1, "{path}: {name:?}");
        let sid = entry["matchedNative"]["sid"].as_str().unwrap();
        let source_set_id = entry["matchedNative"]["sourceSetId"].as_str().unwrap();
        assert_eq!(entry["matchedNative"]["path"], path);
        assert_eq!(
            correlate(name, start, end, &native, matching_oracle.len()),
            Match::Unique(source_set_id, path, sid),
            "{path}: {name:?}"
        );
        let measured = records
            .iter()
            .find(|row| row["path"] == path && row["sid"] == sid)
            .unwrap();
        let emitted = matching_oracle[0];
        assert!(!entry["cells"].as_array().unwrap().is_empty());
        let candidate_fields: &[&str] = if oracle == "arborist" {
            &["cyclomatic", "cognitive", "sloc"]
        } else {
            &[
                "cyclomatic.sum",
                "cognitive.sum",
                "loc.sloc",
                "halstead.n1",
                "halstead.n2",
                "halstead.N1",
                "halstead.N2",
                "halstead.vocabulary",
                "halstead.length",
                "loc.ploc",
                "loc.cloc",
                "halstead.volume",
                "halstead.difficulty",
                "halstead.effort",
                "halstead.bugs",
                "nargs.total_functions",
                "nexits.sum",
            ]
        };
        let emitted_fields: HashSet<_> = candidate_fields
            .iter()
            .copied()
            .filter(|field| {
                let value = if oracle == "arborist" {
                    &emitted[*field]
                } else {
                    dotted(&emitted["metrics"], field)
                };
                value.is_number()
            })
            .collect();
        let recorded_fields: HashSet<_> = entry["cells"]
            .as_array()
            .unwrap()
            .iter()
            .map(|cell| cell["oracleField"].as_str().unwrap())
            .collect();
        assert_eq!(
            recorded_fields, emitted_fields,
            "unregistered comparable emitted field: {path}: {name:?}"
        );
        for cell in entry["cells"].as_array().unwrap() {
            let oracle_field = cell["oracleField"].as_str().unwrap();
            let field = cell["metric"].as_str().unwrap();
            let actual = if oracle == "arborist" {
                &emitted[oracle_field]
            } else {
                dotted(&emitted["metrics"], oracle_field)
            };
            let ours = dotted(&measured["score"], field);
            assert_eq!(cell["oracleValue"], *actual, "{path}: {oracle_field}");
            assert_eq!(cell["oursValue"], *ours, "{path}: {field}");
            let same = (actual.as_f64().unwrap() * 1_000_000.0).round()
                == (ours.as_f64().unwrap() * 1_000_000.0).round();
            if same {
                assert_eq!(cell["category"], "agreement", "{path}: {field}");
                assert!(cell["justification"].is_null());
            } else {
                diverged += 1;
                assert_eq!(cell["category"], "ruleDivergence", "{path}: {field}");
                assert!(
                    cell["justification"]
                        .as_str()
                        .is_some_and(|text| text.len() > 50)
                );
            }
            matched += 1;
        }
    }
    // Every emitted callable row has a register entry; anonymous native-only rows
    // are separate and cannot be mistaken for an agreement.
    for (_, path) in fixture_sources() {
        for oracle in ["arborist", "rca"] {
            let filename = format!("{}.{}.json", golden_stem(path), oracle);
            let raw = &cache[&filename];
            let count = if oracle == "arborist" {
                raw["functions"].as_array().unwrap().len()
            } else {
                let mut functions = Vec::new();
                rca_functions(raw["spaces"].as_array().unwrap(), &mut functions);
                functions.len()
            };
            let registered = register["comparisons"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| {
                    entry["path"] == path
                        && entry["oracle"] == oracle
                        && entry["nativeSid"].is_null()
                })
                .count();
            assert_eq!(registered, count, "missing emitted {oracle} row in {path}");
        }
    }
    assert!(
        matched > 5000 && diverged > 100 && unavailable > 0,
        "{matched} compared, {diverged} divergent, {unavailable} unavailable"
    );
}
