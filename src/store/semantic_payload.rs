//! Shared semantic payloads (#680, Decision 0021): the revision-neutral rows of one
//! `(producer, document)` tuple, written once and named by every revision's manifest row.
//!
//! A payload stores the writing revision's native revision as NULL. Reading it as evidence of
//! revision r goes through [`project_payload`], the single projection, which supplies r's
//! native revision, r's rejoined provenance IDs and r's freshness.
use super::*;
use crate::semantic_records as sem;
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

/// A targeted syntax ID's flags: a call binding's declared target is it ([`TARGET_CALL`]), and
/// an own symbol row declares in it ([`TARGET_DECLARATION`]).
pub(in crate::store) type TargetFlags = u8;
/// A call binding's declared target.
pub(in crate::store) const TARGET_CALL: TargetFlags = 1;
/// An own symbol row's declaration. Such a declaration in another document comes from the
/// destination row's discovery, which keeps it only while that document is unchanged since the
/// capture (#863).
pub(in crate::store) const TARGET_DECLARATION: TargetFlags = 2;

/// The syntax IDs of one other document that a payload's rows target, each with its flags.
pub(in crate::store) type DependencyTargets = BTreeMap<String, TargetFlags>;

/// The documents the tuple's rows name: the union of every internal endpoint, join anchor,
/// candidate and dispatch document, with the syntax IDs targeted in each. A change to any of
/// them can invalidate the payload; [`dependency_term`] says which.
pub(in crate::store) fn projection_dependencies(
    owns: &BTreeSet<&str>,
    publication: &sem::SemanticPublication,
) -> BTreeMap<(String, String), DependencyTargets> {
    let mut out: BTreeMap<(String, String), DependencyTargets> = BTreeMap::new();
    let mut target = |t: &sem::Target, flags: TargetFlags| {
        if let sem::Target::Internal {
            document,
            syntax_id,
            ..
        } = t
        {
            *out.entry((document.language.clone(), document.path.clone()))
                .or_default()
                .entry(syntax_id.clone())
                .or_default() |= flags;
        }
    };
    for symbol in publication
        .symbols
        .iter()
        .filter(|s| owns.contains(s.provenance_id.as_str()))
    {
        symbol
            .declarations
            .iter()
            .for_each(|t| target(t, TARGET_DECLARATION));
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
        reference.declared_target.iter().for_each(|t| target(t, 0));
        reference.candidates.iter().for_each(|t| target(t, 0));
    }
    for call in publication
        .call_bindings
        .iter()
        .filter(|b| owns.contains(b.provenance_id.as_str()))
    {
        call.declared_target
            .iter()
            .for_each(|t| target(t, TARGET_CALL));
        call.candidates.iter().for_each(|t| target(t, 0));
        call.possible_dispatch.iter().for_each(|t| target(t, 0));
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
        target(&relationship.source, 0);
        target(&relationship.target, 0);
    }
    for anchor in anchors {
        out.entry(anchor).or_default();
    }
    out
}

/// A dependency's recorded term and targets, as stored with its payload.
pub(in crate::store) struct DependencyRecord {
    pub language: String,
    pub path: String,
    /// `None` for the payload's own document and for a payload with no reuse key: a change to
    /// that document then always invalidates the payload.
    pub term: Option<String>,
    pub targets: Vec<(Vec<u8>, TargetFlags)>,
}

/// How many targeted syntax IDs one pending decision may look up; beyond it every remaining
/// dependent is conservatively pending.
pub(in crate::store) const TERM_LOOKUP_BOUND: usize = 200_000;

/// The dependency term of document `language`/`path` for a payload that targets `targets`
/// (syntax ID BLOB, [`TargetFlags`]), read at `revision` with `captured_pin` as the capture
/// (owner ruling 2026-11-09, Decision 0021 D4). Equal terms mean the payload's settled tuple is
/// equal, because the term holds everything the settlement reads of another document D:
///
/// - whether D is present (`retarget` keeps a target only when `doc_row(N, D)` exists);
/// - for each targeted syntax ID of D, the declaration status the settlement reads
///   (`declaration_status`: none, or named, name matches, method), which is read by `retarget`,
///   `check_row`'s `known` and `rejoined_dispatch`;
/// - only when a call binding's declared target is a method of D, whether D's content hash
///   equals its hash at the capture. The destination row's declared target is cleared for a
///   changed D (`staged/witness.rs` `reused_source`, FULL's `StagedSide::unchanged`), and only
///   `rejoined_dispatch` reads it, turning a captured `Direct` call on a method into `Unknown`.
///   It compares content hashes, not versions: a config-only change keeps the bytes and the
///   dispatch;
/// - the same bit when an own symbol row declares in D (#863). An own symbol with no captured
///   exact definition in its document takes the destination row's declarations, which FULL
///   discovers only over documents unchanged since the capture (`definitions_sql`). Such a
///   declaration needs D's own symbol information in another artifact (#473; symbol keys are
///   unique per artifact), and a multi-artifact batch always settles through FULL.
///
/// No byte offset, range or anchor of D is read or stored: anchors are own-document, targets
/// are `(syntax_id, document)` and syntax IDs survive comment and body edits. A future
/// settlement read of another document must extend this term. Captured-side reads and
/// external targets are constant for the batch and so for the key.
pub(in crate::store) fn dependency_term(
    db: &Connection,
    revision: &str,
    captured_pin: &str,
    language: &str,
    path: &str,
    targets: &[(Vec<u8>, TargetFlags)],
) -> Result<String> {
    let document = |revision: &str| -> Result<Option<(i64, String)>> {
        Ok(db
            .prepare_cached("SELECT d.vid,v.content_hash FROM revision_documents d JOIN document_versions v ON v.vid=d.vid WHERE d.revision_id=?1 AND d.language=?2 AND d.path=?3")?
            .query_row(params![revision, language, path], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    };
    let current = document(revision)?;
    let mut sensitive = false;
    let mut statuses = Vec::with_capacity(targets.len());
    for (syntax_id, flags) in targets {
        let status: Option<(bool, bool, bool)> = match &current {
            Some((vid, _)) => db
                .prepare_cached("SELECT name_start IS NOT NULL,name_start IS NOT NULL AND name IS NOT NULL,kind='method' FROM native_version_declarations WHERE vid=?1 AND syntax_id=?2")?
                .query_row(params![vid, syntax_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?,
            None => None,
        };
        sensitive |= flags & TARGET_DECLARATION != 0
            || (flags & TARGET_CALL != 0 && status.is_some_and(|(_, _, method)| method));
        statuses.push(serde_json::json!([
            super::id_codec::decode_sid(syntax_id)?,
            status
        ]));
    }
    let unchanged = match (&current, sensitive) {
        (Some((_, hash)), true) => Some(
            revision == captured_pin
                || document(captured_pin)?.is_some_and(|(_, captured)| captured == *hash),
        ),
        _ => None,
    };
    Ok(crate::native_ids::digest(
        b"trellis.semantic-dependency-term.v1\0",
        &crate::native_ids::canonical(&serde_json::json!({
            "language": language,
            "path": path,
            "present": current.is_some(),
            "targets": statuses,
            "unchanged": unchanged,
        })),
    ))
}

/// What a payload key hashes besides the document's own identity: the join-inputs digest
/// (the build's join and settle code plus its join limits; owner ruling 2026-11-09) and
/// whether a stored payload may be named at all.
#[derive(Clone)]
pub(in crate::store) struct KeyInputs {
    join_inputs: String,
    reuse: bool,
    /// Test seam: the store's FULL oracle checks every payload a write reuses.
    #[cfg(feature = "test-support")]
    check_hits: bool,
}

impl KeyInputs {
    /// The digest a keyed payload records, so a copy can tell which build wrote it.
    pub(in crate::store) fn join_inputs(&self) -> &str {
        &self.join_inputs
    }

    /// The running build's digest: the hash of its join modules (`build/semantic_join_inputs.rs`)
    /// and of the join `limits` this operation applies.
    pub(in crate::store) fn current(limits: &crate::semantic_batch::limits::BatchLimits) -> Self {
        Self {
            join_inputs: crate::native_ids::digest(
                b"trellis.semantic-join-inputs.v1\0",
                &crate::native_ids::canonical(&serde_json::json!({
                    "build": env!("TRELLIS_SEMANTIC_JOIN_INPUTS_HASH"),
                    "limits": format!("{limits:?}"),
                })),
            ),
            reuse: true,
            #[cfg(feature = "test-support")]
            check_hits: false,
        }
    }

    /// Test seam: a different digest, as another build of the join code would have.
    #[cfg(feature = "test-support")]
    pub(in crate::store) fn with_join_inputs(mut self, digest: &str) -> Self {
        self.join_inputs = digest.to_owned();
        self
    }

    /// Test seam: no payload is keyed, so none can be named by a later write.
    #[cfg(feature = "test-support")]
    pub(in crate::store) fn without_reuse(mut self) -> Self {
        self.reuse = false;
        self
    }

    /// Test seam: the FULL oracle checks each reused payload (`SemanticTestHooks::full_oracle`).
    #[cfg(feature = "test-support")]
    pub(in crate::store) fn checking_hits(mut self, check: bool) -> Self {
        self.check_hits = check;
        self
    }

    #[cfg(feature = "test-support")]
    pub(in crate::store) fn check_hits(&self) -> bool {
        self.check_hits
    }
}

/// The input key (B): equal keys name equal payloads. `terms` are the sorted dependency terms
/// of the payload's other documents; its own document enters by `version`. `None` when the
/// inputs disable reuse.
#[allow(clippy::too_many_arguments)]
pub(in crate::store) fn payload_key(
    inputs: &KeyInputs,
    producer: &sem::Producer,
    descriptor_sha256: &str,
    captured_revision: &str,
    language: &str,
    path: &str,
    version: &str,
    terms: &[(String, String, String)],
) -> Option<String> {
    if !inputs.reuse {
        return None;
    }
    Some(crate::native_ids::digest(
        b"trellis.semantic-payload.v1\0",
        &crate::native_ids::canonical(&serde_json::json!({
            "joinInputs": inputs.join_inputs,
            "producer": {"id": producer.id, "version": producer.version, "executableHash": producer.executable_hash},
            "batch": {"descriptorSha256": descriptor_sha256, "capturedRevision": captured_revision},
            "document": {"language": language, "path": path, "version": version},
            "dependencies": terms.iter().map(|(l, p, t)| serde_json::json!({"language": l, "path": p, "term": t})).collect::<Vec<_>>(),
        })),
    ))
}

/// Where a projection takes provenance freshness from.
pub(crate) enum FreshnessSource<'a> {
    /// The freshness rows stored for the reading revision.
    Stored,
    /// Freshness a publication supplies before its rows are stored.
    Publication(&'a BTreeMap<i64, sem::Freshness>),
}

/// What reading a stored target needs from the reading revision.
#[derive(Clone, Copy)]
pub(in crate::store) struct TargetScope<'a> {
    pub source_set: &'a str,
    pub native: &'a str,
}

/// How one payload is read as evidence of one revision (C).
pub(crate) struct PayloadProjection<'a> {
    db: &'a Connection,
    native: String,
    pub(in crate::store) source_set: String,
    pub(crate) payload_id: Option<i64>,
    freshness: BTreeMap<i64, sem::Freshness>,
    ids: RefCell<BTreeMap<i64, String>>,
}

/// The projection of the payload named by manifest row `tuple_id` of `revision_key`.
pub(crate) fn project_payload<'a>(
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
    /// Reads the payload as evidence of the revision whose native revision is `native`.
    pub(in crate::store) fn at_native(mut self, native: &str) -> Self {
        self.native = native.to_owned();
        self
    }

    pub(in crate::store) fn scope(&self) -> TargetScope<'_> {
        TargetScope {
            source_set: &self.source_set,
            native: &self.native,
        }
    }

    /// The reading revision's native revision.
    pub(crate) fn native(&self) -> &str {
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
    const TAIL: [(&str, &str); 9] = [
        ("semantic_provenance", "payload"),
        (
            "semantic_basis_lookup_dependencies",
            "basis_id IN (SELECT basis_id FROM semantic_basis WHERE payload_id=?1)",
        ),
        ("semantic_basis", "payload"),
        ("semantic_projection_dependencies", "payload"),
        ("semantic_projection_dependency_targets", "payload"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_records::{DocumentKey, Range};

    fn document(path: &str) -> DocumentKey {
        DocumentKey {
            source_set_id: "set".into(),
            language: "javascript".into(),
            path: path.into(),
        }
    }

    fn internal(path: &str) -> sem::Target {
        sem::Target::Internal {
            syntax_id: format!("sid:v1:{}", "1".repeat(32)),
            document: document(path),
            revision_id: "native".into(),
        }
    }

    fn anchor(path: &str, kind: sem::JoinKind) -> sem::Join {
        sem::Join {
            anchor: sem::MeasuredAnchor {
                document: document(path),
                revision_id: "native".into(),
                content_hash: "h".into(),
                range: Range { start: 0, end: 1 },
                kind,
            },
            status: sem::JoinStatus::Exact,
            candidate_ids: vec![],
            diagnostic: None,
        }
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn keys_differing_only_in_the_join_inputs_digest_differ() {
        let producer = sem::Producer {
            id: "producer".into(),
            version: "1".into(),
            executable_hash: "a".repeat(64),
            kind: sem::SemanticKind::Semantic,
            languages: vec![sem::Language::Javascript],
            position_encoding: sem::PositionEncoding::Utf8,
        };
        let key = |inputs: &KeyInputs| {
            payload_key(inputs, &producer, "d", "c", "javascript", "a.js", "v", &[])
        };
        let current = KeyInputs::current(&crate::semantic_batch::limits::BatchLimits::PRODUCTION);
        assert_eq!(
            key(&current),
            key(&KeyInputs::current(
                &crate::semantic_batch::limits::BatchLimits::PRODUCTION
            ))
        );
        assert_ne!(
            key(&current),
            key(&current.clone().with_join_inputs("other"))
        );
        assert_eq!(key(&current.without_reuse()), None);
    }

    /// A publication whose one document names a different document through every kind of
    /// endpoint the projection depends on.
    fn every_endpoint() -> (sem::Coverage, sem::SemanticPublication) {
        let key = |symbol: &str| sem::SymbolKey {
            scheme: sem::SymbolScheme::Scip,
            symbol: symbol.into(),
            scope: sem::SymbolScope::Global,
            document: None,
        };
        let coverage = sem::Coverage {
            producer_id: "producer".into(),
            language: sem::Language::Javascript,
            source_set_id: "set".into(),
            document_path: "main.js".into(),
            revision_id: "native".into(),
            requested: true,
            selected: true,
            state: sem::CoverageState::Complete,
            supported_roles: vec![],
            observed_roles: vec![],
            diagnostic: None,
            reasons: None,
        };
        let provenance = |id: &str| sem::Provenance {
            id: id.into(),
            producer_id: "producer".into(),
            document: document("main.js"),
            revision_id: "native".into(),
            content_hash: "h".into(),
            evidence_kind: sem::EvidenceKind::MeasuredSyntax,
            basis: Some(sem::SemanticBasis {
                producer_id: "producer".into(),
                producer_version: "1".into(),
                producer_hash: "a".repeat(64),
                artifact_hash: "b".repeat(64),
                language: sem::Language::Javascript,
                source_set_id: "set".into(),
                revision_id: "native".into(),
                source_manifest_hash: "c".repeat(64),
                toolchain_hash: "d".repeat(64),
                config_hash: "e".repeat(64),
                dependency_hash: "f".repeat(64),
                lookup_dependencies: vec![],
            }),
            freshness: sem::Freshness::Fresh,
            derived_from: None,
        };
        let publication = sem::SemanticPublication {
            producers: vec![],
            coverage: vec![coverage.clone()],
            provenance: vec![provenance("p")],
            symbols: vec![sem::Symbol {
                key: key("s"),
                display_name: None,
                declarations: vec![internal("symbol-target.js")],
                provenance_id: "p".into(),
            }],
            declaration_bindings: vec![sem::DeclarationBinding {
                syntax_id: None,
                symbols: vec![],
                join: anchor("declaration-anchor.js", sem::JoinKind::DeclarationName),
                provenance_id: "p".into(),
            }],
            references: vec![sem::Reference {
                id: "occ:v1:00".into(),
                owner_syntax_id: "sid:v1:00".into(),
                ordinal: 0,
                document: document("main.js"),
                revision_id: "native".into(),
                range: Range { start: 0, end: 1 },
                spelling: "x".into(),
                lookup_key: "x".into(),
                site: sem::ReferenceSite::Use,
                roles: vec![],
                resolution: sem::Resolution::Resolved,
                declared_target: Some(internal("reference-declared.js")),
                candidates: vec![internal("reference-candidate.js")],
                provenance_id: "p".into(),
            }],
            call_bindings: vec![sem::CallBinding {
                call_id: None,
                join: anchor("call-anchor.js", sem::JoinKind::Callee),
                resolution: sem::Resolution::Resolved,
                declared_target: Some(internal("call-declared.js")),
                candidates: vec![internal("call-candidate.js")],
                dispatch: sem::Dispatch::Direct,
                possible_dispatch: vec![internal("call-dispatch.js")],
                possible_dispatch_complete: false,
                stale_target: None,
                provenance_id: "p".into(),
            }],
            type_relationships: vec![sem::TypeRelationship {
                kind: sem::RelationshipKind::Extends,
                source: internal("type-source.js"),
                target: internal("type-target.js"),
                provenance_id: "p".into(),
            }],
        };
        (coverage, publication)
    }

    #[test]
    fn projection_dependencies_covers_every_endpoint_kind() {
        let (coverage, publication) = every_endpoint();
        let owns = owned_provenance(&coverage, &publication);
        let paths: Vec<String> = projection_dependencies(&owns, &publication)
            .into_iter()
            .map(|((_, path), _)| path)
            .collect();
        assert_eq!(
            paths,
            [
                "call-anchor.js",
                "call-candidate.js",
                "call-declared.js",
                "call-dispatch.js",
                "declaration-anchor.js",
                "reference-candidate.js",
                "reference-declared.js",
                "symbol-target.js",
                "type-source.js",
                "type-target.js",
            ]
        );
    }

    #[test]
    fn a_fact_of_another_tuple_adds_no_dependency() {
        let (coverage, mut publication) = every_endpoint();
        publication.symbols[0].provenance_id = "other".into();
        let owns = owned_provenance(&coverage, &publication);
        assert!(
            !projection_dependencies(&owns, &publication)
                .keys()
                .any(|(_, path)| path == "symbol-target.js")
        );
    }

    #[test]
    fn writer_rejects_dependency_endpoint_missing_from_revision() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(CACHE_SCHEMA_V43).unwrap();
        db.pragma_update(None, "foreign_keys", "OFF").unwrap();
        db.execute("INSERT INTO native_revisions VALUES('key','set',?1,?1,?1,'native','[]','[]','{}','[]','[]',NULL,'[]',0,'{}','[]',1)", ["a".repeat(64)]).unwrap();
        let (_, mut publication) = every_endpoint();
        publication.producers.push(sem::Producer {
            id: "producer".into(),
            version: "1".into(),
            executable_hash: "a".repeat(64),
            kind: sem::SemanticKind::Semantic,
            languages: vec![sem::Language::Javascript],
            position_encoding: sem::PositionEncoding::Utf8,
        });
        // The document itself is in the revision; its dependencies are not.
        db.execute("INSERT INTO document_versions(vid,id,language,path,content_hash,extraction_context,producer_id,producer_version,byte_length,source_bytes,native_witness) VALUES(1,'v','javascript','main.js',?1,?1,'p','1',0,x'',?1)", ["b".repeat(64)]).unwrap();
        db.execute("INSERT INTO revision_documents VALUES('key','javascript','main.js',1,1,NULL,NULL,1,1,'complete',NULL,0,1)", []).unwrap();
        let error = write_import_coverage(
            &db,
            "key",
            &publication,
            None,
            &KeyInputs::current(&crate::semantic_batch::limits::BatchLimits::PRODUCTION),
        )
        .unwrap_err();
        assert!(
            error.is::<SelectedIntegrity>()
                && format!("{error:#}").contains("semantic dependency endpoint missing"),
            "{error:#}"
        );
    }

    /// A payload written without a batch has no reuse key and so no term: any change to a
    /// document it names makes it pending.
    #[test]
    fn unbatched_payload_records_null_terms_and_no_targets() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(CACHE_SCHEMA_V43).unwrap();
        db.pragma_update(None, "foreign_keys", "OFF").unwrap();
        db.execute("INSERT INTO native_revisions VALUES('key','set',?1,?1,?1,'native','[]','[]','{}','[]','[]',NULL,'[]',0,'{}','[]',1)", ["a".repeat(64)]).unwrap();
        let (_, mut publication) = every_endpoint();
        publication.producers.push(sem::Producer {
            id: "producer".into(),
            version: "1".into(),
            executable_hash: "a".repeat(64),
            kind: sem::SemanticKind::Semantic,
            languages: vec![sem::Language::Javascript],
            position_encoding: sem::PositionEncoding::Utf8,
        });
        publication.references.clear();
        let paths: BTreeSet<String> = projection_dependencies(
            &owned_provenance(&publication.coverage[0], &publication),
            &publication,
        )
        .into_keys()
        .map(|(_, path)| path)
        .chain(["main.js".to_owned()])
        .collect();
        for (vid, path) in paths.iter().enumerate() {
            let vid = vid as i64 + 1;
            db.execute("INSERT INTO document_versions(vid,id,language,path,content_hash,extraction_context,producer_id,producer_version,byte_length,source_bytes,native_witness) VALUES(?1,?2,'javascript',?3,?4,?4,'p','1',0,x'',?4)", params![vid, format!("v{vid}"), path, format!("{vid:064}")]).unwrap();
            db.execute("INSERT INTO revision_documents VALUES('key','javascript',?1,?2,1,NULL,NULL,1,1,'complete',NULL,?2,1)", params![path, vid]).unwrap();
        }
        write_import_coverage(
            &db,
            "key",
            &publication,
            None,
            &KeyInputs::current(&crate::semantic_batch::limits::BatchLimits::PRODUCTION),
        )
        .unwrap();
        let (rows, terms, targets): (i64, i64, i64) = db
            .query_row(
                "SELECT (SELECT COUNT(*) FROM semantic_projection_dependencies),(SELECT COUNT(term) FROM semantic_projection_dependencies),(SELECT COUNT(*) FROM semantic_projection_dependency_targets)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((rows, terms, targets), (8, 0, 0));
    }

    fn payload_with_children(db: &Connection, key: Option<&str>, symbols: usize) {
        db.execute_batch(CACHE_SCHEMA_V43).unwrap();
        db.pragma_update(None, "foreign_keys", "OFF").unwrap();
        db.execute(
            "INSERT INTO semantic_payloads(payload_id,payload_key,producer_id,producer_version,language,path,version_id,batch_id) VALUES(1,?1,'p','1','javascript','a.js','v',1)",
            [key],
        )
        .unwrap();
        db.execute(
            "INSERT INTO semantic_release_candidate_payloads VALUES('rev',1)",
            [],
        )
        .unwrap();
        for ordinal in 0..symbols {
            db.execute(
                "INSERT INTO semantic_symbols VALUES(NULL,1,?1,'scip',?2,'global',NULL,NULL,NULL,0)",
                params![ordinal as i64, format!("s{ordinal}")],
            )
            .unwrap();
        }
    }

    #[test]
    fn collection_clears_key_before_children() {
        let db = Connection::open_in_memory().unwrap();
        payload_with_children(&db, Some(&"a".repeat(64)), 600);
        assert!(cleanup_payload_candidates(&db, "rev").unwrap());
        let key: Option<String> = db
            .query_row("SELECT payload_key FROM semantic_payloads", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            key, None,
            "no writer hit can reach a payload being collected"
        );
        let left: i64 = db
            .query_row("SELECT COUNT(*) FROM semantic_symbols", [], |r| r.get(0))
            .unwrap();
        assert!(left > 0, "the children go in later chunks");
    }

    #[test]
    fn last_release_collects_payload_in_chunks() {
        let db = Connection::open_in_memory().unwrap();
        payload_with_children(&db, Some(&"a".repeat(64)), 600);
        let mut calls = 0;
        while cleanup_payload_candidates(&db, "rev").unwrap() {
            calls += 1;
            assert!(calls < 20);
        }
        assert!(
            calls >= 3,
            "600 children take at least three 256-row chunks"
        );
        for table in [
            "semantic_payloads",
            "semantic_symbols",
            "semantic_release_candidate_payloads",
        ] {
            let rows: i64 = db
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(rows, 0, "{table}");
        }
    }

    #[test]
    fn a_payload_a_manifest_row_still_names_is_only_unjournaled() {
        let db = Connection::open_in_memory().unwrap();
        payload_with_children(&db, Some(&"a".repeat(64)), 3);
        db.execute("INSERT INTO semantic_coverage VALUES(1,'other','p','1','javascript','a.js',1,1,'complete',NULL,1)", []).unwrap();
        cleanup_payload_candidates(&db, "rev").unwrap();
        let (payloads, symbols, key): (i64, i64, Option<String>) = db
            .query_row(
                "SELECT (SELECT COUNT(*) FROM semantic_payloads),(SELECT COUNT(*) FROM semantic_symbols),(SELECT payload_key FROM semantic_payloads)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((payloads, symbols), (1, 3));
        assert!(key.is_some(), "a named payload keeps its reuse key");
        let journal: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM semantic_release_candidate_payloads",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(journal, 0);
    }
}
