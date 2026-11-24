#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    model::{CancelFlag, IndexPin},
    store::Store,
};

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, source) in [
        ("src/lib.rs", "pub mod high; pub mod low;\n"),
        ("src/high.rs", "pub fn upper() {}\n"),
        (
            "src/low.rs",
            "pub fn lower() { crate::high::upper(); }\n#[cfg(test)] mod tests { fn test_ref() { crate::high::upper(); } }\n#[cfg(feature = \"test-support\")] pub fn support() { crate::high::upper(); }\n",
        ),
    ] {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().into()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let pin = store
        .publish_native(&graph, &capture, &native, &leader, baseline, &cancel)
        .unwrap();
    drop(leader);
    (state, root, store, pin)
}
fn config(root: &std::path::Path, text: &str) {
    fs::create_dir_all(root.join(".trellis")).unwrap();
    fs::write(root.join(".trellis/rules.toml"), text).unwrap();
}
const RULES: &str = r#"version = 1
[layers.high]
order = 1
paths = ["src/high.rs"]
[layers.low]
order = 2
paths = ["src/low.rs"]
[roles.lower]
select = { name = "lower" }
[roles.upper]
select = { name = "upper" }
[[rules]]
id = "must-call-upper"
for_each = "lower"
require = [{ calls = "upper" }]
"#;
fn read(store: &Store, kind: &str, args: Value) -> Value {
    let response = store.evidence_response().unwrap();
    let payload = response
        .architecture_response(kind, &args, &mut || Ok(()))
        .unwrap();
    response.finish(payload).unwrap()
}
fn db_metadata(state: &std::path::Path) -> Vec<(String, u64, std::time::SystemTime)> {
    let index = fs::read_dir(state.join("cache/indexes"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_dir())
        .unwrap();
    let mut result: Vec<_> = fs::read_dir(index)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_name().to_string_lossy().starts_with("index.db"))
        .map(|e| {
            let metadata = e.metadata().unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                metadata.len(),
                metadata.modified().unwrap(),
            )
        })
        .collect();
    result.sort();
    result
}

fn index_path(state: &std::path::Path) -> std::path::PathBuf {
    fs::read_dir(state.join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db")
}

fn publish_current(store: &Store, root: &std::path::Path) {
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.into()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let leader = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &leader,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
}

#[test]
fn architecture_incremental_scoped_fallback() {
    use trellis::references::{self, MAX_SAMPLE_LIMIT};
    let (_state, root, store, pin) = fixture();
    let base = store.architecture_build_for_tests(None, None).unwrap();
    let canonical = base.core.canonical().unwrap();
    let work = canonical["references"]["workUsed"].as_u64().unwrap() as usize;
    let args = json!({"expectedBasis":pin,"paths":["src/high.rs"],"sampleLimit":MAX_SAMPLE_LIMIT});
    let before = read(&store, "references", args.clone());
    let snapshot = store.architecture_reference_snapshot_for_tests().unwrap();
    let all = references::resolve_all(&snapshot, &mut || Ok(())).unwrap();
    let selected = all
        .select(&["src/high.rs".into()], MAX_SAMPLE_LIMIT, &mut || Ok(()))
        .unwrap();
    let scoped = references::project(
        &snapshot,
        &["src/high.rs".into()],
        MAX_SAMPLE_LIMIT,
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(selected.data, scoped.data);
    let _work_limit = references::set_reference_work_limit_for_tests(work - 1);
    let error = references::resolve_all(&snapshot, &mut || Ok(()))
        .err()
        .unwrap();
    assert!(error.to_string().starts_with("bounded_work:"), "{error}");
    assert_eq!(
        references::project(
            &snapshot,
            &["src/high.rs".into()],
            MAX_SAMPLE_LIMIT,
            &mut || Ok(())
        )
        .unwrap()
        .data,
        selected.data,
        "scoped Work remains scoped after the all-source bound fails"
    );
    fs::write(
        root.path().join("src/high.rs"),
        "this live edit was not captured",
    )
    .unwrap();
    assert_eq!(
        store
            .architecture_scoped_response_for_tests("references", &args)
            .unwrap(),
        before,
        "fallback uses captured inputs, not the live checkout"
    );
    let response = store.evidence_response().unwrap();
    let bounded = response
        .architecture_response("references", &json!({"expectedBasis":pin}), &mut || Ok(()))
        .err()
        .unwrap();
    assert!(
        bounded.to_string().starts_with("bounded_work:"),
        "{bounded}"
    );
}

#[test]
fn architecture_incremental_preparation_failure_retries_full_read_b_first() {
    use crate::graph_fixtures::{Publisher, Spec, bind};
    use trellis::{
        references::set_reference_work_limit_for_tests,
        store::set_architecture_semantic_rows_for_tests,
    };
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[
        ("src/lib.rs", "pub mod caller; pub mod target;\n"),
        ("src/caller.rs", "pub fn run() { crate::target::f(); }\n"),
        ("src/target.rs", "pub fn f() {}\n"),
        ("src/idle.rs", "pub fn idle() {}\n"),
    ]);
    spec.bindings = vec![bind("run", 0, "f")];
    publisher.publish(&spec);
    let store = &publisher.store;
    let base = store.architecture_build_for_tests(None, None).unwrap();
    assert!(
        base.core.canonical().unwrap()["dependencies"]["documents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|doc| doc["semantic"]["rows"].as_u64().unwrap() > 0)
    );
    let _work_limit = set_reference_work_limit_for_tests(0);
    struct ResetRows;
    impl Drop for ResetRows {
        fn drop(&mut self) {
            set_architecture_semantic_rows_for_tests(2_000_000);
        }
    }
    let _rows = ResetRows;
    set_architecture_semantic_rows_for_tests(0);
    let full = store
        .architecture_build_for_tests(None, None)
        .err()
        .unwrap();
    assert_eq!(
        full.to_string(),
        "bounded_work: architecture semantic evidence"
    );
    let incremental = store
        .architecture_build_for_tests(Some(&base.core), None)
        .err()
        .unwrap();
    assert_eq!(
        incremental.to_string(),
        full.to_string(),
        "ordinary preparation failure retries independent full A/B before full compute"
    );
    assert!(incremental.to_string().starts_with("bounded_work:"));
    assert_eq!(
        trellis::rules::diagnostic(&incremental),
        trellis::rules::diagnostic(&full)
    );
}

#[test]
fn architecture_incremental_version_identity() {
    let (state, root, store, pin) = fixture();
    let baseline = store
        .architecture_fingerprints_for_tests(Some(pin))
        .unwrap();
    let key = baseline
        .documents
        .keys()
        .find(|key| key.path == "src/high.rs")
        .unwrap()
        .clone();
    let first = baseline.documents[&key].clone();
    drop(store);
    let db = rusqlite::Connection::open(index_path(state.path())).unwrap();
    let original_vid: i64 = db
        .query_row(
            "SELECT vid FROM document_versions WHERE id=?1",
            [&first.native.version_id],
            |row| row.get(0),
        )
        .unwrap();
    drop(db);
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    fs::write(
        root.path().join("src/high.rs"),
        "pub fn upper() { /* body edit */ }\n",
    )
    .unwrap();
    publish_current(&store, root.path());
    let edited = store
        .architecture_fingerprints_for_tests(None)
        .unwrap()
        .documents[&key]
        .clone();
    assert_ne!(first.native.version_id, edited.native.version_id);
    assert_eq!(
        first.sids, edited.sids,
        "a body comment is not a declaration identity"
    );
    drop(store);

    // Remap both versions and every SQL foreign-key column. The changed content now owns
    // the old surrogate, while every retained revision still names its original content.
    let db = rusqlite::Connection::open(index_path(state.path())).unwrap();
    let edited_vid: i64 = db
        .query_row(
            "SELECT vid FROM document_versions WHERE id=?1",
            [&edited.native.version_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(original_vid, edited_vid);
    let tables: Vec<String> = db
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let tables: Vec<_> = tables
        .into_iter()
        .filter(|table| {
            db.prepare(&format!("PRAGMA table_info(\"{table}\")"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .any(|column| column.unwrap() == "vid")
        })
        .collect();
    db.execute_batch("BEGIN IMMEDIATE; PRAGMA defer_foreign_keys=ON")
        .unwrap();
    for (from, to) in [
        (original_vid, -1),
        (edited_vid, original_vid),
        (-1, edited_vid),
    ] {
        for table in &tables {
            db.execute(
                &format!("UPDATE \"{table}\" SET vid=?1 WHERE vid=?2"),
                [to, from],
            )
            .unwrap();
        }
    }
    db.execute_batch("COMMIT").unwrap();
    let violations: i64 = db
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
    drop(db);
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_eq!(
        store
            .architecture_fingerprints_for_tests(None)
            .unwrap()
            .documents[&key],
        edited
    );
    assert_eq!(
        store
            .architecture_fingerprints_for_tests(Some(pin))
            .unwrap()
            .documents[&key],
        first
    );
    fs::write(root.path().join("src/high.rs"), "pub fn upper() {}\n").unwrap();
    publish_current(&store, root.path());
    assert_eq!(
        store
            .architecture_fingerprints_for_tests(None)
            .unwrap()
            .documents[&key],
        first
    );
}

#[test]
fn architecture_incremental_coverage() {
    let (state, root, store, _) = fixture();
    let baseline = store.architecture_fingerprints_for_tests(None).unwrap();
    let key = baseline
        .documents
        .keys()
        .find(|key| key.path == "src/high.rs")
        .unwrap()
        .clone();
    let first = baseline.documents[&key].clone();
    drop(store);
    for (assignments, requested, selected, status, diagnostic) in [
        ("coverage_requested=0", false, true, "complete", None),
        (
            "coverage_requested=1,coverage_selected=0",
            true,
            false,
            "complete",
            None,
        ),
        (
            "coverage_selected=1,coverage_state='partial',coverage_diagnostic='one'",
            true,
            true,
            "partial",
            Some("one"),
        ),
        (
            "coverage_diagnostic='two'",
            true,
            true,
            "partial",
            Some("two"),
        ),
        (
            "coverage_state='failed',coverage_diagnostic='two'",
            true,
            true,
            "failed",
            Some("two"),
        ),
        (
            "coverage_state='unsupported'",
            true,
            true,
            "unsupported",
            Some("two"),
        ),
        (
            "coverage_state='omitted'",
            true,
            true,
            "omitted",
            Some("two"),
        ),
        (
            "coverage_state='notRequested'",
            true,
            true,
            "notRequested",
            Some("two"),
        ),
        (
            "coverage_state='complete',coverage_diagnostic=NULL",
            true,
            true,
            "complete",
            None,
        ),
    ] {
        let db = rusqlite::Connection::open(index_path(state.path())).unwrap();
        db.execute(
            &format!("UPDATE revision_documents SET {assignments} WHERE path='src/high.rs'"),
            [],
        )
        .unwrap();
        drop(db);
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let current = store.architecture_fingerprints_for_tests(None).unwrap();
        let mut expected = first.clone();
        expected.native.coverage_requested = requested;
        expected.native.coverage_selected = selected;
        expected.native.coverage_state = status.into();
        expected.native.coverage_diagnostic = diagnostic.map(str::to_owned);
        assert_eq!(current.documents[&key], expected, "{assignments}");
        assert_eq!(current.producers, baseline.producers);
        assert_eq!(current.selections, baseline.selections);
        drop(store);
    }
    let db = rusqlite::Connection::open(index_path(state.path())).unwrap();
    db.execute_batch("UPDATE revision_documents SET ordinal=ordinal+100; UPDATE revision_documents SET ordinal=102-ordinal,class_complete=1-class_complete").unwrap();
    let stat: String = db
        .query_row(
            "SELECT capture_stat FROM revision_documents WHERE path='src/high.rs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut stat: trellis::capture::CaptureStat = serde_json::from_str(&stat).unwrap();
    stat.mtime_seconds = stat.mtime_seconds.map(|seconds| seconds + 1);
    db.execute(
        "UPDATE revision_documents SET capture_stat=?1 WHERE path='src/high.rs'",
        [serde_json::to_string(&stat).unwrap()],
    )
    .unwrap();
    drop(db);
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert_eq!(
        store.architecture_fingerprints_for_tests(None).unwrap(),
        baseline
    );
    drop(store);
    let db = rusqlite::Connection::open(index_path(state.path())).unwrap();
    db.execute("UPDATE graph_projections SET state='unavailable' WHERE gid IN (SELECT gid FROM revision_documents WHERE path='src/high.rs')", []).unwrap();
    drop(db);
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let error = store.architecture_fingerprints_for_tests(None).unwrap_err();
    assert!(
        format!("{error:#}").contains("incompatible_index:"),
        "{error:#}"
    );
}

#[test]
fn architecture_incremental_captured_root_config_fallback() {
    let (_state, root, store, _) = fixture();
    assert!(
        store
            .architecture_reference_snapshot_for_tests()
            .unwrap()
            .root_tsconfig
            .is_none()
    );
    fs::write(
        root.path().join("jsconfig.json"),
        "{\"compilerOptions\":{}}",
    )
    .unwrap();
    publish_current(&store, root.path());
    let snapshot = store.architecture_reference_snapshot_for_tests().unwrap();
    let config = snapshot.root_tsconfig.unwrap();
    assert_eq!(config.path, "jsconfig.json");
    assert_eq!(config.bytes, b"{\"compilerOptions\":{}}");
    fs::write(root.path().join("tsconfig.json"), "{}").unwrap();
    publish_current(&store, root.path());
    let snapshot = store.architecture_reference_snapshot_for_tests().unwrap();
    let config = snapshot.root_tsconfig.unwrap();
    assert_eq!(config.path, "tsconfig.json");
    assert_eq!(config.bytes, b"{}");
}
#[test]
fn captured_references_keep_stored_roles_and_call_rules_unknown_without_writes() {
    let (state, root, store, pin) = fixture();
    config(root.path(), RULES);
    let metadata = db_metadata(state.path());
    let args = json!({"expectedBasis":pin});
    let refs = read(&store, "references", args.clone());
    let prod = refs["production"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["source"]["path"] == "src/low.rs" && e["target"]["path"] == "src/high.rs")
        .unwrap();
    assert_eq!(prod["count"], 2, "feature support remains production");
    let tests = refs["tests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["source"]["path"] == "src/low.rs" && e["target"]["path"] == "src/high.rs")
        .unwrap();
    assert_eq!(tests["count"], 1);
    let check = read(&store, "check", args);
    assert_eq!(check["indexGeneration"], pin.index_generation.to_string());
    assert_eq!(check["indexRevision"], pin.index_revision);
    assert!(check["counts"]["deviates"].as_u64().unwrap() > 0);
    let call = check["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["rule"] == "must-call-upper")
        .unwrap();
    assert_eq!(call["status"], "unknown");
    assert!(
        !call["constraints"][0]["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db_metadata(state.path()),
        metadata,
        "evaluation must not write index or WAL"
    );
    fs::write(
        root.path().join("src/low.rs"),
        "this is not the captured Rust",
    )
    .unwrap();
    assert_eq!(
        read(&store, "references", json!({"expectedBasis":pin})),
        refs,
        "never reread source from disk"
    );
}
#[test]
fn live_config_has_separate_digest_and_exact_pair_exception_round_trips() {
    let (_state, root, store, pin) = fixture();
    config(root.path(), RULES);
    let args = json!({"expectedBasis":pin});
    let before = read(&store, "check", args.clone());
    let edge = before["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["rule"] == "$layers" && i["status"] == "deviates")
        .unwrap();
    let identity = |value: &Value| {
        format!(
            "{{ language = {:?}, path = {:?} }}",
            value["language"].as_str().unwrap(),
            value["path"].as_str().unwrap()
        )
    };
    config(
        root.path(),
        &format!(
            "{RULES}\n[[exceptions]]\nrule = \"$layers\"\ntarget = {}\ndestination = {}\nreason = \"Reviewed existing upward pair\"\n",
            identity(&edge["target"]),
            identity(&edge["destination"])
        ),
    );
    let after = read(&store, "check", args.clone());
    assert_eq!(before["indexRevision"], after["indexRevision"]);
    assert_ne!(
        before["rulesFile"]["contentHash"],
        after["rulesFile"]["contentHash"]
    );
    let accepted = after["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| {
            i["rule"] == "$layers"
                && i["target"] == edge["target"]
                && i["destination"] == edge["destination"]
        })
        .unwrap();
    assert_eq!(accepted["status"], "deviates");
    assert_eq!(accepted["excepted"], true);
    assert_eq!(accepted["exceptionReason"], "Reviewed existing upward pair");
    let explanation = read(
        &store,
        "explain",
        json!({"expectedBasis":pin,"rule":"$layers","target":edge["target"],"destination":edge["destination"]}),
    );
    assert_eq!(
        explanation["instances"].as_array().unwrap(),
        &vec![accepted.clone()]
    );
    assert_eq!(explanation["counts"]["deviates"], 0);
    let prompt = read(&store, "prompt", args);
    assert_eq!(prompt["rulesFile"], after["rulesFile"]);
    assert!(
        prompt["rulesText"]
            .as_str()
            .unwrap()
            .contains("Reviewed existing upward pair")
    );
}
#[test]
fn config_and_pin_failures_are_atomic_and_cancelled_reads_do_not_publish_results() {
    let (_state, root, store, pin) = fixture();
    assert!(read(&store, "prompt", json!({"expectedBasis":pin}))["rulesText"].is_null());
    let response = store.evidence_response().unwrap();
    assert!(
        response
            .architecture_response("check", &json!({}), &mut || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("rules_not_found")
    );
    config(
        root.path(),
        "version = 1\n[roles.bad]\nselect = { mystery = \"x\" }\n",
    );
    let error = response
        .architecture_response("check", &json!({}), &mut || Ok(()))
        .unwrap_err();
    let diagnostic = trellis::rules::diagnostic(&error).unwrap();
    assert_eq!(diagnostic["code"], "invalid_rules");
    assert!(diagnostic["line"].as_u64().unwrap() > 0);
    config(root.path(), RULES);
    let wrong = IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        index_revision: pin.index_revision,
    };
    assert!(
        response
            .architecture_response("check", &json!({"expectedBasis":wrong}), &mut || Ok(()))
            .is_err()
    );
    let mut checks = 0;
    assert!(
        response
            .architecture_response("check", &json!({}), &mut || {
                checks += 1;
                anyhow::ensure!(checks < 3, "cancelled");
                Ok(())
            })
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
}

/// Snapshot admission decodes documents in parallel. A deadline at any poll,
/// including one inside a decode worker, fails the whole check with that error.
#[test]
fn deadline_at_every_poll_fails_the_check_atomically() {
    let (_state, root, store, _pin) = fixture();
    config(root.path(), RULES);
    let response = store.evidence_response().unwrap();
    let mut polls = 0usize;
    let complete = response
        .architecture_response("check", &json!({}), &mut || {
            polls += 1;
            Ok(())
        })
        .unwrap();
    assert!(polls > 3);
    for fail_at in 1..=polls {
        let mut seen = 0usize;
        let error = response
            .architecture_response("check", &json!({}), &mut || {
                seen += 1;
                anyhow::ensure!(seen < fail_at, "deadline_exceeded");
                Ok(())
            })
            .expect_err("a failed poll returns no partial result");
        assert_eq!(error.to_string(), "deadline_exceeded", "poll {fail_at}");
    }
    let again = response
        .architecture_response("check", &json!({}), &mut || Ok(()))
        .unwrap();
    assert_eq!(again, complete);
}

#[test]
fn portable_exception_matches_two_real_clones_without_replacing_native_identities() {
    let (_state_a, root_a, store_a, pin_a) = fixture();
    let (_state_b, root_b, store_b, pin_b) = fixture();
    let stores = [(&store_a, &root_a, pin_a), (&store_b, &root_b, pin_b)];
    let mut native_pairs = Vec::new();
    let portable = format!(
        "{RULES}\n[[exceptions]]\nrule='$layers'\ntarget={{language='rust',path='src/low.rs'}}\ndestination={{language='rust',path='src/high.rs'}}\nreason='Reviewed portable pair'\n"
    );
    for (store, root, pin) in stores {
        config(root.path(), RULES);
        let before = read(store, "check", json!({"expectedBasis":pin}));
        let edge = before["instances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["rule"] == "$layers" && row["target"]["path"] == "src/low.rs")
            .unwrap();
        native_pairs.push((edge["target"].clone(), edge["destination"].clone()));
        config(root.path(), &portable);
    }
    for endpoint in 0..2 {
        let a = if endpoint == 0 {
            &native_pairs[0].0
        } else {
            &native_pairs[0].1
        };
        let b = if endpoint == 0 {
            &native_pairs[1].0
        } else {
            &native_pairs[1].1
        };
        assert_ne!(
            a["sourceSetId"], b["sourceSetId"],
            "fixture must be distinct clones"
        );
        assert_ne!(a["sid"], b["sid"], "native module SIDs must really differ");
        assert_eq!(a["language"], b["language"]);
        assert_eq!(a["path"], b["path"]);
    }
    assert_eq!(
        fs::read(root_a.path().join(trellis::rules::RULES_PATH)).unwrap(),
        fs::read(root_b.path().join(trellis::rules::RULES_PATH)).unwrap()
    );
    let mut hashes = Vec::new();
    for ((store, _root, pin), (target, destination)) in stores.into_iter().zip(native_pairs) {
        let checked = read(store, "check", json!({"expectedBasis":pin}));
        hashes.push(checked["rulesFile"]["contentHash"].clone());
        let row = checked["instances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["rule"] == "$layers" && row["target"] == target)
            .unwrap();
        assert_eq!(row["destination"], destination);
        assert_eq!(row["status"], "deviates");
        assert_eq!(row["referenceCount"], 2);
        assert_eq!(row["excepted"], true);
        assert_eq!(row["exceptionReason"], "Reviewed portable pair");
        assert!(!row["evidence"].as_array().unwrap().is_empty());
        assert_eq!(checked["counts"]["deviates"], 0);
        let test = &checked["testOnly"][0];
        assert_eq!(test["target"], target);
        assert_eq!(test["destination"], destination);
        assert_eq!(test["excepted"], true);
        assert_eq!(test["referenceCount"], 1);
        assert_eq!(test["unresolved"], json!(["test_only"]));
        let explained = read(
            store,
            "explain",
            json!({"expectedBasis":pin,"rule":"$layers","target":target,"destination":destination}),
        );
        assert_eq!(explained["instances"], json!([row]));
        assert_eq!(explained["testOnly"], json!([test]));
        let mut wrong = target.clone();
        wrong["sid"] = json!("sid:v1:00000000000000000000000000000000");
        let response = store.evidence_response().unwrap();
        let error = response
            .architecture_response(
                "explain",
                &json!({"expectedBasis":pin,"rule":"$layers","target":wrong,"destination":destination}),
                &mut || Ok(()),
            )
            .unwrap_err();
        assert!(error.to_string().starts_with("not_found:"), "{error}");
        assert!(response.architecture_response(
            "explain",
            &json!({"expectedBasis":pin,"rule":"$layers","target":{"language":"rust","path":"src/low.rs"},"destination":{"language":"rust","path":"src/high.rs"}}),
            &mut || Ok(()),
        ).is_err(), "portable config does not make portable explain identities");
        response.finish(()).unwrap();
    }
    assert_eq!(hashes[0], hashes[1]);
}

/// A checkout of `files`, indexed and published.
fn published(files: &[(&str, &str)]) -> (tempfile::TempDir, tempfile::TempDir, Store, IndexPin) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, source) in files {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let baseline = store.index_baseline().unwrap();
    let leader = store.publication_for_tests().unwrap();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().into()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let pin = store
        .publish_native(&graph, &capture, &native, &leader, baseline, &cancel)
        .unwrap();
    drop(leader);
    (state, root, store, pin)
}

#[test]
fn architecture_incremental_external_scoped_projection() {
    use trellis::references;
    let (_state, root, store, pin) = published(&[
        (
            "Cargo.toml",
            "[package]\nname='fixture'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nwire={package='serde',version='1'}\n",
        ),
        (
            "src/lib.rs",
            "pub fn run() { wire::send(); std::mem::drop(1); }\n",
        ),
        (
            "package.json",
            r#"{"dependencies":{"shared":"1.0.0"},"devDependencies":{"shared":"1.0.0"}}"#,
        ),
        ("root.js", "import one from 'shared';\n"),
        ("other.js", "import one from 'shared';\n"),
        ("typed.ts", "import one from 'shared';\n"),
        (
            "tools/package.json",
            r#"{"optionalDependencies":{"shared":"2.0.0"}}"#,
        ),
        ("tools/nested.js", "import one from 'shared';\n"),
    ]);
    let snapshot = store.architecture_reference_snapshot_for_tests().unwrap();
    assert_eq!(snapshot.pin, pin);
    let base = references::resolve_all(&snapshot, &mut || Ok(())).unwrap();
    let frozen = base.select(&[], 5, &mut || Ok(())).unwrap().data;
    for path in [
        "Cargo.toml",
        "package.json",
        "tools/package.json",
        "root.js",
        "tools/nested.js",
    ] {
        fs::write(root.path().join(path), "not the captured input").unwrap();
    }
    fs::remove_file(root.path().join("other.js")).unwrap();
    for paths in [
        vec![],
        vec!["root.js".into()],
        vec!["tools/**".into()],
        vec!["root.js".into(), "tools/nested.js".into()],
        vec!["typed.ts".into()],
        vec!["missing/**".into()],
    ] {
        for prefix in [None, Some("tools")] {
            let selected = base
                .select_under(&paths, prefix, 5, &mut || Ok(()))
                .unwrap();
            let projected =
                references::project_under(&snapshot, &paths, prefix, 5, &mut || Ok(())).unwrap();
            assert_eq!(selected.data, projected.data, "{paths:?} {prefix:?}");
            if prefix.is_none() {
                assert_eq!(
                    selected.data,
                    references::project(&snapshot, &paths, 5, &mut || Ok(()))
                        .unwrap()
                        .data
                );
                assert_eq!(
                    selected.data,
                    base.select(&paths, 5, &mut || Ok(())).unwrap().data
                );
            }
            let files = BTreeSet::from([
                "root.js".into(),
                "typed.ts".into(),
                "tools/nested.js".into(),
            ]);
            assert_eq!(
                base.select_where(&paths, prefix, Some(&files), &[], 5, &mut || Ok(()))
                    .unwrap()
                    .data,
                references::project_where(
                    &snapshot,
                    &paths,
                    prefix,
                    Some(&files),
                    &[],
                    5,
                    &mut || Ok(())
                )
                .unwrap()
                .data,
            );
        }
    }
    let only = base
        .select(&["root.js".into()], 5, &mut || Ok(()))
        .unwrap()
        .data;
    assert!(
        only["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|package| package["evidence"].as_array().unwrap())
            .all(|evidence| evidence["path"] == "package.json")
    );
    let nested = base
        .select_under(&[], Some("tools"), 5, &mut || Ok(()))
        .unwrap()
        .data;
    assert!(
        nested["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|package| package["evidence"].as_array().unwrap())
            .all(|evidence| evidence["path"] == "tools/package.json")
    );
    assert_eq!(base.select(&[], 5, &mut || Ok(())).unwrap().data, frozen);
    assert_eq!(
        read(&store, "references", json!({"expectedBasis":pin}))["externalPackages"],
        frozen["externalPackages"],
        "public fallback still uses captured descriptors"
    );
}

#[test]
fn architecture_dependency_endpoints_include_current_alternatives_without_evidence_charges() {
    use crate::graph_fixtures::{Bind, Publisher, Spec, bind};
    use trellis::semantic_evidence as s;
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[
        (
            "src/lib.rs",
            "pub mod call; pub mod a; pub mod b; pub mod c; pub mod d;\n",
        ),
        (
            "src/call.rs",
            "pub struct Source;\npub fn caller() { crate::a::first(); let value: Missing; }\n",
        ),
        ("src/a.rs", "pub fn first() {}\n"),
        ("src/b.rs", "pub fn possible() {}\n"),
        ("src/c.rs", "pub trait Trait {}\n"),
        ("src/d.rs", "pub struct Candidate;\n"),
    ]);
    spec.bindings = vec![Bind {
        resolution: s::Resolution::Ambiguous,
        dispatch: s::Dispatch::Virtual,
        possible: vec!["possible".into()],
        ..bind("caller", 0, "first")
    }];
    spec.declaration_bindings = vec!["Source".into(), "Trait".into(), "Candidate".into()];
    publisher.publish(&spec);
    let use_site = publisher
        .native()
        .references
        .iter()
        .find(|reference| {
            reference.document.path == "src/call.rs" && reference.spelling == "Missing"
        })
        .unwrap()
        .clone();
    publisher.publish_with_semantic(&spec, |semantic, _| {
        let target = |name: &str| {
            semantic
                .symbols
                .iter()
                .find(|symbol| symbol.display_name.as_deref() == Some(name))
                .unwrap()
                .declarations[0]
                .clone()
        };
        let (source, relationship_target, reference_target) =
            (target("Source"), target("Trait"), target("Candidate"));
        let external = semantic.call_bindings[0]
            .candidates
            .iter()
            .find(|target| matches!(target, s::Target::External { .. }))
            .unwrap()
            .clone();
        let base = semantic
            .provenance
            .iter()
            .find(|proof| proof.id == "decl-Source")
            .unwrap()
            .clone();
        let mut proof = base.clone();
        proof.id = "relationship".into();
        proof.evidence_kind = s::EvidenceKind::TypeRelationship;
        semantic.provenance.push(proof);
        semantic.type_relationships.push(s::TypeRelationship {
            kind: s::RelationshipKind::Implements,
            source,
            target: relationship_target,
            provenance_id: "relationship".into(),
        });
        let mut proof = base;
        proof.id = "reference".into();
        proof.evidence_kind = s::EvidenceKind::SemanticReference;
        let revision_id = proof.revision_id.clone();
        semantic.provenance.push(proof);
        let mut candidates = vec![reference_target, external];
        candidates.sort_by_key(|target| trellis::native_ids::canonical(&json!(target)));
        semantic.references.push(s::Reference {
            id: use_site.id.clone(),
            owner_syntax_id: use_site.owner_syntax_id.clone(),
            ordinal: use_site.ordinal,
            document: use_site.document.clone(),
            revision_id,
            range: use_site.range.clone(),
            spelling: use_site.spelling.clone(),
            lookup_key: use_site.lookup_key.clone(),
            site: s::ReferenceSite::Use,
            roles: vec![s::Role::Read],
            resolution: s::Resolution::Ambiguous,
            declared_target: None,
            candidates,
            provenance_id: "reference".into(),
        });
        semantic.provenance.sort_by(|a, b| a.id.cmp(&b.id));
        for coverage in &mut semantic.coverage {
            coverage
                .supported_roles
                .extend([s::Role::Read, s::Role::Type]);
            coverage.supported_roles.sort();
            coverage.supported_roles.dedup();
            if coverage.document_path == "src/call.rs" {
                coverage
                    .observed_roles
                    .extend([s::Role::Read, s::Role::Type]);
                coverage.observed_roles.sort();
                coverage.observed_roles.dedup();
            }
        }
    });
    trellis::store::set_architecture_semantic_rows_for_tests(2);
    let snapshot = publisher
        .store
        .architecture_reference_snapshot_for_tests()
        .unwrap();
    let source = snapshot
        .documents
        .iter()
        .find(|doc| doc.key.path == "src/call.rs")
        .unwrap();
    let endpoints: BTreeSet<_> = source
        .semantic
        .endpoint_documents
        .iter()
        .map(|index| snapshot.documents[*index].key.path.as_str())
        .collect();
    assert_eq!(
        endpoints,
        BTreeSet::from([
            "src/call.rs",
            "src/a.rs",
            "src/b.rs",
            "src/c.rs",
            "src/d.rs"
        ])
    );
    assert!(source.semantic.projection.is_none());
    assert_eq!(
        source.semantic.anchors.len(),
        1,
        "dependency candidates do not become output anchors"
    );
    assert_eq!(source.semantic.relationships.len(), 1);
    assert!(
        source
            .semantic
            .anchors
            .iter()
            .all(|anchor| !anchor.metadata.contains_key("historicalTargets"))
    );
    publisher.store.architecture_semantic_for_tests(4).unwrap();
    let error = publisher
        .store
        .architecture_semantic_for_tests(3)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("bounded_work: architecture lookup dependencies"),
        "{error:#}"
    );
    trellis::store::set_architecture_semantic_rows_for_tests(1);
    let error = publisher
        .store
        .architecture_reference_snapshot_for_tests()
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "bounded_work: architecture semantic evidence"
    );
    trellis::store::set_architecture_semantic_rows_for_tests(2_000_000);
}

/// `value` without its index pin, its partial files and their limitation, its counts of what
/// is in no layer, and any row that names `path`. Rows are sorted: syntax IDs, which can order
/// them, differ between checkouts.
fn without(value: &Value, path: &str) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "indexGeneration"
                            | "indexRevision"
                            | "partialFiles"
                            | "limitations"
                            | "unassignedFileCount"
                            | "unassignedCount"
                    )
                })
                .map(|(key, value)| (key.clone(), without(value, path)))
                .collect(),
        ),
        Value::Array(rows) => {
            let mut rows: Vec<_> = rows
                .iter()
                .filter(|row| !row.to_string().contains(path))
                .map(|row| without(row, path))
                .collect();
            rows.sort_by_key(Value::to_string);
            Value::Array(rows)
        }
        other => other.clone(),
    }
}

/// Every value of `key` in `value`, in document order.
fn values_of<'a>(value: &'a Value, key: &str) -> Vec<&'a Value> {
    match value {
        Value::Object(map) => map
            .iter()
            .flat_map(|(name, value)| {
                if name == key {
                    vec![value]
                } else {
                    values_of(value, key)
                }
            })
            .collect(),
        Value::Array(rows) => rows.iter().flat_map(|row| values_of(row, key)).collect(),
        _ => Vec::new(),
    }
}

/// #1300: a checkout holding a Rust source past measurement's parse-work budget (#1282) reads
/// in bounded time. Its whole-checkout metrics, Architecture view, references and rules check
/// each finish within the hang backstop, on every run. The file is one whole-file
/// `evidence_budget` partial file and contributes nothing else, and every other result equals
/// the checkout's without it. Before, each read re-parsed the file's stored source for minutes.
#[test]
fn whole_checkout_reads_skip_a_rust_source_past_the_parse_work_budget() {
    const RULES: &str = r#"version = 1
[layers.high]
order = 1
paths = ["src/high.rs"]
[layers.low]
order = 2
paths = ["src/low.rs"]
[roles.square]
select = { name = "Square" }
[roles.shape]
select = { name = "Shape" }
[[rules]]
id = "squares-are-shapes"
for_each = "square"
require = [{ implements = "shape" }]
"#;
    let clean = [
        ("src/lib.rs", "pub mod high;\npub mod low;\n"),
        (
            "src/high.rs",
            "pub trait Shape {}\npub fn upper(n: u32) -> u32 { if n == 0 { 0 } else { upper(n - 1) } }\n",
        ),
        (
            "src/low.rs",
            "pub struct Square;\nimpl crate::high::Shape for Square {}\npub fn lower() { crate::high::upper(3); }\n",
        ),
        ("src/flow.py", "def go(x):\n    if x:\n        go(x - 1)\n"),
        (".trellis/rules.toml", RULES),
    ];
    let recovery = "} = ) + ] * ".repeat((64 << 10) / 12);
    let mut all = clean.to_vec();
    all.push(("src/recovery.rs", &recovery));
    let reads = |store: &Store, pin: IndexPin| {
        let timed = |read: &dyn Fn() -> Value| {
            let started = std::time::Instant::now();
            let value = read();
            // Unbounded, each read re-parses the file for minutes.
            assert!(started.elapsed() < trellis::testing::HANG_BACKSTOP);
            value
        };
        let options = trellis::metrics_read::MetricsOptions {
            pin: Some(pin),
            aggregate: "max".into(),
            ..Default::default()
        };
        let metrics = timed(&|| {
            let response = store.evidence_response().unwrap();
            let payload = response.metrics_at(&options, &mut || Ok(())).unwrap();
            serde_json::to_value(response.finish(payload).unwrap()).unwrap()
        });
        let view = timed(&|| {
            let response = store.evidence_response().unwrap();
            let value = response
                .architecture_view(&options, &mut || Ok(()))
                .unwrap();
            response.finish(value).unwrap()
        });
        let references = timed(&|| read(store, "references", json!({"expectedBasis":pin})));
        let check = timed(&|| read(store, "check", json!({"expectedBasis":pin})));
        [metrics, view, references, check]
    };
    // The source set ID names the checkout's root, and syntax and provenance IDs hash it in, so
    // a row of either checkout reads the same with them and the root path replaced.
    let checkout = |root: &tempfile::TempDir, read: &Value| {
        let id = read["files"][0]["sourceSetId"].as_str().unwrap().to_owned();
        let root = root.path().display().to_string();
        move |value: &Value| -> Value {
            let text = value
                .to_string()
                .replace(&id, "<set>")
                .replace(&root, "<root>");
            serde_json::from_str(&without_digests(&text)).unwrap()
        }
    };
    let (_clean_state, clean_root, clean_store, clean_pin) = published(&clean);
    let expected = reads(&clean_store, clean_pin);
    let expected_view = checkout(&clean_root, &expected[0]);
    let expected: Vec<Value> = expected.iter().map(&expected_view).collect();
    let (_state, root, store, pin) = published(&all);
    for read in &expected {
        assert!(
            values_of(read, "partialFiles")
                .iter()
                .all(|files| **files == json!([])),
            "{read}"
        );
    }
    for _ in 0..2 {
        let found = reads(&store, pin);
        let found_view = checkout(&root, &found[0]);
        let found: Vec<Value> = found.iter().map(&found_view).collect();
        for (found, expected) in found.iter().zip(&expected) {
            let (found_rest, expected_rest) = (
                without(found, "src/recovery.rs"),
                without(expected, "src/recovery.rs"),
            );
            assert!(
                found_rest == expected_rest,
                "{:?}",
                difference(&found_rest, &expected_rest, String::new())
            );
            // The file is a module in no layer, like any other.
            for key in ["unassignedFileCount", "unassignedCount"] {
                let counts = |read: &Value| -> Vec<u64> {
                    values_of(read, key)
                        .into_iter()
                        .map(|count| count.as_u64().unwrap())
                        .collect()
                };
                let expected: Vec<u64> = counts(expected).into_iter().map(|n| n + 1).collect();
                assert_eq!(counts(found), expected, "{key}");
            }
            let lists = values_of(found, "partialFiles");
            assert!(!lists.is_empty(), "{found}");
            for files in lists {
                let [file] = files.as_array().unwrap().as_slice() else {
                    panic!("{files}");
                };
                assert_eq!(
                    (
                        &file["path"],
                        &file["reason"],
                        &file["range"],
                        &file["subtrees"]
                    ),
                    (
                        &json!("src/recovery.rs"),
                        &json!("evidence_budget"),
                        &json!({"start": 0, "end": recovery.len()}),
                        &json!(0)
                    )
                );
            }
        }
    }
}

/// The first path where `a` and `b` differ, with both values there.
fn difference(a: &Value, b: &Value, path: String) -> Option<(String, Value, Value)> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for key in x.keys().chain(y.keys()) {
                let (u, v) = (x.get(key), y.get(key));
                if u != v {
                    return difference(
                        u.unwrap_or(&Value::Null),
                        v.unwrap_or(&Value::Null),
                        format!("{path}/{key}"),
                    );
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .find_map(|(i, (u, v))| difference(u, v, format!("{path}/{i}"))),
        _ => (a != b).then(|| (path, a.clone(), b.clone())),
    }
}

/// `text` with every run of 32 or more lowercase hex digits, a digest, replaced.
fn without_digests(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for c in text.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() || ('a'..='f').contains(&c) {
            run.push(c);
            continue;
        }
        out.push_str(if run.len() >= 32 { "<digest>" } else { &run });
        run.clear();
        out.push(c);
    }
    out.pop();
    out
}
