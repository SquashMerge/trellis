use super::*;
use std::sync::{Arc, atomic::AtomicBool};

fn pin(revision: u64) -> IndexPin {
    IndexPin {
        index_generation: uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
        index_revision: revision,
    }
}
fn fixture() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(CACHE_SCHEMA_V42).unwrap();
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    db.execute(
        "INSERT INTO semantic_producers VALUES('producer','1',?1,'semantic','utf8','executable')",
        ["a".repeat(64)],
    )
    .unwrap();
    db.execute(
        "INSERT INTO semantic_producer_languages VALUES('producer','1','javascript',0)",
        [],
    )
    .unwrap();
    header(&db, 1, "a");
    header(&db, 2, "a");
    header(&db, 3, "a");
    for revision in [1, 2, 3] {
        for (ordinal, path, vid) in [(0, "a.js", 1), (1, "b.js", 2), (2, "c.js", 3)] {
            let vid = if revision == 2 && path == "c.js" {
                4
            } else {
                vid
            };
            db.execute("INSERT INTO revision_documents VALUES(?1,'javascript',?2,?3,1,NULL,NULL,1,1,'complete',NULL,?4,1)",params![semantic_pin_key(pin(revision)),path,vid,ordinal]).unwrap();
        }
    }
    let key = semantic_pin_key(pin(1));
    db.execute(
        "INSERT INTO semantic_revision_producers(revision_id,id,version,context_mode,context_trust,context_input_hash) VALUES(?1,'producer','1','fullCaptured',NULL,NULL)",
        [&key],
    )
    .unwrap();
    db.execute(
        "INSERT INTO semantic_requested_producers VALUES(?1,'producer','1',?2)",
        params![key, "a".repeat(64)],
    )
    .unwrap();
    db.execute(
        "INSERT INTO semantic_requested_producer_languages VALUES(?1,'producer','javascript',0)",
        [&key],
    )
    .unwrap();
    for (tuple, path) in [(1, "a.js"), (2, "b.js"), (3, "c.js")] {
        db.execute("INSERT INTO semantic_payloads(payload_id,producer_id,producer_version,language,path,version_id) VALUES(?1,'producer','1','javascript',?2,'v')",params![tuple,path]).unwrap();
        db.execute("INSERT INTO semantic_coverage VALUES(?1,?2,'producer','1','javascript',?3,1,1,'complete',NULL,?1)",params![tuple,key,path]).unwrap();
    }
    // A retained raw selection: bounded S will catch changed documents up.
    db.execute(
        "INSERT INTO semantic_revision_raw_selections VALUES(?1,'producer',1,?1)",
        [&key],
    )
    .unwrap();
    db
}
fn header(db: &Connection, revision: u64, context: &str) {
    db.execute("INSERT INTO native_revisions VALUES(?1,'source-set',?2,?2,?2,?3,'[]','[]','{}','[]','[]',NULL,'[]',0,'{}','[]',?4)",params![semantic_pin_key(pin(revision)),context.repeat(64),format!("native-{revision}"),revision as i64]).unwrap();
}
fn projected(db: &Connection, revision: u64, path: &str) -> SemanticProjection {
    semantic_projection_for(db, pin(revision), "producer", "javascript", path)
        .unwrap()
        .unwrap()
}

/// Owner ruling #620 (A6): with no retained raw input nothing will catch a changed document
/// up, so it and its dependents need re-import; unchanged evidence stays older-basis.
#[test]
fn changed_documents_without_retained_input_need_reimport() {
    let db = fixture();
    db.execute("DELETE FROM semantic_revision_raw_selections", [])
        .unwrap();
    db.execute(
        "INSERT INTO semantic_projection_dependencies VALUES(2,'javascript','c.js','v')",
        [],
    )
    .unwrap();
    write_pending(
        &db,
        pin(1),
        pin(2),
        "root",
        &[],
        &[],
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert!(pending_target(&db).unwrap().is_none());
    assert_eq!(
        projected(&db, 2, "a.js").semantic_status,
        SemanticStatus::OlderBasis
    );
    for path in ["b.js", "c.js"] {
        assert_eq!(
            projected(&db, 2, path).semantic_status,
            SemanticStatus::NeedsReimport
        );
    }
    assert_eq!(
        endpoint_boundary(&db, pin(2), "producer", "javascript", "c.js", pin(1)).unwrap(),
        Some(crate::graph_query::BoundaryReason::NeedsReimport)
    );
}

#[test]
fn pending_uses_recorded_endpoints_not_transitive_call_paths() {
    let db = fixture();
    // A -> B and B -> C are separate evidence, not consumption derivation.
    db.execute(
        "INSERT INTO semantic_projection_dependencies VALUES(1,'javascript','b.js','v')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO semantic_projection_dependencies VALUES(2,'javascript','c.js','v')",
        [],
    )
    .unwrap();
    write_pending(
        &db,
        pin(1),
        pin(2),
        "root",
        &[],
        &[],
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(
        projected(&db, 2, "a.js").semantic_status,
        SemanticStatus::OlderBasis
    );
    assert_eq!(
        projected(&db, 2, "b.js").semantic_status,
        SemanticStatus::CatchingUp
    );
    assert_eq!(
        projected(&db, 2, "c.js").semantic_status,
        SemanticStatus::CatchingUp
    );
    assert!(
        endpoint_boundary(&db, pin(2), "producer", "javascript", "b.js", pin(1))
            .unwrap()
            .is_some()
    );
    let a = projected(&db, 2, "a.js");
    assert_eq!(a.semantic_basis_pin, pin(1));
    assert_eq!(a.basis_native_revision_id, "native-1");
    assert_eq!(
        a.effective_freshness(crate::semantic_evidence::Freshness::Fresh),
        crate::semantic_evidence::Freshness::PossiblyStale
    );
}

#[test]
fn restored_native_recomputes_flat_basis_without_mutating_retained_n() {
    let db = fixture();
    db.execute(
        "INSERT INTO semantic_projection_dependencies VALUES(2,'javascript','c.js','v')",
        [],
    )
    .unwrap();
    write_pending(
        &db,
        pin(1),
        pin(2),
        "root",
        &[],
        &[],
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    write_pending(
        &db,
        pin(2),
        pin(3),
        "root",
        &[],
        &[],
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(
        projected(&db, 3, "b.js").semantic_status,
        SemanticStatus::OlderBasis
    );
    assert_eq!(projected(&db, 3, "b.js").semantic_basis_pin, pin(1));
    assert_eq!(
        projected(&db, 2, "b.js").semantic_status,
        SemanticStatus::CatchingUp
    );
}

#[test]
fn missing_validated_basis_tuple_is_corruption_not_pending() {
    let db = fixture();
    db.execute("DELETE FROM semantic_coverage WHERE tuple_id=2", [])
        .unwrap();
    let error = write_pending(
        &db,
        pin(1),
        pin(2),
        "root",
        &[],
        &[],
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap_err();
    assert!(error.is::<SelectedIntegrity>(), "{error:#}");
}

#[test]
fn missing_projection_disposition_is_corruption() {
    let db = fixture();
    write_pending(
        &db,
        pin(1),
        pin(2),
        "root",
        &[],
        &[],
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    db.execute(
        "DELETE FROM semantic_projection_documents WHERE revision_id=?1 AND path='a.js'",
        [semantic_pin_key(pin(2))],
    )
    .unwrap();
    let error = semantic_projection_for(&db, pin(2), "producer", "javascript", "a.js").unwrap_err();
    assert!(error.is::<SelectedIntegrity>(), "{error:#}");
}

#[test]
fn context_change_marks_only_tuples_recording_context_pending() {
    let db = fixture();
    db.execute(
        "UPDATE native_revisions SET config_hash=?1 WHERE id=?2",
        params!["b".repeat(64), semantic_pin_key(pin(2))],
    )
    .unwrap();
    // Captured basis already differs from P. That historical mismatch is not itself pending.
    db.execute("INSERT INTO semantic_basis VALUES(1,1,'producer','1',?1,?1,'javascript','source-set','captured',?1,?1,?1,?1)",["f".repeat(64)]).unwrap();
    db.execute("INSERT INTO semantic_projection_contexts VALUES(1)", [])
        .unwrap();
    write_pending(
        &db,
        pin(1),
        pin(2),
        "root",
        &[],
        &[],
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(
        projected(&db, 2, "a.js").semantic_status,
        SemanticStatus::CatchingUp
    );
    assert_eq!(
        projected(&db, 2, "b.js").semantic_status,
        SemanticStatus::OlderBasis
    );
}

fn controlled_fixture() -> (Connection, Vec<serde_json::Value>) {
    use crate::producer::{ProducerName, TrustProfile, profiles, worktree};
    let db = fixture();
    let profile = profiles::row(
        profiles::Target::Real(ProducerName::ScipPython),
        TrustProfile::Restricted,
    )
    .unwrap();
    let producer = profile.target.pin().unwrap().producer();
    db.execute("DELETE FROM semantic_producers", []).unwrap();
    db.execute("DELETE FROM semantic_producer_languages", [])
        .unwrap();
    db.execute(
        "INSERT INTO semantic_producers VALUES(?1,?2,?3,'semantic','utf8','executable')",
        params![producer.id, producer.version, producer.executable_hash],
    )
    .unwrap();
    db.execute(
        "INSERT INTO semantic_producer_languages VALUES(?1,?2,'python',0)",
        params![producer.id, producer.version],
    )
    .unwrap();
    db.execute(
        "UPDATE revision_documents SET language='python',path=replace(path,'.js','.py')",
        [],
    )
    .unwrap();
    for table in [
        "semantic_revision_producers",
        "semantic_requested_producers",
    ] {
        db.execute(
            &format!("UPDATE {table} SET id=?1,version=?2"),
            params![producer.id, producer.version],
        )
        .unwrap();
    }
    db.execute(
        "UPDATE semantic_requested_producer_languages SET id=?1,language='python'",
        [&producer.id],
    )
    .unwrap();
    db.execute(
        "UPDATE semantic_revision_raw_selections SET producer_id=?1",
        [&producer.id],
    )
    .unwrap();
    db.execute("UPDATE semantic_coverage SET producer_id=?1,producer_version=?2,language='python',path=replace(path,'.js','.py')",
        params![producer.id, producer.version]).unwrap();
    let config = |requirements: &str, pyproject: &str| {
        vec![
            serde_json::json!({"type":"present","path":"requirements.txt","bytes":requirements,"byteLength":1}),
            serde_json::json!({"type":"present","path":"pyproject.toml","bytes":pyproject,"byteLength":1}),
        ]
    };
    let captured = config("30", "30");
    let published = config("31", "30");
    let current = config("31", "32");
    header(&db, 0, "a");
    header(&db, 4, "a");
    db.execute("INSERT INTO revision_documents SELECT ?1,language,path,vid,gid,cid,capture_stat,coverage_requested,coverage_selected,coverage_state,coverage_diagnostic,ordinal,class_complete FROM revision_documents WHERE revision_id=?2",
        params![semantic_pin_key(pin(4)), semantic_pin_key(pin(3))]).unwrap();
    for (revision, entries) in [
        (0, &captured),
        (1, &published),
        (2, &current),
        (3, &current),
        (4, &current),
    ] {
        db.execute(
            "UPDATE native_revisions SET retained_config=?1 WHERE id=?2",
            params![
                serde_json::to_string(entries).unwrap(),
                semantic_pin_key(pin(revision))
            ],
        )
        .unwrap();
    }
    db.execute(
        "UPDATE native_revisions SET config_hash=?1 WHERE id IN (?2,?3)",
        params![
            "b".repeat(64),
            semantic_pin_key(pin(2)),
            semantic_pin_key(pin(3))
        ],
    )
    .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let captured_hash = worktree::context_hash(
        &profiles::rules(&profile),
        &captured,
        &[],
        &|_| Ok(false),
        &cancel,
    )
    .unwrap();
    let input_hash = worktree::context_hash(
        &profiles::rules(&profile),
        &published,
        &[],
        &|path| {
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?1 AND path=?2)",
                params![semantic_pin_key(pin(1)), path],
                |row| row.get(0),
            )?)
        },
        &cancel,
    )
    .unwrap();
    assert_ne!(captured_hash, input_hash);
    db.execute("UPDATE semantic_revision_producers SET context_mode='controlledProfile',context_trust='restricted',context_input_hash=?1",
        [&input_hash]).unwrap();
    // Arrange captured C and validated destination P with different supplied inputs.
    db.execute("INSERT INTO semantic_basis VALUES(1,1,?1,?2,?3,?3,'python','source-set','native-0',?3,?3,?3,?3)",
        params![producer.id, producer.version, "f".repeat(64)]).unwrap();
    db.execute("INSERT INTO semantic_projection_contexts VALUES(1)", [])
        .unwrap();
    (db, current)
}

#[test]
fn controlled_context_compares_validated_p_not_capture_across_native_moves() {
    let (db, config) = controlled_fixture();
    let cancel = Arc::new(AtomicBool::new(false));
    write_pending(&db, pin(1), pin(2), "root", &config, &[], &cancel).unwrap();
    write_pending(&db, pin(2), pin(3), "root", &config, &[], &cancel).unwrap();
    let projection = |revision| {
        semantic_projection_for(&db, pin(revision), "scip-python", "python", "a.py")
            .unwrap()
            .unwrap()
    };
    for revision in [2, 3] {
        let selected = projection(revision);
        assert_eq!(selected.semantic_status, SemanticStatus::OlderBasis);
        assert_eq!(selected.semantic_basis_pin, pin(1));
        assert_eq!(selected.basis_native_revision_id, "native-1");
    }
    let mut supplied_change = config;
    supplied_change[0]["bytes"] = serde_json::json!("33");
    write_pending(&db, pin(3), pin(4), "root", &supplied_change, &[], &cancel).unwrap();
    assert_eq!(projection(4).semantic_status, SemanticStatus::CatchingUp);
    assert_eq!(projection(4).semantic_basis_pin, pin(1));
    assert_eq!(projection(2).semantic_status, SemanticStatus::OlderBasis);
    assert_eq!(projection(3).semantic_status, SemanticStatus::OlderBasis);
}

#[test]
fn missing_controlled_context_metadata_or_association_is_corruption() {
    for missing_hash in [true, false] {
        let (db, config) = controlled_fixture();
        if missing_hash {
            db.pragma_update(None, "ignore_check_constraints", "ON")
                .unwrap();
            db.execute(
                "UPDATE semantic_revision_producers SET context_input_hash=NULL",
                [],
            )
            .unwrap();
            db.pragma_update(None, "ignore_check_constraints", "OFF")
                .unwrap();
        } else {
            db.execute(
                "DELETE FROM semantic_projection_contexts WHERE payload_id=1",
                [],
            )
            .unwrap();
        }
        let error = if missing_hash {
            context::validate(&db, &semantic_pin_key(pin(1))).unwrap_err()
        } else {
            write_pending(
                &db,
                pin(1),
                pin(2),
                "root",
                &config,
                &[],
                &Arc::new(AtomicBool::new(false)),
            )
            .unwrap_err()
        };
        assert!(
            error.is::<SelectedIntegrity>(),
            "missing_hash={missing_hash}: {error:#}"
        );
    }
}

#[test]
fn orphan_requested_language_rejects_pending_copy_without_successor_state() {
    let mut db = fixture();
    let prior = semantic_pin_key(pin(1));
    let successor = semantic_pin_key(pin(2));
    db.execute(
        "DELETE FROM semantic_requested_producers WHERE revision_id=?1 AND id='producer'",
        [&prior],
    )
    .unwrap();
    let unchanged = |db: &Connection| {
        db.query_row(
            "SELECT
                (SELECT count(*) FROM semantic_revision_producers WHERE revision_id=?1),
                (SELECT count(*) FROM semantic_requested_producers WHERE revision_id=?1),
                (SELECT count(*) FROM semantic_requested_producer_languages WHERE revision_id=?1),
                (SELECT count(*) FROM semantic_projection_origins WHERE revision_id=?1),
                (SELECT count(*) FROM semantic_projection_documents WHERE revision_id=?1),
                (SELECT count(*) FROM semantic_pending_selections WHERE revision_id=?1),
                (SELECT count(*) FROM semantic_pending_target)",
            [&successor],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .unwrap()
    };
    let before = unchanged(&db);
    let transaction = db.transaction().unwrap();
    let error = write_pending(
        &transaction,
        pin(1),
        pin(2),
        "root",
        &[],
        &[],
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap_err();
    assert!(error.is::<SelectedIntegrity>(), "{error:#}");
    transaction.rollback().unwrap();
    assert_eq!(unchanged(&db), before);
    let orphan_retained: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM semantic_requested_producer_languages l LEFT JOIN semantic_requested_producers p ON p.revision_id=l.revision_id AND p.id=l.id WHERE l.revision_id=?1 AND l.id='producer' AND p.id IS NULL)",
        [&prior], |row| row.get(0),
    ).unwrap();
    assert!(orphan_retained);
}

/// A native document version and one declaration in it, for dependency terms to read.
fn version(db: &Connection, vid: i64, content_hash: &str, kind: Option<&str>) {
    db.execute(
        "INSERT INTO document_versions(vid,id,language,path,content_hash,extraction_context,producer_id,producer_version,byte_length,source_bytes,native_witness) VALUES(?1,?2,'javascript','c.js',?3,?4,'p','1',0,x'',?3)",
        params![
            vid,
            format!("v{vid}"),
            format!("{content_hash:0>64}"),
            format!("{:0>64}", format!("context-{vid}"))
        ],
    )
    .unwrap();
    if let Some(kind) = kind {
        db.execute(
            "INSERT INTO native_version_declarations(vid,syntax_id,kind,role,name,lookup_key,key_signature_present,key_ordinal,start_byte,end_byte,name_start,name_end) VALUES(?1,?2,?3,'production','c','c',0,0,0,10,1,2)",
            params![vid, vec![7u8; 16], kind],
        )
        .unwrap();
    }
}

/// `b.js` records `c.js` with its term at revision 1 and the one syntax ID it targets.
fn record_term(db: &Connection, call: bool) {
    let term = semantic_payload::dependency_term(
        db,
        &semantic_pin_key(pin(1)),
        &semantic_pin_key(pin(1)),
        "javascript",
        "c.js",
        &[(vec![7u8; 16], call)],
    )
    .unwrap();
    db.execute(
        "INSERT INTO semantic_projection_dependencies VALUES(2,'javascript','c.js',?1)",
        [term],
    )
    .unwrap();
    db.execute(
        "INSERT INTO semantic_projection_dependency_targets VALUES(2,'javascript','c.js',?1,?2)",
        params![vec![7u8; 16], call],
    )
    .unwrap();
}

fn pending_within(db: &Connection, bound: usize) {
    write_pending_within(
        db,
        pin(1),
        pin(2),
        "root",
        &[],
        &[],
        &Arc::new(AtomicBool::new(false)),
        bound,
    )
    .unwrap();
}

/// Decision 0021 D4 (owner ruling 2026-11-09): a changed dependency whose term is unchanged
/// at N leaves its dependent older-basis evidence; the changed document itself is pending.
#[test]
fn dependency_with_an_equal_term_does_not_make_a_dependent_pending() {
    let db = fixture();
    version(&db, 3, "h1", Some("function"));
    version(&db, 4, "h2", Some("function"));
    record_term(&db, true);
    pending_within(&db, 10);
    assert_eq!(
        projected(&db, 2, "b.js").semantic_status,
        SemanticStatus::OlderBasis
    );
    assert_eq!(
        projected(&db, 2, "c.js").semantic_status,
        SemanticStatus::CatchingUp
    );
}

#[test]
fn dependency_whose_targeted_declaration_changed_makes_a_dependent_pending() {
    for kind in [None, Some("method")] {
        let db = fixture();
        version(&db, 3, "h1", Some("function"));
        version(&db, 4, "h2", kind);
        record_term(&db, false);
        pending_within(&db, 10);
        assert_eq!(
            projected(&db, 2, "b.js").semantic_status,
            SemanticStatus::CatchingUp,
            "{kind:?}"
        );
    }
}

/// A call bound to a method reads whether the dependency's content changed since capture;
/// a method called any other way, or a free function, does not.
#[test]
fn only_a_call_bound_to_a_method_reads_the_dependency_content() {
    for (kind, call, pending) in [
        ("method", true, true),
        ("method", false, false),
        ("function", true, false),
    ] {
        let db = fixture();
        version(&db, 3, "h1", Some(kind));
        version(&db, 4, "h2", Some(kind));
        record_term(&db, call);
        pending_within(&db, 10);
        let expected = if pending {
            SemanticStatus::CatchingUp
        } else {
            SemanticStatus::OlderBasis
        };
        assert_eq!(
            projected(&db, 2, "b.js").semantic_status,
            expected,
            "{kind} {call}"
        );
    }
}

/// Equal bytes under another extraction context are a new document version but the same
/// content: a method call keeps its dispatch, so the term is equal.
#[test]
fn a_config_only_change_keeps_the_term_of_a_method_call() {
    let db = fixture();
    version(&db, 3, "h1", Some("method"));
    version(&db, 4, "h1", Some("method"));
    record_term(&db, true);
    pending_within(&db, 10);
    assert_eq!(
        projected(&db, 2, "b.js").semantic_status,
        SemanticStatus::OlderBasis
    );
}

/// A term that is NULL (the payload's own document, or no reuse key) is pending on any change;
/// so is any check that goes over its lookup bound.
#[test]
fn null_term_and_over_bound_checks_are_pending() {
    let db = fixture();
    version(&db, 3, "h1", Some("function"));
    version(&db, 4, "h2", Some("function"));
    record_term(&db, false);
    db.execute("UPDATE semantic_projection_dependencies SET term=NULL", [])
        .unwrap();
    pending_within(&db, 10);
    assert_eq!(
        projected(&db, 2, "b.js").semantic_status,
        SemanticStatus::CatchingUp
    );
    let db = fixture();
    version(&db, 3, "h1", Some("function"));
    version(&db, 4, "h2", Some("function"));
    record_term(&db, false);
    pending_within(&db, 1);
    assert_eq!(
        projected(&db, 2, "b.js").semantic_status,
        SemanticStatus::CatchingUp,
        "one target and its row do not fit a bound of one"
    );
}

#[test]
fn deleted_dependency_makes_a_dependent_pending() {
    let db = fixture();
    version(&db, 3, "h1", Some("function"));
    record_term(&db, false);
    db.execute(
        "DELETE FROM revision_documents WHERE revision_id=?1 AND path='c.js'",
        [semantic_pin_key(pin(2))],
    )
    .unwrap();
    pending_within(&db, 10);
    assert_eq!(
        projected(&db, 2, "b.js").semantic_status,
        SemanticStatus::CatchingUp
    );
}
