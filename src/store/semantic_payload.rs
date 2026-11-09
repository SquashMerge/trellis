//! Shared semantic payloads (#680, Decision 0021): the revision-neutral rows of one
//! `(producer, document)` tuple, written once and named by every revision's manifest row.
//!
//! A payload stores the writing revision's native revision as NULL. Reading it as evidence of
//! revision r goes through [`project_payload`], the single projection, which supplies r's
//! native revision, r's rejoined provenance IDs and r's freshness.
use super::*;
use crate::semantic_evidence as sem;
use std::cell::RefCell;

/// The provenance IDs of `coverage`'s own tuple: the rows the tuple owns in `publication`.
pub(in crate::store) fn owned_provenance<'a>(
    coverage: &sem::Coverage,
    publication: &'a sem::SemanticPublication,
) -> BTreeSet<&'a str> {
    let language = semantic_variant(&coverage.language).unwrap_or_default();
    publication
        .provenance
        .iter()
        .filter(|p| {
            p.producer_id == coverage.producer_id
                && p.document.language == language
                && p.document.path == coverage.document_path
        })
        .map(|p| p.id.as_str())
        .collect()
}

/// The documents the tuple's rows name: the union of every internal endpoint, join anchor,
/// candidate and dispatch document. A change to any of them invalidates the payload.
pub(in crate::store) fn projection_dependencies(
    owns: &BTreeSet<&str>,
    publication: &sem::SemanticPublication,
) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    let mut target = |t: &sem::Target| {
        if let sem::Target::Internal { document, .. } = t {
            out.insert((document.language.clone(), document.path.clone()));
        }
    };
    for symbol in publication
        .symbols
        .iter()
        .filter(|s| owns.contains(s.provenance_id.as_str()))
    {
        symbol.declarations.iter().for_each(&mut target);
    }
    let mut anchors = Vec::new();
    for binding in publication
        .declaration_bindings
        .iter()
        .filter(|b| owns.contains(b.provenance_id.as_str()))
    {
        anchors.push((
            binding.join.anchor.document.language.clone(),
            binding.join.anchor.document.path.clone(),
        ));
    }
    for reference in publication
        .references
        .iter()
        .filter(|r| owns.contains(r.provenance_id.as_str()))
    {
        reference.declared_target.iter().for_each(&mut target);
        reference.candidates.iter().for_each(&mut target);
    }
    for call in publication
        .call_bindings
        .iter()
        .filter(|b| owns.contains(b.provenance_id.as_str()))
    {
        call.declared_target.iter().for_each(&mut target);
        call.candidates.iter().for_each(&mut target);
        call.possible_dispatch.iter().for_each(&mut target);
        anchors.push((
            call.join.anchor.document.language.clone(),
            call.join.anchor.document.path.clone(),
        ));
    }
    for relationship in publication
        .type_relationships
        .iter()
        .filter(|r| owns.contains(r.provenance_id.as_str()))
    {
        target(&relationship.source);
        target(&relationship.target);
    }
    out.extend(anchors);
    out
}

/// The input key (B): equal keys name equal payloads.
pub(in crate::store) fn payload_key(
    producer: &sem::Producer,
    descriptor_sha256: &str,
    captured_revision: &str,
    language: &str,
    path: &str,
    version: &str,
    dependencies: &[(String, String, String)],
) -> String {
    crate::native_ids::digest(
        b"trellis.semantic-payload.v1\0",
        &crate::native_ids::canonical(&serde_json::json!({
            "producer": {"id": producer.id, "version": producer.version, "executableHash": producer.executable_hash},
            "batch": {"descriptorSha256": descriptor_sha256, "capturedRevision": captured_revision},
            "document": {"language": language, "path": path, "version": version},
            "dependencies": dependencies.iter().map(|(l, p, v)| serde_json::json!({"language": l, "path": p, "version": v})).collect::<Vec<_>>(),
        })),
    )
}

/// Where a projection takes provenance freshness from.
pub(in crate::store) enum FreshnessSource<'a> {
    /// The freshness rows stored for the reading revision.
    Stored,
    /// Freshness a publication supplies before its rows are stored.
    #[allow(dead_code)]
    Publication(&'a BTreeMap<i64, sem::Freshness>),
}

/// What reading a stored target needs from the reading revision.
#[derive(Clone, Copy)]
pub(in crate::store) struct TargetScope<'a> {
    pub source_set: &'a str,
    pub native: &'a str,
}

/// How one payload is read as evidence of one revision (C).
pub(in crate::store) struct PayloadProjection<'a> {
    db: &'a Connection,
    native: String,
    pub(in crate::store) source_set: String,
    pub(in crate::store) payload_id: Option<i64>,
    freshness: BTreeMap<i64, sem::Freshness>,
    ids: RefCell<BTreeMap<i64, String>>,
}

/// The projection of the payload named by manifest row `tuple_id` of `revision_key`.
pub(in crate::store) fn project_payload<'a>(
    db: &'a Connection,
    revision_key: &str,
    tuple_id: i64,
    freshness: FreshnessSource<'a>,
) -> Result<PayloadProjection<'a>> {
    let (source_set, native): (String, String) = db
        .prepare_cached(
            "SELECT source_set_id,native_revision_id FROM native_revisions WHERE id=?1",
        )?
        .query_row([revision_key], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let payload_id: Option<i64> = db
        .prepare_cached(
            "SELECT payload_id FROM semantic_coverage WHERE tuple_id=?1 AND revision_id=?2",
        )?
        .query_row(params![tuple_id, revision_key], |r| r.get(0))?;
    let freshness = match freshness {
        FreshnessSource::Publication(map) => map.clone(),
        FreshnessSource::Stored => {
            let mut rows = db.prepare_cached(
                "SELECT basis_id,freshness FROM semantic_coverage_freshness WHERE revision_id=?1 AND tuple_id=?2",
            )?;
            let rows = rows
                .query_map(params![revision_key, tuple_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut map = BTreeMap::new();
            for (basis, stored) in rows {
                map.insert(basis, semantic_enum(stored)?);
            }
            map
        }
    };
    Ok(PayloadProjection {
        db,
        native,
        source_set,
        payload_id,
        freshness,
        ids: RefCell::new(BTreeMap::new()),
    })
}

impl PayloadProjection<'_> {
    pub(in crate::store) fn scope(&self) -> TargetScope<'_> {
        TargetScope {
            source_set: &self.source_set,
            native: &self.native,
        }
    }

    /// The reading revision's native revision.
    pub(in crate::store) fn native(&self) -> &str {
        &self.native
    }

    /// A stored native-revision column: NULL means the reading revision's.
    pub(in crate::store) fn revision(&self, stored: Option<String>) -> String {
        stored.unwrap_or_else(|| self.native.clone())
    }

    /// The reading revision's provenance ID for the row at `ordinal`.
    pub(in crate::store) fn provenance_id(&self, ordinal: i64) -> Result<String> {
        if let Some(id) = self.ids.borrow().get(&ordinal) {
            return Ok(id.clone());
        }
        let payload = self.payload_id.context(SelectedIntegrity(
            "incompatible_index: payload-less tuple names provenance".into(),
        ))?;
        let row: Option<(Option<String>, Option<String>, String)> = self
            .db
            .prepare_cached(
                "SELECT provenance_id,derived_from_provenance_id,evidence_kind FROM semantic_provenance WHERE payload_id=?1 AND ordinal=?2",
            )?
            .query_row(params![payload, ordinal], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        let (literal, derived, kind) = row.context(SelectedIntegrity(
            "incompatible_index: semantic fact without provenance".into(),
        ))?;
        let id = match (literal, derived) {
            (Some(id), None) => id,
            (None, Some(raw)) => crate::semantic_import::rejoin_provenance_id(
                &raw,
                &self.native,
                semantic_enum(kind)?,
            ),
            _ => {
                return Err(SelectedIntegrity(
                    "incompatible_index: corrupt semantic provenance ID".into(),
                )
                .into());
            }
        };
        self.ids.borrow_mut().insert(ordinal, id.clone());
        Ok(id)
    }

    /// The reading revision's freshness for one basis of the payload.
    pub(in crate::store) fn freshness(&self, basis_id: i64) -> Result<sem::Freshness> {
        self.freshness
            .get(&basis_id)
            .copied()
            .context(SelectedIntegrity(
                "incompatible_index: semantic freshness missing".into(),
            ))
    }
}

/// Collects up to 64 journaled payloads of `revision` that no manifest row names. Returns
/// whether it made progress, which means the caller's unit continues.
pub(in crate::store) fn cleanup_payload_candidates(
    tx: &Connection,
    revision: &str,
) -> Result<bool> {
    const CHILDREN: [(&str, &str); 13] = [
        ("semantic_reference_roles", "payload"),
        ("semantic_reference_candidates", "payload"),
        ("semantic_references", "payload"),
        (
            "semantic_symbol_targets",
            "symbol_id IN (SELECT symbol_id FROM semantic_symbols WHERE payload_id=?1)",
        ),
        (
            "semantic_declaration_binding_symbols",
            "binding_id IN (SELECT binding_id FROM semantic_declaration_bindings WHERE payload_id=?1)",
        ),
        (
            "semantic_declaration_join_candidates",
            "binding_id IN (SELECT binding_id FROM semantic_declaration_bindings WHERE payload_id=?1)",
        ),
        (
            "semantic_call_candidates",
            "binding_id IN (SELECT binding_id FROM semantic_call_bindings WHERE payload_id=?1)",
        ),
        (
            "semantic_call_possible_dispatch",
            "binding_id IN (SELECT binding_id FROM semantic_call_bindings WHERE payload_id=?1)",
        ),
        (
            "semantic_call_join_candidates",
            "binding_id IN (SELECT binding_id FROM semantic_call_bindings WHERE payload_id=?1)",
        ),
        ("semantic_type_relationships", "payload"),
        ("semantic_symbols", "payload"),
        ("semantic_declaration_bindings", "payload"),
        ("semantic_call_bindings", "payload"),
    ];
    const TAIL: [(&str, &str); 8] = [
        ("semantic_provenance", "payload"),
        (
            "semantic_basis_lookup_dependencies",
            "basis_id IN (SELECT basis_id FROM semantic_basis WHERE payload_id=?1)",
        ),
        ("semantic_basis", "payload"),
        ("semantic_projection_dependencies", "payload"),
        ("semantic_projection_lookup_dependencies", "payload"),
        ("semantic_projection_contexts", "payload"),
        ("semantic_coverage_roles", "payload"),
        ("semantic_coverage_reasons", "payload"),
    ];
    let mut progress = false;
    for _ in 0..64 {
        let candidate: Option<i64> = tx
            .query_row(
                "SELECT payload_id FROM semantic_release_candidate_payloads WHERE revision_id=?1 ORDER BY payload_id LIMIT 1",
                [revision],
                |r| r.get(0),
            )
            .optional()?;
        let Some(payload) = candidate else {
            return Ok(progress);
        };
        progress = true;
        let named: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM semantic_coverage WHERE payload_id=?1)",
            [payload],
            |r| r.get(0),
        )?;
        if !named {
            // The key goes first: once collection starts no writer hit can reach the payload.
            tx.execute(
                "UPDATE semantic_payloads SET payload_key=NULL WHERE payload_id=?1 AND payload_key IS NOT NULL",
                [payload],
            )?;
            for (table, filter) in CHILDREN.iter().chain(TAIL.iter()) {
                let filter = if *filter == "payload" {
                    "payload_id=?1"
                } else {
                    filter
                };
                let deleted = tx.execute(
                    &format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE {filter} LIMIT 256)"),
                    [payload],
                )?;
                if deleted == 256 {
                    return Ok(true);
                }
            }
            tx.execute(
                "DELETE FROM semantic_payloads WHERE payload_id=?1",
                [payload],
            )?;
        }
        tx.execute(
            "DELETE FROM semantic_release_candidate_payloads WHERE revision_id=?1 AND payload_id=?2",
            params![revision, payload],
        )?;
    }
    Ok(progress)
}
