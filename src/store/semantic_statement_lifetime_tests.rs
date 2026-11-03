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

fn tuple_fixture() -> (
    Connection,
    crate::semantic_evidence::Coverage,
    crate::semantic_evidence::SemanticPublication,
) {
    use crate::semantic_evidence as sem;
    let db = Connection::open_in_memory().unwrap();
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    db.execute_batch(CACHE_SCHEMA_V35).unwrap();
    let coverage = sem::Coverage {
        producer_id: "producer".into(),
        language: sem::Language::Rust,
        source_set_id: "source".into(),
        document_path: "main.rs".into(),
        revision_id: "destination".into(),
        requested: true,
        selected: true,
        state: sem::CoverageState::Complete,
        supported_roles: vec![],
        observed_roles: vec![],
        diagnostic: None,
        reasons: None,
    };
    let basis = sem::SemanticBasis {
        producer_id: coverage.producer_id.clone(),
        producer_version: "1".into(),
        producer_hash: "producer-hash".into(),
        artifact_hash: "a".repeat(64),
        language: sem::Language::Rust,
        source_set_id: coverage.source_set_id.clone(),
        revision_id: "captured".into(),
        source_manifest_hash: "manifest".into(),
        toolchain_hash: "toolchain".into(),
        config_hash: "config".into(),
        dependency_hash: "dependencies".into(),
        lookup_dependencies: vec!["first".into(), "second".into()],
    };
    let proof = sem::Provenance {
        id: "proof".into(),
        producer_id: coverage.producer_id.clone(),
        document: crate::native_evidence::DocumentKey {
            source_set_id: coverage.source_set_id.clone(),
            language: coverage.language.as_str().into(),
            path: coverage.document_path.clone(),
        },
        revision_id: coverage.revision_id.clone(),
        content_hash: "content".into(),
        evidence_kind: sem::EvidenceKind::SemanticReference,
        basis: Some(basis),
        freshness: sem::Freshness::Fresh,
        derived_from: None,
    };
    let publication = sem::SemanticPublication {
        producers: vec![],
        coverage: vec![],
        provenance: vec![proof],
        symbols: vec![],
        declaration_bindings: vec![],
        references: vec![],
        call_bindings: vec![],
        type_relationships: vec![],
    };
    (db, coverage, publication)
}

#[test]
fn tuple_basis_interning_uses_every_field_and_keeps_fresh_tuple_rows() {
    use crate::semantic_evidence as sem;
    let (db, coverage, mut publication) = tuple_fixture();
    let original = publication.provenance[0].clone();
    let mut equal = original.clone();
    equal.id = "equal-proof".into();
    publication.provenance.push(equal);
    let changes: [fn(&mut sem::SemanticBasis); 13] = [
        |b| b.producer_id.push('x'),
        |b| b.producer_version.push('x'),
        |b| b.producer_hash.push('x'),
        |b| b.artifact_hash.push('x'),
        |b| b.language = sem::Language::Java,
        |b| b.source_set_id.push('x'),
        |b| b.revision_id.push('x'),
        |b| b.source_manifest_hash.push('x'),
        |b| b.toolchain_hash.push('x'),
        |b| b.config_hash.push('x'),
        |b| b.dependency_hash.push('x'),
        |b| b.lookup_dependencies.push("third".into()),
        |b| b.lookup_dependencies.reverse(),
    ];
    for (ordinal, change) in changes.into_iter().enumerate() {
        let mut proof = original.clone();
        proof.id = format!("changed-{ordinal}");
        change(proof.basis.as_mut().unwrap());
        publication.provenance.push(proof);
    }
    for tuple in [1, 2] {
        write_semantic_tuple(&db, tuple, &coverage, &publication).unwrap();
        let mut statement = db.prepare(
            "SELECT provenance_id,basis_id FROM semantic_provenance WHERE tuple_id=?1 ORDER BY rowid"
        ).unwrap();
        let rows: Vec<(String, i64)> = statement
            .query_map([tuple], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows.len(), publication.provenance.len());
        assert_eq!(rows[0].1, rows[1].1);
        let distinct: BTreeSet<_> = rows.iter().map(|row| row.1).collect();
        assert_eq!(distinct.len(), changes.len() + 1);
        if tuple == 2 {
            let prior_max: i64 = db
                .query_row(
                    "SELECT max(basis_id) FROM semantic_basis WHERE tuple_id=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(distinct.iter().all(|id| *id > prior_max));
        }
    }
}

#[test]
fn tuple_capture_memo_uses_full_lookup_key() {
    use crate::semantic_evidence as sem;
    let (db, coverage, mut publication) = tuple_fixture();
    let original = publication.provenance[0].clone();
    publication.provenance.clear();
    for ordinal in 0..4 {
        let mut proof = original.clone();
        proof.id = format!("derived-{ordinal}");
        proof.derived_from = Some(sem::DerivedFrom {
            provenance_id: "captured-proof".into(),
            record_kind: sem::RecordKind::Occurrence,
            fact_digest: "f".repeat(64),
        });
        let basis = proof.basis.as_mut().unwrap();
        match ordinal {
            1 => basis.artifact_hash = "b".repeat(64),
            2 => basis.source_set_id.push('x'),
            3 => basis.revision_id.push('x'),
            _ => {}
        }
        db.execute(
            "INSERT INTO semantic_raw_captures VALUES(?1,1,?2,'rust',?3,?4,?5,?6,X'','decoder')",
            params![
                ordinal + 1,
                ordinal,
                proof.producer_id,
                basis.artifact_hash,
                basis.source_set_id,
                basis.revision_id
            ],
        )
        .unwrap();
        publication.provenance.push(proof);
    }
    let mut repeated = publication.provenance[0].clone();
    repeated.id = "repeated".into();
    publication.provenance.push(repeated);
    write_semantic_tuple(&db, 1, &coverage, &publication).unwrap();
    let mut statement = db
        .prepare("SELECT capture_id FROM semantic_provenance ORDER BY rowid")
        .unwrap();
    let ids: Vec<i64> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(ids, vec![1, 2, 3, 4, 1]);
}

#[test]
fn tuple_memo_preserves_missing_basis_and_capture_errors_and_rollback() {
    use crate::semantic_evidence as sem;
    for missing_basis in [true, false] {
        let (mut db, coverage, mut publication) = tuple_fixture();
        let mut second = publication.provenance[0].clone();
        second.id = "second".into();
        if missing_basis {
            second.basis = None;
        } else {
            second.derived_from = Some(sem::DerivedFrom {
                provenance_id: "absent".into(),
                record_kind: sem::RecordKind::Occurrence,
                fact_digest: "f".repeat(64),
            });
        }
        publication.provenance.push(second);
        let tx = db.transaction().unwrap();
        let error = write_semantic_tuple(&tx, 1, &coverage, &publication).unwrap_err();
        if missing_basis {
            assert!(
                error
                    .to_string()
                    .contains("missing captured semantic basis")
            );
        } else {
            assert!(matches!(
                error.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::QueryReturnedNoRows)
            ));
        }
        tx.rollback().unwrap();
        for table in [
            "semantic_basis",
            "semantic_basis_lookup_dependencies",
            "semantic_provenance",
        ] {
            let count: i64 = db
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0);
        }
    }
}
