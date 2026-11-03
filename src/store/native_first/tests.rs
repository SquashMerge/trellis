use super::*;

fn pin(revision:u64)->IndexPin { IndexPin {index_generation:uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),index_revision:revision} }
fn fixture()->Connection {
    let db=Connection::open_in_memory().unwrap();
    db.execute_batch(CACHE_SCHEMA_V38).unwrap();
    db.pragma_update(None,"foreign_keys","OFF").unwrap();
    db.execute("INSERT INTO semantic_producers VALUES('producer','1',?1,'semantic','utf8','executable')",["a".repeat(64)]).unwrap();
    db.execute("INSERT INTO semantic_producer_languages VALUES('producer','1','javascript',0)",[]).unwrap();
    header(&db,1,"a");header(&db,2,"a");header(&db,3,"a");
    for revision in [1,2,3] {
        for (ordinal,path,vid) in [(0,"a.js",1),(1,"b.js",2),(2,"c.js",3)] {
            let vid=if revision==2 && path=="c.js" {4}else{vid};
            db.execute("INSERT INTO revision_documents VALUES(?1,'javascript',?2,?3,1,NULL,NULL,1,1,'complete',NULL,?4,1)",params![semantic_pin_key(pin(revision)),path,vid,ordinal]).unwrap();
        }
    }
    let key=semantic_pin_key(pin(1));
    db.execute("INSERT INTO semantic_revision_producers VALUES(?1,'producer','1')",[&key]).unwrap();
    db.execute("INSERT INTO semantic_requested_producers VALUES(?1,'producer','1',?2)",params![key,"a".repeat(64)]).unwrap();
    db.execute("INSERT INTO semantic_requested_producer_languages VALUES(?1,'producer','javascript',0)",[&key]).unwrap();
    for (tuple,path) in [(1,"a.js"),(2,"b.js"),(3,"c.js")] {
        db.execute("INSERT INTO semantic_coverage VALUES(?1,?2,'producer','1','javascript',?3,1,1,'complete',NULL)",params![tuple,key,path]).unwrap();
    }
    db
}
fn header(db:&Connection,revision:u64,context:&str) {
    db.execute("INSERT INTO native_revisions VALUES(?1,'source-set',?2,?2,?2,?3,'[]','[]','{}','[]','[]',NULL,'[]',0,'{}','[]',?4)",params![semantic_pin_key(pin(revision)),context.repeat(64),format!("native-{revision}"),revision as i64]).unwrap();
}
fn projected(db:&Connection,revision:u64,path:&str)->SemanticProjection { semantic_projection_for(db,pin(revision),"producer","javascript",path).unwrap().unwrap() }

#[test]
fn pending_uses_recorded_endpoints_not_transitive_call_paths() {
    let db=fixture();
    // A -> B and B -> C are separate evidence, not consumption derivation.
    db.execute("INSERT INTO semantic_projection_dependencies VALUES(1,'javascript','b.js')",[]).unwrap();
    db.execute("INSERT INTO semantic_projection_dependencies VALUES(2,'javascript','c.js')",[]).unwrap();
    write_pending(&db,pin(1),pin(2),"root").unwrap();
    assert_eq!(projected(&db,2,"a.js").semantic_status,SemanticStatus::OlderBasis);
    assert_eq!(projected(&db,2,"b.js").semantic_status,SemanticStatus::CatchingUp);
    assert_eq!(projected(&db,2,"c.js").semantic_status,SemanticStatus::CatchingUp);
    assert!(!endpoint_eligible(&db,pin(2),"producer","javascript","b.js",pin(1)).unwrap());
    let a=projected(&db,2,"a.js");
    assert_eq!(a.semantic_basis_pin,pin(1));
    assert_eq!(a.basis_native_revision_id,"native-1");
    assert_eq!(a.effective_freshness(crate::semantic_evidence::Freshness::Fresh),crate::semantic_evidence::Freshness::PossiblyStale);
}

#[test]
fn restored_native_recomputes_flat_basis_without_mutating_retained_n() {
    let db=fixture();
    db.execute("INSERT INTO semantic_projection_dependencies VALUES(2,'javascript','c.js')",[]).unwrap();
    write_pending(&db,pin(1),pin(2),"root").unwrap();
    write_pending(&db,pin(2),pin(3),"root").unwrap();
    assert_eq!(projected(&db,3,"b.js").semantic_status,SemanticStatus::OlderBasis);
    assert_eq!(projected(&db,3,"b.js").semantic_basis_pin,pin(1));
    assert_eq!(projected(&db,2,"b.js").semantic_status,SemanticStatus::CatchingUp);
}

#[test]
fn missing_validated_basis_tuple_is_corruption_not_pending() {
    let db=fixture();
    db.execute("DELETE FROM semantic_coverage WHERE tuple_id=2",[]).unwrap();
    let error=write_pending(&db,pin(1),pin(2),"root").unwrap_err();
    assert!(error.is::<SelectedIntegrity>(),"{error:#}");
}

#[test]
fn missing_projection_disposition_is_corruption() {
    let db=fixture();
    write_pending(&db,pin(1),pin(2),"root").unwrap();
    db.execute("DELETE FROM semantic_projection_documents WHERE revision_id=?1 AND path='a.js'",[semantic_pin_key(pin(2))]).unwrap();
    let error=semantic_projection_for(&db,pin(2),"producer","javascript","a.js").unwrap_err();
    assert!(error.is::<SelectedIntegrity>(),"{error:#}");
}

#[test]
fn context_change_marks_only_tuples_recording_context_pending() {
    let db=fixture();
    db.execute("UPDATE native_revisions SET config_hash=?1 WHERE id=?2",params!["b".repeat(64),semantic_pin_key(pin(2))]).unwrap();
    // Captured basis already differs from P. That historical mismatch is not itself pending.
    db.execute("INSERT INTO semantic_basis VALUES(1,1,'producer','1',?1,?1,'javascript','source-set','captured',?1,?1,?1,?1)",["f".repeat(64)]).unwrap();
    index_dependencies(&db,&semantic_pin_key(pin(1))).unwrap();
    write_pending(&db,pin(1),pin(2),"root").unwrap();
    assert_eq!(projected(&db,2,"a.js").semantic_status,SemanticStatus::CatchingUp);
    assert_eq!(projected(&db,2,"b.js").semantic_status,SemanticStatus::OlderBasis);
}

#[test]
fn anchor_candidates_and_all_type_endpoints_are_indexed() {
    let db=fixture();
    // The index consumes validated publication rows, not chosen-target shortcuts.
    db.execute("INSERT INTO semantic_declaration_bindings VALUES(1,1,0,NULL,'proof','javascript','b.js','native-1','hash',0,1,'declarationName','ambiguous',NULL)",[]).unwrap();
    db.execute("INSERT INTO semantic_declaration_join_candidates VALUES(1,0,?1)",[vec![1u8;16]]).unwrap();
    index_dependencies(&db,&semantic_pin_key(pin(1))).unwrap();
    let endpoint:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM semantic_projection_dependencies WHERE tuple_id=1 AND path='b.js')",[],|r|r.get(0)).unwrap();
    assert!(endpoint);
}
