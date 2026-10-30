use super::{
    BatchLimits,
    staging::{
        Stage, StageClaimSink, lookup_assigned, remove_abandoned_semantic_stages, stage_dir_name,
    },
};
use crate::scip_raw::{
    CapturedClaim, CapturedScipFactV1, ClaimSink, ScipCaptureError, UnitDocument,
};
use rusqlite::params;

fn seeded(parent: &std::path::Path, incarnation: &str) -> Stage {
    let mut stage = Stage::create(parent, "request", incarnation).unwrap();
    let conn = stage.conn().unwrap();
    conn.execute_batch(
        "INSERT INTO sources VALUES(0,'java','a.java','h',1),(1,'java','b.java','h',1);
         INSERT INTO coverage VALUES(0,0,NULL),(1,NULL,'failed');
         INSERT INTO artifacts(ordinal,path,language,sha256,byte_length) VALUES(0,'p','java','s',1);",
    )
    .unwrap();
    stage
}

fn claim(bytes: &[u8], digest: &str, proof: &str) -> CapturedClaim {
    CapturedClaim {
        envelope: CapturedScipFactV1 {
            format_version: 1,
            record_kind: "occurrence".into(),
            relative_path: "a.java".into(),
            position_encoding: 1,
            raw: serde_json::json!({}),
        },
        canonical_bytes: bytes.to_vec(),
        fact_digest: digest.into(),
        captured_proof_id: proof.into(),
        language: "java".into(),
        artifact_hash: "s".into(),
    }
}

fn unit(path: &str) -> UnitDocument<'_> {
    UnitDocument {
        path,
        language: "java",
        wire_digest: "w",
    }
}

#[test]
fn stage_names_are_validated() {
    assert_eq!(
        stage_dir_name("r-1", "c-2").unwrap(),
        "semantic-stage-r-1-c-2"
    );
    for bad in ["", "a/b", "..", "a b", &"x".repeat(65)] {
        let error = stage_dir_name(bad, "c").unwrap_err();
        assert_eq!(error.to_string(), "index_failed: stage name");
    }
}

#[test]
fn stage_cleanup_removes_only_its_own_directory() {
    let parent = tempfile::tempdir().unwrap();
    let other = parent.path().join("semantic-stage-other-run");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("keep"), b"x").unwrap();
    let unrelated = parent.path().join("native-spool-1");
    std::fs::create_dir(&unrelated).unwrap();
    let stage = seeded(parent.path(), "mine");
    let dir = stage.dir().to_owned();
    assert!(dir.join("stage.db").exists());
    drop(stage);
    assert!(!dir.exists());
    assert!(other.join("keep").exists());
    assert!(unrelated.exists());
}

#[test]
fn startup_sweep_removes_abandoned_stages() {
    let parent = tempfile::tempdir().unwrap();
    for name in ["semantic-stage-a-1", "semantic-stage-b-2"] {
        let dir = parent.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("stage.db"), b"x").unwrap();
    }
    std::fs::write(parent.path().join("semantic-stage-file"), b"x").unwrap();
    std::fs::write(parent.path().join("index.db"), b"x").unwrap();
    assert_eq!(remove_abandoned_semantic_stages(parent.path()), 3);
    let left: Vec<_> = std::fs::read_dir(parent.path())
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].file_name(), "index.db");
    assert_eq!(
        remove_abandoned_semantic_stages(&parent.path().join("missing")),
        0
    );
}

#[test]
fn stage_connection_reopens_after_release() {
    let parent = tempfile::tempdir().unwrap();
    let mut stage = seeded(parent.path(), "reopen");
    stage.release_connection();
    assert!(!stage.has_connection());
    let conn = stage.conn().unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sources", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);
    let cache: i64 = conn
        .pragma_query_value(None, "cache_size", |r| r.get(0))
        .unwrap();
    assert_eq!(cache, -(super::limits::STAGE_CACHE_KIB as i64));
    let temp_store: i64 = conn
        .pragma_query_value(None, "temp_store", |r| r.get(0))
        .unwrap();
    assert_eq!(temp_store, 1, "temp_store=FILE");
}

#[test]
fn staged_cursor_queries_use_indexes_without_temp_sort() {
    let parent = tempfile::tempdir().unwrap();
    let mut stage = seeded(parent.path(), "plan");
    let conn = stage.conn().unwrap();
    for sql in [
        "SELECT canonical_bytes FROM claims ORDER BY source_ord, canonical_bytes",
        "SELECT canonical_bytes FROM claims WHERE source_ord=1 ORDER BY canonical_bytes",
        "SELECT claim_id FROM claims WHERE proof_id='p'",
        "SELECT ord FROM sources ORDER BY ord",
        "SELECT source_ord FROM coverage WHERE artifact=0 ORDER BY source_ord",
        "SELECT s.ord FROM sources s JOIN coverage c ON c.source_ord=s.ord WHERE s.language='java' AND s.path='a' AND c.artifact=0",
    ] {
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            plan.iter().all(|step| !step.contains("TEMP B-TREE")),
            "{sql}: {plan:?}"
        );
    }
}

#[test]
fn claim_sink_dedups_and_types_collisions_and_duplicate_documents() {
    let parent = tempfile::tempdir().unwrap();
    let mut stage = seeded(parent.path(), "sink");
    let budget = stage.budget(&BatchLimits::PRODUCTION);
    let conn = stage.conn().unwrap();
    let mut sink = StageClaimSink::new(conn, &budget, 0, "java");
    sink.begin_document(&unit("a.java")).unwrap();
    sink.claim(claim(b"one", "d1", "p1")).unwrap();
    sink.claim(claim(b"one", "d1", "p1")).unwrap();
    assert_eq!(
        sink.claim(claim(b"two", "d1", "p1")).unwrap_err(),
        ScipCaptureError::DigestCollision
    );
    sink.end_document().unwrap();
    assert_eq!(
        sink.begin_document(&unit("a.java")).unwrap_err(),
        ScipCaptureError::InvalidArtifact("duplicate document path".into())
    );
    let totals = sink.finish().unwrap();
    assert_eq!((totals.documents, totals.claims), (1, 1));
    let stored: i64 = conn
        .query_row("SELECT claims FROM artifacts WHERE ordinal=0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(stored, 1);
}

#[test]
fn claim_sink_types_a_proof_id_clash_as_non_unique_proof() {
    let parent = tempfile::tempdir().unwrap();
    let mut stage = seeded(parent.path(), "proof-clash");
    let budget = stage.budget(&BatchLimits::PRODUCTION);
    let conn = stage.conn().unwrap();
    let mut sink = StageClaimSink::new(conn, &budget, 0, "java");
    sink.begin_document(&unit("a.java")).unwrap();
    sink.claim(claim(b"one", "d1", "p1")).unwrap();
    assert_eq!(
        sink.claim(claim(b"other", "d2", "p1")).unwrap_err(),
        ScipCaptureError::NonUniqueProof,
        "a different claim under an existing proof ID is not a digest collision"
    );
}

#[test]
fn claim_sink_types_a_full_disk_insert_as_storage_full() {
    let parent = tempfile::tempdir().unwrap();
    let mut stage = seeded(parent.path(), "full-disk");
    let budget = stage.budget(&BatchLimits::PRODUCTION);
    let conn = stage.conn().unwrap();
    let pages: i64 = conn
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    conn.query_row(&format!("PRAGMA max_page_count={pages}"), [], |_| Ok(()))
        .unwrap();
    let mut sink = StageClaimSink::new(conn, &budget, 0, "java");
    sink.begin_document(&unit("a.java")).unwrap();
    let big = vec![7u8; 1 << 20];
    let error = sink.claim(claim(&big, "d1", "p1")).unwrap_err();
    assert_eq!(
        error,
        ScipCaptureError::InvalidArtifact("stage storage".into())
    );
    let fault = crate::store::typed_storage_full(sink.take_fault().unwrap());
    assert!(fault.is::<crate::store::StorageFull>(), "{fault:#}");
    assert!(
        format!("{fault:#}").starts_with("storage_full: database or disk is full"),
        "{fault:#}"
    );
    let other = crate::store::typed_storage_full(anyhow::anyhow!("stage write"));
    assert!(!other.is::<crate::store::StorageFull>());
}

#[test]
fn only_documents_assigned_to_the_artifact_resolve() {
    let parent = tempfile::tempdir().unwrap();
    let mut stage = seeded(parent.path(), "assign");
    let conn = stage.conn().unwrap();
    assert!(
        lookup_assigned(conn, 0, "java", "a.java")
            .unwrap()
            .is_some()
    );
    assert!(
        lookup_assigned(conn, 1, "java", "a.java")
            .unwrap()
            .is_none()
    );
    assert!(
        lookup_assigned(conn, 0, "java", "b.java")
            .unwrap()
            .is_none(),
        "excluded"
    );
    assert!(
        lookup_assigned(conn, 0, "java", "missing.java")
            .unwrap()
            .is_none()
    );
    let _ = params![0];
}

/// Duplicate coverage rows are rejected by the row-order check, so a failing coverage insert is
/// a storage fault. It must surface as itself, not as `invalid_request: duplicate coverage row`.
#[cfg(feature = "test-support")]
#[test]
fn coverage_insert_storage_fault_propagates_typed() {
    use super::testing;
    use std::sync::{Arc, atomic::AtomicBool};
    const WIRE: &str = "122e0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a090a0241232811320166";
    let workspace = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(workspace.path()).unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/A.js"), "function f(){g();}\n").unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = crate::indexer::IndexOptions::new(root);
    let capture = crate::capture::Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let artifact = hex::decode(WIRE).unwrap();
    let (descriptor, pages) =
        testing::single_artifact_batch(&capture, "scip-typescript", "javascript", &artifact)
            .unwrap();
    let pages: Vec<&[u8]> = pages.iter().map(Vec::as_slice).collect();
    let admit = || {
        testing::admit_in_memory_from_capture(&capture, &descriptor, &pages, &[&artifact], &cancel)
    };
    assert_eq!(admit().unwrap().len(), 1, "the batch is admissible");
    testing::before_coverage_pass(|conn| {
        conn.execute_batch(
            "CREATE TRIGGER disk_full BEFORE INSERT ON coverage \
             BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END",
        )
        .unwrap();
    });
    match admit().unwrap_err() {
        ScipCaptureError::InvalidArtifact(cause) => {
            assert!(cause.contains("database or disk is full"), "{cause}");
        }
        other => panic!("the storage fault was reported as {other:?}"),
    }
}

#[test]
fn admitted_claims_record_the_symbol_presence_discovery_reads() {
    use super::staging::{PRESENCE_DECLARED, PRESENCE_INFORMATION, claim_presence};
    let with = |kind: &str, raw: serde_json::Value| {
        let mut row = claim(b"{}", "d", "p");
        row.envelope.record_kind = kind.into();
        row.envelope.raw = raw;
        row
    };
    let cases = [
        (
            with("symbolInformation", serde_json::json!({"symbol": "pkg A#"})),
            (Some("pkg A#"), PRESENCE_INFORMATION),
        ),
        (with("symbolInformation", serde_json::json!({})), (None, 0)),
        (
            with(
                "occurrence",
                serde_json::json!({"symbol": "pkg A#", "symbolRoles": 1}),
            ),
            (Some("pkg A#"), PRESENCE_DECLARED),
        ),
        (
            with(
                "occurrence",
                serde_json::json!({"symbol": "pkg A#", "symbolRoles": 9}),
            ),
            (Some("pkg A#"), PRESENCE_DECLARED),
        ),
        (
            with(
                "occurrence",
                serde_json::json!({"symbol": "pkg A#", "symbolRoles": 8}),
            ),
            (None, 0),
        ),
        (
            with(
                "occurrence",
                serde_json::json!({"symbol": "", "symbolRoles": 1}),
            ),
            (None, 0),
        ),
        (
            with(
                "occurrence",
                serde_json::json!({"symbol": "pkg A#", "symbolRoles": "1"}),
            ),
            (None, 0),
        ),
        (
            with("relationship", serde_json::json!({"symbol": "pkg A#"})),
            (None, 0),
        ),
    ];
    for (row, expected) in &cases {
        assert_eq!(claim_presence(row), *expected, "{:?}", row.envelope.raw);
    }
    // The sink stores it with the claim.
    let parent = tempfile::tempdir().unwrap();
    let mut stage = seeded(parent.path(), "presence");
    let budget = stage.budget(&BatchLimits::PRODUCTION);
    let conn = stage.conn().unwrap();
    let mut sink = StageClaimSink::new(conn, &budget, 0, "java");
    sink.begin_document(&unit("a.java")).unwrap();
    let mut first = cases[2].0.clone();
    first.fact_digest = "d1".into();
    first.captured_proof_id = "p1".into();
    let mut second = cases[4].0.clone();
    second.fact_digest = "d2".into();
    second.captured_proof_id = "p2".into();
    sink.claim(first).unwrap();
    sink.claim(second).unwrap();
    sink.end_document().unwrap();
    let stored: Vec<(Option<String>, i64)> = conn
        .prepare("SELECT symbol, presence FROM claims ORDER BY claim_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        stored,
        [(Some("pkg A#".to_owned()), PRESENCE_DECLARED), (None, 0)]
    );
}
