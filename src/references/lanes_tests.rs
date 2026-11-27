//! A read whose syntax is parsed in lanes (#1321) answers as one that holds every tree at once.
use super::*;
use parallel_tests::{snapshot_in, snapshot_of};

/// Every catalog's language, a cross-file Rust glob whose unused names its scope drops (#986)
/// and a re-export chain.
const FILES: &[(&str, &str)] = &[
    (
        "src/lib.rs",
        "pub mod a;\npub mod b;\npub use a::value;\npub fn run() { value(); b::g(); }\n",
    ),
    ("src/a.rs", "pub fn value() {}\npub fn unused() {}\n"),
    (
        "src/b.rs",
        "use crate::a::*;\npub fn g() { value(); }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn t() { g(); }\n}\n",
    ),
    (
        "Use.java",
        "package app; import app.Provider; public class Use { Provider value; }",
    ),
    ("Provider.java", "package app; public class Provider {}"),
    ("use.py", "from provider import value\nvalue()\n"),
    ("provider.py", "def value():\n    return 1\n"),
    (
        "use.js",
        "import { value } from './provider.js'; export * from './again.js'; value();",
    ),
    ("again.js", "export { value } from './provider.js';"),
    ("provider.js", "export function value() {}"),
];

/// A whole read's references at `workers`: the canonical base and its Work, or the failure.
fn outcome(snapshot: &Snapshot, lane_bytes: Option<usize>, workers: usize) -> String {
    let _lanes = lane_bytes.map(set_lane_source_bytes_for_tests);
    let mut check = || Ok(());
    let request = parallel_check::RequestCheck::new(&mut check);
    let base = request.result(resolve(
        snapshot,
        Sources::all(),
        MAX_SAMPLE_LIMIT,
        workers,
        &request,
    ));
    match base {
        // The canonical base, each source's charge plan and facts, the catalog digests, and Work.
        Ok(base) => format!(
            "{} facts={} digests={:?} work={}",
            base.canonical().unwrap(),
            serde_json::Value::Array(
                base.source_facts
                    .iter()
                    .map(SourceFacts::canonical)
                    .collect()
            ),
            [
                base.rust_catalog_digest,
                base.python_catalog_digest,
                base.javascript_catalog_digest,
                base.java_catalog_digest
            ],
            base.work_used
        ),
        Err(error) => format!("failed: {error:#}"),
    }
}

/// One catalog per lane (0), two (the JS catalog alone), and every catalog in one lane.
const LANES: [Option<usize>; 3] = [Some(0), Some(150), None];

#[test]
fn lanes_resolve_as_one_pass_at_every_batch_size() {
    let snapshot = snapshot_of(FILES);
    for workers in [1, 2, 8] {
        let one_pass = outcome(&snapshot, Some(usize::MAX), workers);
        assert!(!one_pass.starts_with("failed"), "{one_pass}");
        for lanes in LANES {
            assert_eq!(
                outcome(&snapshot, lanes, workers),
                one_pass,
                "lanes {lanes:?}, {workers} workers"
            );
        }
    }
}

#[cfg(feature = "test-support")]
#[test]
fn lanes_fail_as_one_pass_under_every_work_limit() {
    // A source prepared in its lane under the whole allowance, but over its batch's own, is
    // prepared again under that allowance: the same failure, at the same site.
    let snapshot = snapshot_of(FILES);
    let used: usize = outcome(&snapshot, None, 2)
        .rsplit_once("work=")
        .unwrap()
        .1
        .parse()
        .unwrap();
    let mut failures = 0;
    for limit in (0..=used)
        .step_by((used / 80).max(1))
        .chain([used - 1, used])
    {
        let _limit = work::set_reference_work_limit_for_tests(limit);
        let one_pass = outcome(&snapshot, Some(usize::MAX), 2);
        failures += usize::from(one_pass.starts_with("failed"));
        for lanes in LANES {
            assert_eq!(
                outcome(&snapshot, lanes, 2),
                one_pass,
                "lanes {lanes:?}, limit {limit}"
            );
        }
    }
    assert!(failures > 10, "the sweep reaches failures: {failures}");
}

#[test]
fn a_private_glob_reads_its_source_parsed_again() {
    // The glob in `src/b.rs` drops the names its scope never spells, read in its own tree,
    // which a lane has dropped by pass 2.
    let snapshot = snapshot_of(FILES);
    let one_pass = outcome(&snapshot, Some(usize::MAX), 2);
    assert!(one_pass.contains("src/b.rs"), "{one_pass}");
    assert_eq!(outcome(&snapshot, Some(0), 2), one_pass);
}

#[test]
fn incremental_lanes_finish_as_a_full_read() {
    // #1315 kept syntax with lanes: the dirty source and parsed Rust sources keep their trees.
    let root = tempfile::tempdir().unwrap();
    let before = snapshot_in(root.path(), FILES);
    let versions = |snapshot: &Snapshot| -> Vec<String> {
        snapshot
            .documents
            .iter()
            .map(|doc| doc.file.hash.clone())
            .collect()
    };
    let mut check = || Ok(());
    let request = parallel_check::RequestCheck::new(&mut check);
    let (base, kept) = request
        .result(resolve_keeping(
            &before,
            Sources::all(),
            MAX_SAMPLE_LIMIT,
            2,
            Some(&versions(&before)),
            &request,
        ))
        .unwrap();
    let kept = kept.unwrap();
    for (path, text) in [
        (
            "src/b.rs",
            "use crate::a::*;\npub fn g() { value(); value(); }\n",
        ),
        ("use.py", "from provider import value\nvalue()\nvalue()\n"),
    ] {
        let after = snapshot_in(root.path(), &[(path, text)]);
        let dirty: BTreeSet<_> = after
            .documents
            .iter()
            .filter(|doc| doc.key.path == path)
            .map(|doc| doc.key.clone())
            .collect();
        let finish = |lanes: Option<usize>| {
            let _lanes = lanes.map(set_lane_source_bytes_for_tests);
            let versions = versions(&after);
            let preparation = incremental::prepare_with_workers(
                &after,
                2,
                Some(KeptRead {
                    syntax: Some(&kept),
                    versions: &versions,
                    dirty: &dirty,
                }),
                &mut || Ok(()),
            )
            .unwrap();
            incremental::finish_incremental(
                &after,
                preparation,
                &base,
                &dirty,
                &BTreeSet::new(),
                &mut || Ok(()),
            )
            .map(|done| done.references.canonical().unwrap())
            .map_err(|error| format!("{error:#}"))
        };
        let one_pass = finish(Some(usize::MAX));
        for lanes in LANES {
            assert_eq!(finish(lanes), one_pass, "{path}, lanes {lanes:?}");
        }
    }
}
