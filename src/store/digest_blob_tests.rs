//! #1248: the semantic tables store their 64-hex digests as 32-byte BLOBs. Every write path
//! stores the digest's bytes, reads give back the lowercase text, and a malformed digest fails
//! the write typed, inside its transaction, with nothing written. PR 2: a payload stores its
//! content hash once, and a derived provenance row only the raw proof it names.
use super::*;
use crate::native_records::{DocumentKey, Range};
use crate::semantic_records as sem;

const NATIVE: &str = "native-1";
const TABLES: [&str; 4] = [
    "semantic_raw_proofs",
    "semantic_provenance",
    "semantic_declaration_bindings",
    "semantic_call_bindings",
];

fn hex(digit: char) -> String {
    digit.to_string().repeat(64)
}

/// An empty index with one raw capture (7) for derived provenance to name.
fn index() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(CACHE_SCHEMA_V50).unwrap();
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    db.execute(
        "INSERT INTO semantic_raw_captures VALUES(7,1,0,'[\"rust\"]','producer',?1,'set','captured',x'','decoder')",
        [hex('a')],
    )
    .unwrap();
    db.execute(
        "INSERT INTO semantic_payloads(payload_id,producer_id,producer_version,language,path,version_id) VALUES(1,'producer','1','rust','a.rs','v')",
        [],
    )
    .unwrap();
    db
}

fn count(db: &Connection, table: &str) -> i64 {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn document() -> DocumentKey {
    DocumentKey {
        source_set_id: "set".into(),
        language: "rust".into(),
        path: "a.rs".into(),
    }
}

fn join(kind: sem::JoinKind, content_hash: String) -> sem::Join {
    sem::Join {
        anchor: sem::MeasuredAnchor {
            document: document(),
            revision_id: NATIVE.into(),
            content_hash,
            range: Range { start: 0, end: 1 },
            kind,
        },
        status: sem::JoinStatus::Unmatched,
        candidate_ids: Vec::new(),
        diagnostic: None,
    }
}

/// One measured provenance row, one derived from raw proof `hex('d')` of capture 7, and a
/// declaration and a call binding on the measured one.
fn publication() -> sem::SemanticPublication {
    let basis = sem::SemanticBasis {
        producer_id: "producer".into(),
        producer_version: "1".into(),
        producer_hash: hex('1'),
        artifact_hash: hex('a'),
        language: sem::Language::Rust,
        source_set_id: "set".into(),
        revision_id: "captured".into(),
        source_manifest_hash: hex('2'),
        toolchain_hash: hex('3'),
        config_hash: hex('4'),
        dependency_hash: hex('5'),
        lookup_dependencies: Vec::new(),
    };
    let provenance = |id: String, derived_from: Option<sem::DerivedFrom>| sem::Provenance {
        id,
        producer_id: "producer".into(),
        document: document(),
        revision_id: NATIVE.into(),
        content_hash: hex('c'),
        evidence_kind: sem::EvidenceKind::SemanticReference,
        basis: Some(basis.clone()),
        freshness: sem::Freshness::Fresh,
        derived_from,
    };
    let derived = sem::DerivedFrom {
        provenance_id: hex('d'),
        record_kind: sem::RecordKind::Occurrence,
        fact_digest: hex('e'),
    };
    sem::SemanticPublication {
        producers: Vec::new(),
        coverage: Vec::new(),
        provenance: vec![
            provenance(hex('f'), None),
            provenance(rejoin_id(&derived.provenance_id), Some(derived)),
        ],
        symbols: Vec::new(),
        declaration_bindings: vec![sem::DeclarationBinding {
            syntax_id: None,
            symbols: Vec::new(),
            join: join(sem::JoinKind::DeclarationName, hex('6')),
            provenance_id: hex('f'),
        }],
        references: Vec::new(),
        call_bindings: vec![sem::CallBinding {
            call_id: None,
            join: join(sem::JoinKind::Callee, hex('7')),
            resolution: sem::Resolution::Unresolved,
            declared_target: None,
            candidates: Vec::new(),
            dispatch: sem::Dispatch::Direct,
            possible_dispatch: Vec::new(),
            possible_dispatch_complete: false,
            stale_target: None,
            provenance_id: hex('f'),
        }],
        type_relationships: Vec::new(),
    }
}

fn rejoin_id(raw: &str) -> String {
    sem::rejoin_provenance_id(raw, NATIVE, sem::EvidenceKind::SemanticReference)
}

fn write(db: &Connection, publication: &sem::SemanticPublication) -> Result<()> {
    let coverage = sem::Coverage {
        producer_id: "producer".into(),
        language: sem::Language::Rust,
        source_set_id: "set".into(),
        document_path: "a.rs".into(),
        revision_id: NATIVE.into(),
        requested: true,
        selected: true,
        state: sem::CoverageState::Omitted,
        supported_roles: Vec::new(),
        observed_roles: Vec::new(),
        diagnostic: None,
        reasons: None,
    };
    let owns = publication
        .provenance
        .iter()
        .map(|p| p.id.as_str())
        .collect();
    write_semantic_tuple(
        db,
        1,
        NATIVE,
        &coverage,
        publication,
        &owns,
        &[],
        &BTreeSet::new(),
    )
    .map(drop)
}

#[test]
fn tuple_digests_are_stored_as_bytes_and_read_as_lowercase_text() {
    let db = index();
    write(&db, &publication()).unwrap();
    let stored: Vec<(String, i64)> = db
        .prepare(
            "SELECT typeof(content_hash),length(content_hash) FROM semantic_payloads
             UNION ALL SELECT typeof(derived_from_provenance_id),length(derived_from_provenance_id) FROM semantic_provenance WHERE capture_id IS NOT NULL
             UNION ALL SELECT typeof(join_content_hash),length(join_content_hash) FROM semantic_declaration_bindings
             UNION ALL SELECT typeof(join_content_hash),length(join_content_hash) FROM semantic_call_bindings",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(stored, vec![("blob".to_owned(), 32); 4]);
    let read: (String, String, String, String) = db
        .query_row(
            "SELECT (SELECT lower(hex(content_hash)) FROM semantic_payloads WHERE payload_id=p.payload_id),lower(hex(p.derived_from_provenance_id)),
                    (SELECT lower(hex(join_content_hash)) FROM semantic_declaration_bindings),
                    (SELECT lower(hex(join_content_hash)) FROM semantic_call_bindings)
             FROM semantic_provenance p WHERE p.capture_id=7",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(read, (hex('c'), hex('d'), hex('6'), hex('7')));
}

/// One malformed value per converted column the tuple writer fills: uppercase, short, non-hex,
/// empty and overlong text all fail with the same typed error, and the transaction they ran in
/// holds no row of the tuple once it rolls back.
#[test]
fn a_malformed_tuple_digest_fails_typed_and_writes_nothing() {
    type Corrupt = fn(&mut sem::SemanticPublication);
    let cases: [(&str, Corrupt); 4] = [
        ("semantic_payloads.content_hash", |p| {
            p.provenance[0].content_hash = hex('C');
        }),
        ("semantic_provenance.derived_from_provenance_id", |p| {
            let raw = hex('d')[..63].to_owned();
            p.provenance[1].id = rejoin_id(&raw);
            p.provenance[1].derived_from.as_mut().unwrap().provenance_id = raw;
        }),
        ("semantic_declaration_bindings.join_content_hash", |p| {
            p.declaration_bindings[0].join.anchor.content_hash = String::new();
        }),
        ("semantic_call_bindings.join_content_hash", |p| {
            p.call_bindings[0].join.anchor.content_hash = format!("{}0", hex('7'));
        }),
    ];
    for (column, corrupt) in cases {
        let db = index();
        let mut publication = publication();
        corrupt(&mut publication);
        {
            let tx = db.unchecked_transaction().unwrap();
            let error = write(&tx, &publication).unwrap_err();
            assert_eq!(error.to_string(), "invalid semantic digest", "{column}");
        }
        for table in TABLES {
            assert_eq!(count(&db, table), 0, "{column}: {table}");
        }
    }
}

/// PR 2: every provenance row of one payload names the payload's document version, so their
/// content hashes are one value; two differing ones fail the write typed, with nothing written.
#[test]
fn differing_content_hashes_in_one_payload_fail_the_write() {
    let db = index();
    let mut publication = publication();
    publication.provenance[1].content_hash = hex('b');
    {
        let tx = db.unchecked_transaction().unwrap();
        let error = write(&tx, &publication).unwrap_err();
        assert_eq!(
            error.to_string(),
            "semantic provenance content hashes differ within a payload"
        );
    }
    for table in TABLES {
        assert_eq!(count(&db, table), 0, "{table}");
    }
    let hash: Option<Vec<u8>> = db
        .query_row("SELECT content_hash FROM semantic_payloads", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(hash, None);
}

/// A stage with two valid claims of artifact 0, published as capture 7, whose document the
/// publication has keyed (`RAW_PROOF_DOCUMENTS_SQL`).
fn staged() -> Connection {
    let db = index();
    db.execute_batch(
        "ATTACH DATABASE ':memory:' AS stage;
         CREATE TABLE stage.sources(ord INTEGER PRIMARY KEY, language TEXT NOT NULL, path TEXT NOT NULL);
         CREATE TABLE stage.documents(source_ord INTEGER PRIMARY KEY, artifact INTEGER NOT NULL);
         CREATE TABLE stage.claims(claim_id INTEGER PRIMARY KEY, artifact INTEGER NOT NULL, source_ord INTEGER NOT NULL, record_kind TEXT NOT NULL, fact_digest TEXT NOT NULL, proof_id TEXT NOT NULL, canonical_bytes BLOB NOT NULL);
         INSERT INTO stage.sources VALUES(0,'rust','a.rs'),(1,'rust','b.rs');
         INSERT INTO stage.documents VALUES(0,0);",
    )
    .unwrap();
    db.execute(&with_capture_ids("(0,7)", RAW_PROOF_DOCUMENTS_SQL), [])
        .unwrap();
    for (claim, proof, digest) in [(1, hex('9'), hex('1')), (2, hex('0'), hex('2'))] {
        db.execute(
            "INSERT INTO stage.claims VALUES(?1,0,0,'occurrence',?2,?3,x'7b7d')",
            params![claim, digest, proof],
        )
        .unwrap();
    }
    db
}

fn stored_proofs(db: &Connection) -> Vec<(String, String)> {
    db.prepare(
        "SELECT lower(hex(proof_id)),lower(hex(fact_digest)) FROM semantic_raw_proofs ORDER BY proof_id",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// Staged proofs are stored as bytes in digest order, and a stored batch's refill skips its
/// existing rows, as `INSERT OR IGNORE` did.
#[test]
fn staged_raw_proofs_are_stored_as_bytes_and_a_refill_skips_stored_rows() {
    let db = staged();
    write_raw_proofs(&db, "(0,7)").unwrap();
    write_raw_proofs(&db, "(0,7)").unwrap();
    assert_eq!(
        stored_proofs(&db),
        vec![(hex('0'), hex('2')), (hex('9'), hex('1'))]
    );
    let types: (String, i64, String, i64) = db
        .query_row(
            "SELECT typeof(proof_id),length(proof_id),typeof(fact_digest),length(fact_digest) FROM semantic_raw_proofs LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(types, ("blob".to_owned(), 32, "blob".to_owned(), 32));
}

/// A raw proof names its document by the publication's integer key (#1248 PR 4), never by
/// language and path, and reads resolve it back.
#[test]
fn staged_raw_proofs_name_their_document_by_key() {
    let db = staged();
    write_raw_proofs(&db, "(0,7)").unwrap();
    let documents: Vec<(i64, String, String)> = db
        .prepare("SELECT p.capture_id,d.language,d.path FROM semantic_raw_proofs p JOIN semantic_raw_proof_documents d ON d.document_id=p.document_id AND d.capture_id=p.capture_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(documents, vec![(7, "rust".into(), "a.rs".into()); 2]);
}

/// A staged claim whose document has no key would store a NULL document; it fails the
/// publication typed instead of dropping its row, and nothing of the batch is written.
#[test]
fn a_staged_proof_without_a_keyed_document_fails_typed_and_writes_nothing() {
    let db = staged();
    db.execute("UPDATE stage.claims SET source_ord=1 WHERE claim_id=2", [])
        .unwrap();
    {
        let tx = db.unchecked_transaction().unwrap();
        let error = write_raw_proofs(&tx, "(0,7)").unwrap_err();
        assert!(
            error.to_string().starts_with("invalid staged raw proof: "),
            "{error:#}"
        );
    }
    assert_eq!(count(&db, "semantic_raw_proofs"), 0);
}

/// `unhex` alone would store uppercase text and, under `INSERT OR IGNORE`, silently drop a row
/// it cannot decode. Every malformed staged proof ID or fact digest fails typed instead, and
/// the transaction holds none of the batch's proofs once it rolls back.
#[test]
fn a_malformed_staged_digest_fails_typed_and_writes_nothing() {
    for column in ["proof_id", "fact_digest"] {
        for bad in [
            hex('A'),
            hex('3')[..63].to_owned(),
            format!("{}0", hex('3')),
            format!("{}00", hex('3')),
            hex('g'),
            String::new(),
        ] {
            let db = staged();
            db.execute(
                &format!("UPDATE stage.claims SET {column}=?1 WHERE claim_id=2"),
                [&bad],
            )
            .unwrap();
            {
                let tx = db.unchecked_transaction().unwrap();
                let error = write_raw_proofs(&tx, "(0,7)").unwrap_err();
                assert!(
                    error.to_string().starts_with("invalid staged raw proof: "),
                    "{column} {bad}: {error:#}"
                );
                assert!(error.downcast_ref::<rusqlite::Error>().is_none());
            }
            assert_eq!(count(&db, "semantic_raw_proofs"), 0, "{column} {bad}");
        }
    }
}
