//! Private, capture-side admission witnesses. The caller streams validated documents inside
//! its publication savepoint and inserts the complete marker last. These rows never become
//! public semantic answers or substitute for a partly released public import.
//!
//! Native-first S (#620): a FULL catch-up of a batch whose import landed below its capture
//! first joins the batch against its own captured revision, keeps those per-document rows in
//! its private stage, and publishes them with the S commit. Later witnessed catch-ups use them
//! as the batch's original source associations.
use super::*;
use crate::native_records::DocumentKey;
use crate::semantic_records as sem;

const STAGE_TABLE: &str = "CREATE TABLE IF NOT EXISTS capture_witness_documents(language TEXT NOT NULL,path TEXT NOT NULL,snapshot_json TEXT NOT NULL,PRIMARY KEY(language,path))";

/// Keep a captured==captured staged join's documents in the producer's private stage.
pub(crate) fn stage_documents(conn: &Connection, producer: &sem::Producer) -> Result<()> {
    conn.execute_batch(STAGE_TABLE)?;
    conn.execute("DELETE FROM capture_witness_documents", [])?;
    let mut insert = conn.prepare(
        "INSERT INTO capture_witness_documents(language,path,snapshot_json) VALUES(?1,?2,?3)",
    )?;
    crate::semantic_import::read_staged_documents(conn, "", producer, &mut |publication| {
        let (language, path, json) = snapshot_row(publication)?;
        insert.execute(params![language, path, json])?;
        Ok(())
    })
}

/// Publish a staged capture witness inside the S commit, unless one is already complete.
pub(crate) fn publish_staged(tx: &Connection, stage: &Connection, batch_id: i64) -> Result<()> {
    if exists(tx, batch_id)? {
        return Ok(());
    }
    tx.execute(
        "DELETE FROM semantic_batch_witness_documents WHERE batch_id=?1",
        [batch_id],
    )?;
    let mut rows = stage.prepare(
        "SELECT language,path,snapshot_json FROM capture_witness_documents ORDER BY language,path",
    )?;
    let mut rows = rows.query([])?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO semantic_batch_witness_documents(batch_id,language,path,snapshot_json) VALUES(?1,?2,?3,?4)",
    )?;
    while let Some(row) = rows.next()? {
        insert.execute(params![
            batch_id,
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?
        ])?;
    }
    complete(tx, batch_id)
}

/// The keyed snapshot row of one validated captured==captured document.
fn snapshot_row(publication: sem::SemanticPublication) -> Result<(String, String, String)> {
    let [coverage] = publication.coverage.as_slice() else {
        anyhow::bail!("captured witness requires one coverage document");
    };
    let language = serde_json::to_value(coverage.language)?;
    let language = language
        .as_str()
        .context("captured witness language")?
        .to_owned();
    let path = coverage.document_path.clone();
    let snapshot = sem::SemanticSnapshot {
        producers: publication.producers,
        coverage: publication.coverage,
        provenance: publication.provenance,
        symbols: publication.symbols,
        declaration_bindings: publication.declaration_bindings,
        references: publication.references,
        call_bindings: publication.call_bindings,
        type_relationships: publication.type_relationships,
    };
    Ok((language, path, serde_json::to_string(&snapshot)?))
}

pub(crate) fn exists(tx: &Connection, batch_id: i64) -> Result<bool> {
    tx.prepare_cached("SELECT EXISTS(SELECT 1 FROM semantic_batch_witnesses WHERE batch_id=?1)")?
        .query_row([batch_id], |row| row.get(0))
        .map_err(Into::into)
}

/// Moves one already validated captured==captured publication into the existing snapshot
/// representation. The caller owns validation, cancellation and rollback of the whole batch.
#[cfg(test)]
pub(super) fn write_document(
    tx: &Connection,
    batch_id: i64,
    publication: sem::SemanticPublication,
) -> Result<()> {
    let [coverage] = publication.coverage.as_slice() else {
        anyhow::bail!("captured witness requires one coverage document");
    };
    let language = serde_json::to_value(coverage.language)?;
    let language = language.as_str().context("captured witness language")?;
    let path = coverage.document_path.clone();
    let snapshot = sem::SemanticSnapshot {
        producers: publication.producers,
        coverage: publication.coverage,
        provenance: publication.provenance,
        symbols: publication.symbols,
        declaration_bindings: publication.declaration_bindings,
        references: publication.references,
        call_bindings: publication.call_bindings,
        type_relationships: publication.type_relationships,
    };
    tx.prepare_cached(
        "INSERT INTO semantic_batch_witness_documents(batch_id,language,path,snapshot_json)
         VALUES(?1,?2,?3,?4)
         ON CONFLICT(batch_id,language,path) DO UPDATE SET snapshot_json=excluded.snapshot_json",
    )?
    .execute(params![
        batch_id,
        language,
        path,
        serde_json::to_string(&snapshot)?
    ])?;
    Ok(())
}

/// Publish only after every document has been validated and written in the same savepoint.
pub(crate) fn complete(tx: &Connection, batch_id: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO semantic_batch_witnesses(batch_id) VALUES(?1)",
        [batch_id],
    )?;
    Ok(())
}

pub(crate) fn document(
    tx: &Connection,
    batch_id: i64,
    document: &DocumentKey,
) -> Result<Option<sem::SemanticSnapshot>> {
    let json: Option<String> = tx
        .prepare_cached(
            "SELECT d.snapshot_json FROM semantic_batch_witness_documents d
             JOIN semantic_batch_witnesses w ON w.batch_id=d.batch_id
             WHERE d.batch_id=?1 AND d.language=?2 AND d.path=?3",
        )?
        .query_row(params![batch_id, document.language, document.path], |row| {
            row.get(0)
        })
        .optional()?;
    json.map(|json| {
        serde_json::from_str(&json).context(SelectedIntegrity(
            "incompatible_index: corrupt captured witness snapshot".into(),
        ))
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orphan_witness_chunk_invalidates_marker_and_allows_complete_rebuild() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(CACHE_SCHEMA_V43).unwrap();
        db.pragma_update(None, "foreign_keys", "ON").unwrap();
        db.execute(
            "INSERT INTO semantic_raw_batches VALUES(1,'producer','1','profile',
             'sources','captured',?1,?1,X'','decoder')",
            ["a".repeat(64)],
        )
        .unwrap();
        let publication = |ordinal| sem::SemanticPublication {
            producers: vec![],
            coverage: vec![sem::Coverage {
                producer_id: "producer".into(),
                language: sem::Language::Javascript,
                source_set_id: "sources".into(),
                document_path: format!("doc-{ordinal:02}.js"),
                revision_id: "captured".into(),
                requested: true,
                selected: true,
                state: sem::CoverageState::Complete,
                supported_roles: vec![],
                observed_roles: vec![],
                diagnostic: None,
                reasons: None,
            }],
            provenance: vec![],
            symbols: vec![],
            declaration_bindings: vec![],
            references: vec![],
            call_bindings: vec![],
            type_relationships: vec![],
        };
        let key = |ordinal| DocumentKey {
            source_set_id: "sources".into(),
            language: "javascript".into(),
            path: format!("doc-{ordinal:02}.js"),
        };
        for ordinal in 0..65 {
            write_document(&db, 1, publication(ordinal)).unwrap();
        }
        complete(&db, 1).unwrap();

        assert!(release_semantic_tuple(&db, "released").unwrap());
        assert!(!exists(&db, 1).unwrap());
        let remaining: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM semantic_batch_witness_documents",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1);
        for ordinal in 0..65 {
            assert!(document(&db, 1, &key(ordinal)).unwrap().is_none());
        }

        // A reselected batch rebuilds from the same immutable capture before GC resumes.
        // The surviving row must not prevent completing that rebuild.
        for ordinal in 0..65 {
            write_document(&db, 1, publication(ordinal)).unwrap();
        }
        complete(&db, 1).unwrap();
        assert!(exists(&db, 1).unwrap());
        for ordinal in 0..65 {
            let snapshot = document(&db, 1, &key(ordinal)).unwrap().unwrap();
            assert_eq!(snapshot.coverage, publication(ordinal).coverage);
        }
    }

    #[test]
    fn orphan_witness_cleanup_defers_batches_past_the_header_chunk() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(CACHE_SCHEMA_V43).unwrap();
        db.pragma_update(None, "foreign_keys", "ON").unwrap();
        let hash = "a".repeat(64);
        // One more header than a release chunk; the first batch keeps its capture through
        // the first chunk, so batch deletion would otherwise reach the undeleted header.
        for batch in 1_i64..=65 {
            db.execute(
                "INSERT INTO semantic_raw_batches VALUES(?1,'producer','1','profile',
                 'sources','captured',?2,?3,X'','decoder')",
                params![batch, hash, format!("{batch:064x}")],
            )
            .unwrap();
            complete(&db, batch).unwrap();
        }
        db.execute(
            "INSERT INTO semantic_raw_captures VALUES(1,1,0,'[\"rust\"]','producer',?1,
             'sources','captured',X'','decoder')",
            [&hash],
        )
        .unwrap();
        for proof in 1..=65 {
            db.execute(
                "INSERT INTO semantic_raw_proofs VALUES(1,?1,'occurrence',?2,
                 'rust','main.rs',X'')",
                params![format!("proof-{proof}"), format!("{proof:064x}")],
            )
            .unwrap();
        }
        let count = |table: &str| -> i64 {
            db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        };

        assert!(release_semantic_tuple(&db, "released").unwrap());
        assert_eq!(count("semantic_batch_witnesses"), 1);
        assert_eq!(count("semantic_raw_proofs"), 1);
        assert_eq!(count("semantic_raw_captures"), 1);
        assert_eq!(count("semantic_raw_batches"), 2);
        assert!(release_semantic_tuple(&db, "released").unwrap());
        for table in [
            "semantic_batch_witness_documents",
            "semantic_batch_witnesses",
            "semantic_raw_proofs",
            "semantic_raw_captures",
            "semantic_raw_batches",
        ] {
            assert_eq!(count(table), 0, "{table} did not drain");
        }
        assert!(!release_semantic_tuple(&db, "released").unwrap());
    }
}
