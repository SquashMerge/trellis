use super::*;

#[test]
fn semantic_fact_children_reuse_statement_without_reprepare_per_symbol() {
    let db = Connection::open_in_memory().unwrap();
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    db.execute_batch(CACHE_SCHEMA_V43).unwrap();
    db.execute("INSERT INTO native_revisions VALUES('key','source-set',?1,?1,?1,'native','[]','[]','{}','[]','[]',NULL,'[]',0,'{}','[]',1)", ["a".repeat(64)]).unwrap();
    db.execute("INSERT INTO semantic_coverage VALUES(1,'key','producer','1','rust','main.rs',1,1,'complete',NULL,1)", []).unwrap();
    db.execute("INSERT INTO semantic_provenance(payload_id,ordinal,provenance_id,content_hash,evidence_kind,basis_id) VALUES(1,0,'proof','hash','measuredSyntax',1)", []).unwrap();
    for id in 10_i64..14 {
        db.execute(
            "INSERT INTO semantic_symbols VALUES(?1,1,?2,'scip',?3,'global',NULL,NULL,NULL,0)",
            params![id, id - 10, format!("symbol-{id}")],
        )
        .unwrap();
        db.execute(
            "INSERT INTO semantic_symbol_targets VALUES(?1,0,'external',NULL,NULL,NULL,NULL,'scip',?2,'global',NULL,NULL)",
            params![id, format!("target-{id}")],
        ).unwrap();
    }
    let document = crate::native_records::DocumentKey {
        source_set_id: "source-set".into(),
        language: "rust".into(),
        path: "main.rs".into(),
    };
    let mut result = crate::semantic_records::SemanticSnapshot {
        producers: vec![],
        coverage: vec![],
        provenance: vec![],
        symbols: vec![],
        declaration_bindings: vec![],
        references: vec![],
        call_bindings: vec![],
        type_relationships: vec![],
    };
    SEMANTIC_CHILD_REPREPARES.with(|count| count.set(0));
    let projection =
        semantic_payload::project_payload(&db, "key", 1, semantic_payload::FreshnessSource::Stored)
            .unwrap();
    read_semantic_facts(&db, &projection, &document, &mut result, None).unwrap();
    assert_eq!(result.symbols.len(), 4);
    for (offset, symbol) in result.symbols.iter().enumerate() {
        let expected = format!("target-{}", offset + 10);
        let [crate::semantic_records::Target::External { symbol: target }] =
            symbol.declarations.as_slice()
        else {
            panic!("expected one external target: {:?}", symbol.declarations);
        };
        assert_eq!(target.symbol, expected);
    }
    assert_eq!(
        SEMANTIC_CHILD_REPREPARES.with(|count| count.take()),
        0,
        "one warmup and three repeated child reads must retain their statement"
    );
}
