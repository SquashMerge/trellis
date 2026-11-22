use super::retained_batch::*;
use super::*;

fn pin(revision: u64) -> IndexPin {
    IndexPin {
        index_generation: uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
        index_revision: revision,
    }
}

fn h(c: &str) -> String {
    c.repeat(64)
}

/// Proof IDs are 64-hex digits; `p(1) < p(2) < p(3)` in stored order.
fn p(n: u8) -> String {
    h(&(n + 3).to_string())
}

/// Batch 1 with two captures: ordinal 0 (capture 10: two documents, three proofs and a
/// five-byte binary) and ordinal 1 (capture 11: nothing), and one excluded document.
fn fixture() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(CACHE_SCHEMA_V47).unwrap();
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    db.execute(
        "INSERT INTO semantic_raw_batches VALUES(1,'producer','1','profile','set','native-1',?1,?2,x'7b7d','decoder')",
        params![h("m"), h("d")],
    )
    .unwrap();
    for (capture, ordinal, hash) in [(10, 0, h("a")), (11, 1, h("b"))] {
        db.execute(
            "INSERT INTO semantic_raw_captures VALUES(?1,1,?2,'[\"java\"]','producer',?3,'set','native-1',x'0102030405','decoder')",
            params![capture, ordinal, hash],
        )
        .unwrap();
    }
    for (path, capped) in [("a.java", 0), ("b.java", 1)] {
        db.execute(
            "INSERT INTO semantic_raw_documents VALUES(10,'java',?1,?2,?3,?4)",
            params![path, h("s"), h("w"), capped],
        )
        .unwrap();
    }
    for (proof, digest, path) in [
        (p(1), h("1"), "a.java"),
        (p(2), h("2"), "a.java"),
        (p(3), h("3"), "b.java"),
    ] {
        db.execute(
            "INSERT INTO semantic_raw_proofs VALUES(10,unhex(?1),'occurrence',unhex(?2),'java',?3,x'aa')",
            params![proof, digest, path],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO semantic_raw_exclusions VALUES(1,'java','z.java','unsupported')",
        [],
    )
    .unwrap();
    db
}

#[test]
fn batch_rows_are_the_selected_batch_descriptor_and_metadata() {
    let db = fixture();
    let rows = RetainedBatchRows::for_tests(&db, 1, pin(1));
    assert_eq!(rows.batch_id(), 1);
    assert_eq!(rows.descriptor_length().unwrap(), 2);
    assert_eq!(rows.descriptor().unwrap(), (b"{}".to_vec(), h("d")));
    let manifest = h("m");
    let metadata = |decoder_version| BatchMetadata {
        producer_id: "producer",
        producer_version: "1",
        profile_id: "profile",
        source_set_id: "set",
        captured_revision: "native-1",
        source_manifest_hash: &manifest,
        decoder_version,
    };
    assert!(rows.metadata_matches(&metadata("decoder")).unwrap());
    assert!(!rows.metadata_matches(&metadata("other")).unwrap());
    // Another batch's handle reads no row of this one.
    let other = RetainedBatchRows::for_tests(&db, 2, pin(1));
    assert!(other.descriptor().is_err());
    assert_eq!(other.capture_count(10).unwrap(), 0);
}

#[test]
fn original_authority_is_a_selection_at_the_captured_pin_or_a_witness() {
    let db = fixture();
    let authority = |pin| {
        RetainedBatchRows::for_tests(&db, 1, pin)
            .has_original_authority("producer", "native-1", "set")
            .unwrap()
    };
    assert!(!authority(pin(1)));
    let key = semantic_pin_key(pin(1));
    db.execute("INSERT INTO native_revisions VALUES(?1,'set',?2,?2,?2,'native-1','[]','[]','{}','[]','[]',NULL,'[]',0,'{}','[]',1)", params![key, h("c")]).unwrap();
    db.execute(
        "INSERT INTO semantic_revision_raw_selections VALUES(?1,'producer',1,?1)",
        [&key],
    )
    .unwrap();
    assert!(authority(pin(1)));
    // The handle's own captured pin decides, not another retained pin.
    assert!(!authority(pin(2)));
    db.execute("INSERT INTO semantic_batch_witnesses VALUES(1)", [])
        .unwrap();
    assert!(authority(pin(2)));
}

#[test]
fn coverage_rows_page_by_key_within_the_batch() {
    let db = fixture();
    let rows = RetainedBatchRows::for_tests(&db, 1, pin(1));
    let first = rows
        .capture_document_key_after(0, ("", ""))
        .unwrap()
        .unwrap();
    assert_eq!(first, ("java".to_owned(), "a.java".to_owned()));
    let second = rows
        .capture_document_key_after(0, (&first.0, &first.1))
        .unwrap()
        .unwrap();
    assert_eq!(second.1, "b.java");
    assert_eq!(
        rows.capture_document_key_after(0, (&second.0, &second.1))
            .unwrap(),
        None
    );
    assert_eq!(rows.capture_document_key_after(1, ("", "")).unwrap(), None);
    assert_eq!(
        rows.assignments("java", "a.java").unwrap(),
        vec![(0, h("s"))]
    );
    assert_eq!(rows.assignments("java", "z.java").unwrap(), vec![]);
    assert_eq!(
        rows.exclusion("java", "z.java").unwrap().as_deref(),
        Some("unsupported")
    );
    assert_eq!(rows.exclusion("java", "a.java").unwrap(), None);
    assert_eq!(
        rows.exclusion_after(("", "")).unwrap(),
        Some(("java".to_owned(), "z.java".to_owned()))
    );
    assert_eq!(rows.exclusion_after(("java", "z.java")).unwrap(), None);
    assert_eq!(
        rows.capture_proof_document_after(0, &p(2)).unwrap(),
        Some((p(3), "java".to_owned(), "b.java".to_owned()))
    );
    assert_eq!(rows.capture_proof_document_after(0, &p(3)).unwrap(), None);
    assert_eq!(
        rows.capture_proof_document_after(0, "").unwrap(),
        Some((p(1), "java".to_owned(), "a.java".to_owned()))
    );
}

#[test]
fn a_capture_is_found_only_by_its_exact_metadata_and_read_in_blocks() {
    let db = fixture();
    let rows = RetainedBatchRows::for_tests(&db, 1, pin(1));
    let hash = h("a");
    let key = |byte_length| CaptureKey {
        ordinal: 0,
        languages_json: "[\"java\"]",
        producer_id: "producer",
        artifact_hash: &hash,
        source_set_id: "set",
        captured_revision: "native-1",
        decoder_version: "decoder",
        byte_length,
    };
    assert_eq!(rows.capture_id(&key(5)).unwrap(), Some(10));
    assert_eq!(rows.capture_id(&key(6)).unwrap(), None);
    assert_eq!(rows.capture_count(3).unwrap(), 2);
    assert_eq!(rows.capture_count(1).unwrap(), 1);
    let mut buffer = [0_u8; 3];
    assert_eq!(rows.read_capture(10, 0, &mut buffer).unwrap(), 3);
    assert_eq!(buffer, [1, 2, 3]);
    assert_eq!(rows.read_capture(10, 3, &mut buffer).unwrap(), 2);
    assert_eq!(buffer[..2], [4, 5]);
    assert_eq!(rows.read_capture(10, 5, &mut buffer).unwrap(), 0);
}

#[test]
fn capture_documents_and_proofs_page_and_compare_by_value() {
    let db = fixture();
    let rows = RetainedBatchRows::for_tests(&db, 1, pin(1));
    let mut document = rows
        .document_after(10, ("java", "a.java"))
        .unwrap()
        .unwrap();
    assert_eq!(document.path, "b.java");
    assert_eq!(document.capped, 1);
    assert!(
        rows.document_after(10, ("java", "b.java"))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        rows.document_key_after(10, ("", "")).unwrap(),
        Some(("java".to_owned(), "a.java".to_owned()))
    );
    assert_eq!(rows.document_matches(10, &document).unwrap(), Some(true));
    document.wire_digest = h("x");
    assert_eq!(rows.document_matches(10, &document).unwrap(), Some(false));
    document.path = "c.java".into();
    assert_eq!(rows.document_matches(10, &document).unwrap(), None);

    let mut proof = rows.proof_after(10, "").unwrap().unwrap();
    assert_eq!(proof.proof_id, p(1));
    assert_eq!(proof.fact_digest, h("1"));
    assert_eq!(proof.canonical_bytes, [0xaa]);
    assert_eq!(rows.proof_id_after(10, &p(1)).unwrap(), Some(p(2)));
    assert_eq!(rows.proof_id_after(10, &p(3)).unwrap(), None);
    assert_eq!(rows.proof_matches(10, &proof).unwrap(), Some(true));
    proof.canonical_bytes = vec![0xbb];
    assert_eq!(rows.proof_matches(10, &proof).unwrap(), Some(false));
    proof.proof_id = h("9");
    assert_eq!(rows.proof_matches(10, &proof).unwrap(), None);
}

#[test]
fn document_proofs_are_a_bounded_page_of_one_document_and_a_failing_row_stops_it() {
    let db = fixture();
    let rows = RetainedBatchRows::for_tests(&db, 1, pin(1));
    let mut seen = Vec::new();
    let mut each = |row: WitnessProofRow<'_>| {
        assert_eq!(row.fact_digest.len(), 64);
        seen.push(format!(
            "{} {} {} {}",
            row.proof_id, row.record_kind, row.language, row.path
        ));
        Ok(())
    };
    rows.document_proofs(10, ("java", "a.java"), ("", ""), 1, &mut each)
        .unwrap();
    // After p1's key only a.java's p2 follows: b.java's p3 is another document's.
    rows.document_proofs(
        10,
        ("java", "a.java"),
        ("occurrence", &h("1")),
        5,
        &mut each,
    )
    .unwrap();
    assert_eq!(
        seen,
        [
            format!("{} occurrence java a.java", p(1)),
            format!("{} occurrence java a.java", p(2))
        ]
    );
    let mut seen = 0;
    let error = rows
        .document_proofs(10, ("java", "b.java"), ("", ""), 5, &mut |_| {
            seen += 1;
            anyhow::bail!("row refused")
        })
        .unwrap_err();
    assert_eq!((seen, error.to_string().as_str()), (1, "row refused"));
}

#[test]
fn proof_ids_after_count_a_bounded_page_of_the_capture() {
    let db = fixture();
    let rows = RetainedBatchRows::for_tests(&db, 1, pin(1));
    assert_eq!(rows.proof_ids_after(10, "", 2).unwrap(), (2, Some(p(2))));
    assert_eq!(rows.proof_ids_after(10, &p(2), 2).unwrap(), (1, Some(p(3))));
    assert_eq!(rows.proof_ids_after(10, &p(3), 2).unwrap(), (0, None));
    assert_eq!(rows.proof_ids_after(11, "", 2).unwrap(), (0, None));
}

/// A malformed digest cursor or staged proof fails typed; it never reads as the end of a walk
/// or as a missing row.
#[test]
fn malformed_digest_cursors_and_proofs_fail() {
    let db = fixture();
    let rows = RetainedBatchRows::for_tests(&db, 1, pin(1));
    for after in ["p1", "P", &h("B"), &p(1)[1..]] {
        let cursor = |error: anyhow::Error| {
            assert_eq!(
                error.to_string(),
                "invalid semantic digest cursor",
                "{after}"
            )
        };
        cursor(rows.proof_id_after(10, after).unwrap_err());
        cursor(rows.proof_after(10, after).err().unwrap());
        cursor(rows.proof_ids_after(10, after, 2).unwrap_err());
        cursor(rows.capture_proof_document_after(0, after).unwrap_err());
        cursor(
            rows.document_proofs(
                10,
                ("java", "a.java"),
                ("occurrence", after),
                5,
                &mut |_| Ok(()),
            )
            .unwrap_err(),
        );
    }
    let mut proof = rows.proof_after(10, "").unwrap().unwrap();
    proof.fact_digest = h("A");
    let error = rows.proof_matches(10, &proof).unwrap_err();
    assert_eq!(error.to_string(), "invalid semantic digest");
}
