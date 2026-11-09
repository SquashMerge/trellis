//! Owner ruling 2026-11-04 (#669 H1): for an unchanged document, S copies the previously
//! published semantic rows to the new revision instead of re-staging and re-settling them.
//!
//! A document is copied when the older-basis projection already serves it (its projection
//! document is not pending: same document version and context, no dependency on changed
//! inputs) and the witness admitted it as reused. Its rows are the basis pin's tuple, which
//! S or an import validated and published; this trusts them as reads do (#261). The copy
//! re-keys exactly what the witnessed settlement derives from the destination: the native
//! revision named by coverage, references, join anchors and kept internal targets, the
//! rejoined provenance IDs and the provenance freshness. Since #680 (Decision 0021) the copy
//! names the basis tuple's shared payload from a new manifest row, which reads back as exactly
//! that re-keying, so S neither reads nor writes the payload's rows; corruption in them is found
//! by the read path as a typed integrity failure that schedules a re-index.
use super::*;
use crate::native_evidence::DocumentKey;
use crate::semantic_evidence as sem;

/// What a copied row takes from the destination native revision.
pub(crate) struct CopyDestination {
    native_revision_id: String,
    toolchain_hash: String,
    config_hash: String,
    dependency_hash: String,
    manifest_hash: String,
}

impl CopyDestination {
    /// The pending target's native revision and source manifest, as the join's settlement
    /// reads them (`SourceManifestHasher` over the canonical inventory).
    pub(crate) fn read(
        store: &Store,
        db: &Connection,
        target: &PendingSemanticTarget,
    ) -> Result<Self> {
        let key = semantic_pin_key(target.pin);
        let (native_revision_id, source_set_id, toolchain_hash, config_hash, dependency_hash): (
            String,
            String,
            String,
            String,
            String,
        ) = db
            .prepare_cached(
                "SELECT native_revision_id,source_set_id,toolchain_hash,config_hash,dependency_hash FROM native_revisions WHERE id=?1",
            )?
            .query_row([&key], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
        selected_ensure!(
            native_revision_id == target.native_revision_id,
            "incompatible_index: copy destination differs from the pending target"
        );
        let read = ReadRevision::new(target.pin, key);
        let reader = NativeReader {
            store,
            db,
            selected: &read,
        };
        let mut manifest =
            crate::semantic_batch::descriptor::SourceManifestHasher::new(&source_set_id);
        let mut cursor = CanonicalInventoryCursor::default();
        loop {
            let page = reader.canonical_inventory_page(&mut cursor, 256)?;
            if page.is_empty() {
                break;
            }
            for document in &page {
                manifest.push(
                    &document.key.language,
                    &document.key.path,
                    &document.content_hash,
                );
            }
        }
        Ok(Self {
            native_revision_id,
            toolchain_hash,
            config_hash,
            dependency_hash,
            manifest_hash: manifest.finish(),
        })
    }
}

/// The basis pin key and tuple of `document` when S copies it for `producer`: the
/// older-basis projection serves it at the pending target (not pending), the basis pin
/// published a tuple for it, that tuple omitted no association and is no unavailable S's
/// failure, and any payload it names is a keyed payload of `batch_id` that the running build's
/// `join_inputs` wrote (D4: the payload's key is then the key the document has at the pending
/// target, and a build that changes join output rotates every payload, copies included).
pub(crate) fn copy_source(
    db: &Connection,
    target: &PendingSemanticTarget,
    producer: &str,
    batch_id: i64,
    join_inputs: &str,
    document: &DocumentKey,
) -> Result<Option<(String, i64)>> {
    // A basis tuple that omitted rows for an absent target is not copied: the target can
    // return without the document changing, and only the raw claims can tell (the witness's
    // own rule for omitted original associations). Nor is a tuple an unavailable S published
    // (#826): it is no producer evidence, and the retained selection can still be re-joined.
    let row: Option<(bool, String, Option<i64>)> = db
        .prepare_cached(
            "SELECT d.pending OR EXISTS(SELECT 1 FROM semantic_coverage_reasons r WHERE (r.payload_id=c.payload_id OR r.tuple_id=c.tuple_id) AND r.omitted=1) \
             OR COALESCE(c.state='failed' AND substr(c.diagnostic,1,length(?5)+1)=?5||':',0) \
             OR COALESCE(c.payload_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM semantic_payloads p WHERE p.payload_id=c.payload_id AND p.payload_key IS NOT NULL AND p.batch_id=?6 AND p.join_inputs=?7),0),\
             o.basis_pin,c.tuple_id FROM semantic_projection_documents d \
             JOIN semantic_projection_origins o ON o.revision_id=d.revision_id AND o.producer_id=d.producer_id \
             LEFT JOIN semantic_coverage c ON c.revision_id=o.basis_pin AND c.producer_id=o.producer_id \
             AND c.language=d.language AND c.path=d.path \
             WHERE d.revision_id=?1 AND d.producer_id=?2 AND d.language=?3 AND d.path=?4",
        )?
        .query_row(
            params![
                semantic_pin_key(target.pin),
                producer,
                document.language,
                document.path,
                UNAVAILABLE_DIAGNOSTIC,
                batch_id,
                join_inputs
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(match row {
        Some((false, basis, Some(tuple))) => Some((basis, tuple)),
        _ => None,
    })
}

/// What decides, per document, whether S copies it for one producer: the pending target, the
/// producer's captured pin and the selected batch. Admission (which then stages none of the
/// document's proof identities) and witness preparation (which stages it as copied) ask the
/// same question.
#[derive(Clone)]
pub(crate) struct CopyFilter {
    pub(crate) target: PendingSemanticTarget,
    pub(crate) captured_pin: IndexPin,
    pub(crate) producer: String,
    pub(crate) batch_id: i64,
    pub(crate) join_inputs: String,
}

impl CopyFilter {
    /// Whether S copies `document`: [`copy_source`] names its basis tuple, and its captured
    /// and current versions are the same (the witness's reuse condition), so the copied rows
    /// are exactly what re-settling it would emit.
    pub(crate) fn copies(&self, db: &Connection, document: &DocumentKey) -> Result<bool> {
        if copy_source(
            db,
            &self.target,
            &self.producer,
            self.batch_id,
            &self.join_inputs,
            document,
        )?
        .is_none()
        {
            return Ok(false);
        }
        let current = witness::selected_version(db, self.target.pin, document)?;
        Ok(current.is_some()
            && witness::selected_version(db, self.captured_pin, document)? == current)
    }
}

/// The freshness each basis of a copied payload has at the destination, exactly as the
/// witnessed settlement would emit it for an unchanged document: its captured and destination
/// bytes agree (which the witness checked before choosing the copy), so only a basis the
/// destination no longer shares is possibly stale. Keyed by `basis_id`: a copy shares the
/// basis payload's bases.
pub(crate) fn reuse_freshness(
    db: &Connection,
    payload: i64,
    producer: &sem::Producer,
    destination: &CopyDestination,
) -> Result<BTreeMap<i64, sem::Freshness>> {
    let mut rows = db.prepare_cached(
        "SELECT basis_id,captured_native_revision_id,toolchain_hash,source_manifest_hash,config_hash,dependency_hash,producer_version,producer_hash,language FROM semantic_basis WHERE payload_id=?1",
    )?;
    let mut rows = rows.query([payload])?;
    let mut out = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let language: sem::Language = semantic_enum(row.get::<_, String>(8)?)?;
        let fresh = row.get::<_, String>(1)? == destination.native_revision_id
            && row.get::<_, String>(2)? == destination.toolchain_hash
            && row.get::<_, String>(3)? == destination.manifest_hash
            && row.get::<_, String>(4)? == destination.config_hash
            && row.get::<_, String>(5)? == destination.dependency_hash
            && row.get::<_, String>(6)? == producer.version
            && row.get::<_, String>(7)? == producer.executable_hash
            && producer.languages.contains(&language);
        out.insert(
            row.get(0)?,
            if fresh {
                sem::Freshness::Fresh
            } else {
                sem::Freshness::PossiblyStale
            },
        );
    }
    Ok(out)
}

/// What S writes for a copied document: a manifest row that names the basis row's payload
/// (or, for a payload-less row, copies its manifest-owned roles and reasons), plus one
/// freshness row per payload basis. The payload's rows are not read or written.
pub(crate) fn publish_copy(
    db: &Connection,
    key: &str,
    basis_tuple: i64,
    freshness: &BTreeMap<i64, sem::Freshness>,
) -> Result<()> {
    let copied = db
        .prepare_cached(
            "INSERT INTO semantic_coverage(revision_id,producer_id,producer_version,language,path,requested,selected,state,diagnostic,payload_id) \
             SELECT ?1,producer_id,producer_version,language,path,requested,selected,state,diagnostic,payload_id FROM semantic_coverage WHERE tuple_id=?2",
        )?
        .execute(params![key, basis_tuple])?;
    selected_ensure!(
        copied == 1,
        "incompatible_index: copied document lost its basis"
    );
    let tuple = db.last_insert_rowid();
    let payload: Option<i64> = db
        .prepare_cached("SELECT payload_id FROM semantic_coverage WHERE tuple_id=?1")?
        .query_row([tuple], |r| r.get(0))?;
    if payload.is_none() {
        db.prepare_cached(
            "INSERT INTO semantic_coverage_roles(payload_id,tuple_id,kind,ordinal,role) SELECT NULL,?1,kind,ordinal,role FROM semantic_coverage_roles WHERE tuple_id=?2",
        )?
        .execute(params![tuple, basis_tuple])?;
        db.prepare_cached(
            "INSERT INTO semantic_coverage_reasons(payload_id,tuple_id,macro_invocation,attribute_item,macro_definition,other,capped,omitted,captured_diagnostics) SELECT NULL,?1,macro_invocation,attribute_item,macro_definition,other,capped,omitted,captured_diagnostics FROM semantic_coverage_reasons WHERE tuple_id=?2",
        )?
        .execute(params![tuple, basis_tuple])?;
    }
    for (basis, freshness) in freshness {
        db.prepare_cached(
            "INSERT INTO semantic_coverage_freshness(revision_id,tuple_id,basis_id,freshness) VALUES(?1,?2,?3,?4)",
        )?
        .execute(params![key, tuple, basis, semantic_variant(freshness)?])?;
    }
    Ok(())
}

/// The publication S writes for a copied document, as the oracle compares it: the basis
/// payload read as evidence of the destination, with the freshness [`reuse_freshness`] gives.
/// This is the projection the committed manifest row will read back; only the oracle reads it.
#[cfg_attr(not(feature = "test-support"), allow(dead_code))]
pub(crate) fn copied_document(
    db: &Connection,
    target: &PendingSemanticTarget,
    selection: &PendingSemanticSelection,
    producer: &sem::Producer,
    destination: &CopyDestination,
    join_inputs: &str,
    document: &DocumentKey,
) -> Result<sem::SemanticPublication> {
    let (basis, tuple) = copy_source(
        db,
        target,
        &producer.id,
        selection.batch_id,
        join_inputs,
        document,
    )?
    .context(SelectedIntegrity(
        "incompatible_index: copied document lost its basis".into(),
    ))?;
    let payload: Option<i64> = db.query_row(
        "SELECT payload_id FROM semantic_coverage WHERE tuple_id=?1",
        [tuple],
        |row| row.get(0),
    )?;
    let freshness = match payload {
        Some(payload) => reuse_freshness(db, payload, producer, destination)?,
        None => BTreeMap::new(),
    };
    let source_set: String = db.query_row(
        "SELECT source_set_id FROM native_revisions WHERE id=?1",
        [&basis],
        |r| r.get(0),
    )?;
    let snapshot = read_semantic_tuple_with_reasons(
        db,
        &basis,
        Some(tuple),
        None,
        true,
        None,
        true,
        source_set,
        Some(ReadAs {
            native: &destination.native_revision_id,
            freshness: &freshness,
        }),
    )?;
    let [coverage] = snapshot.coverage.as_slice() else {
        return Err(
            SelectedIntegrity("incompatible_index: copied tuple coverage missing".into()).into(),
        );
    };
    selected_ensure!(
        coverage.producer_id == producer.id,
        "incompatible_index: copied tuple producer differs"
    );
    Ok(sem::SemanticPublication {
        producers: vec![producer.clone()],
        coverage: snapshot.coverage,
        provenance: snapshot.provenance,
        symbols: snapshot.symbols,
        declaration_bindings: snapshot.declaration_bindings,
        references: snapshot.references,
        call_bindings: snapshot.call_bindings,
        type_relationships: snapshot.type_relationships,
    })
}

/// The documents an admitted witnessed stage marked as copied, in canonical key order.
pub(crate) fn copied_documents(
    stage: &Connection,
    source_set_id: &str,
) -> Result<Vec<DocumentKey>> {
    Ok(stage
        .prepare(
            "SELECT s.language,s.path FROM source_witnesses w JOIN sources s ON s.ord=w.source_ord \
             WHERE w.decision='copied' ORDER BY s.language,s.path",
        )?
        .query_map([], |r| {
            Ok(DocumentKey {
                source_set_id: source_set_id.to_owned(),
                language: r.get(0)?,
                path: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}
