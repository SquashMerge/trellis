use super::*;

#[test]
fn semantic_fact_children_reuse_statement_without_reprepare_per_symbol() {
    let db = Connection::open_in_memory().unwrap();
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    db.execute_batch(CACHE_SCHEMA_V35).unwrap();
    for id in 10_i64..14 {
        db.execute(
            "INSERT INTO semantic_symbols VALUES(?1,1,?2,'scip',?3,'global',NULL,NULL,NULL,'proof')",
            params![id, id - 10, format!("symbol-{id}")],
        ).unwrap();
        db.execute(
            "INSERT INTO semantic_symbol_targets VALUES(?1,0,'external',NULL,NULL,NULL,NULL,'scip',?2,'global',NULL,NULL)",
            params![id, format!("target-{id}")],
        ).unwrap();
    }
    let document = crate::native_evidence::DocumentKey {
        source_set_id: "source-set".into(),
        language: "rust".into(),
        path: "main.rs".into(),
    };
    let mut result = crate::semantic_evidence::SemanticSnapshot {
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
    read_semantic_facts(&db, 1, &document, "native", &mut result, None).unwrap();
    assert_eq!(result.symbols.len(), 4);
    for (offset, symbol) in result.symbols.iter().enumerate() {
        let expected = format!("target-{}", offset + 10);
        let [crate::semantic_evidence::Target::External { symbol: target }] =
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
