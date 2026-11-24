//! Shared semantic payloads (#680, Decision 0021): the revision-neutral rows of one
//! `(producer, document)` tuple, written once and named by every revision's manifest row.
//!
//! A payload stores the writing revision's native revision as NULL. Reading it as evidence of
//! revision r goes through [`project_payload`], the single projection, which supplies r's
//! native revision, r's rejoined provenance IDs, r's freshness and, for a capture-bound call,
//! r's dispatch (#1279, Decision 0021 D1 as amended 2026-11-21).
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
/// A call binding's declared target. Its dependency term reads the target's declaration status,
/// method included; whether the document changed since the capture is read at read time
/// instead, for a capture-bound call ([`capture_bound`], #1279).
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
    /// The document's content hash at the batch's capture, recorded only when the payload has a
    /// capture-bound call into it ([`capture_bound`]); `None` there means it was absent at the
    /// capture, so it is never unchanged.
    pub captured_hash: Option<String>,
}

/// Whether a batch-written call binding of the tuple of `own_language`/`own_path` is
/// capture-bound (#1279): a `direct` call whose declared target is a `method` declaration of
/// another document D at `revision`. Its dispatch then holds only while D's content is unchanged
/// since the capture: #557 makes it `unknown` otherwise, because the facts behind a method's
/// `direct` (its `impl`'s bases, its modifiers) are outside its syntax ID. The payload keeps the
/// `direct`; every read applies the demotion ([`PayloadProjection::dispatch`]). D's term records
/// the target's declaration status, so the method status read here holds at every revision that
/// names the payload. A call into its own document is never bound: a settled document is
/// unchanged since the capture.
pub(in crate::store) fn capture_bound(
    db: &Connection,
    revision: &str,
    own_language: &str,
    own_path: &str,
    binding: &sem::CallBinding,
) -> Result<bool> {
    let (
        sem::Dispatch::Direct,
        Some(sem::Target::Internal {
            syntax_id,
            document,
            ..
        }),
    ) = (binding.dispatch, &binding.declared_target)
    else {
        return Ok(false);
    };
    if document.language == own_language && document.path == own_path {
        return Ok(false);
    }
    let syntax_id =
        super::id_codec::encode_sid(syntax_id).context("invalid semantic call target ID")?;
    Ok(db
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM revision_documents d JOIN native_version_declarations v ON v.vid=d.vid WHERE d.revision_id=?1 AND d.language=?2 AND d.path=?3 AND v.syntax_id=?4 AND v.kind='method')",
        )?
        .query_row(
            params![revision, document.language, document.path, syntax_id],
            |r| r.get(0),
        )?)
}

/// The content hash of `language`/`path` at `revision`, if the revision has the document.
pub(in crate::store) fn content_hash_at(
    db: &Connection,
    revision: &str,
    language: &str,
    path: &str,
) -> Result<Option<String>> {
    Ok(db
        .prepare_cached(
            "SELECT v.content_hash FROM revision_documents d JOIN document_versions v ON v.vid=d.vid WHERE d.revision_id=?1 AND d.language=?2 AND d.path=?3",
        )?
        .query_row(params![revision, language, path], |r| r.get(0))
        .optional()?)
}

/// The capture-bound dispatch of a stored call binding row aliased `binding`, read at the
/// revision key bound to SQL parameter `revision`, as an SQL expression: the stored dispatch
/// while the target document's content hash equals the hash recorded at the capture, else
/// `unknown`. The same rule as [`PayloadProjection::dispatch`], for a reader that sizes rows
/// in SQL before decoding them.
pub(crate) fn projected_dispatch_sql(binding: &str, revision: &str) -> String {
    format!(
        "CASE WHEN {binding}.capture_bound=0 OR EXISTS(SELECT 1 FROM semantic_projection_dependencies e \
         JOIN revision_documents d ON d.revision_id={revision} AND d.language=e.language AND d.path=e.path \
         JOIN document_versions v ON v.vid=d.vid AND v.content_hash=e.captured_hash \
         WHERE e.payload_id={binding}.payload_id AND e.language={binding}.declared_internal_language \
         AND e.path={binding}.declared_internal_path) THEN {binding}.dispatch ELSE 'unknown' END"
    )
}

/// Applies the read-time demotion to a settled, not yet stored `publication` of batch-captured
/// revision `captured`, as reading it back at `revision` would (test-support: the FULL oracle
/// compares a staged settlement with the reader-visible in-memory rejoin).
#[cfg(feature = "test-support")]
pub(in crate::store) fn project_publication(
    db: &Connection,
    revision: &str,
    captured: &str,
    publication: &mut sem::SemanticPublication,
) -> Result<()> {
    let mut unchanged = BTreeMap::<(String, String), bool>::new();
    for call in &mut publication.call_bindings {
        let own = &call.join.anchor.document;
        if !capture_bound(db, revision, &own.language, &own.path, call)? {
            continue;
        }
        let Some(sem::Target::Internal { document, .. }) = &call.declared_target else {
            continue;
        };
        let key = (document.language.clone(), document.path.clone());
        let holds = match unchanged.get(&key) {
            Some(holds) => *holds,
            None => {
                let at = |revision| content_hash_at(db, revision, &key.0, &key.1);
                let holds = matches!((at(revision)?, at(captured)?), (Some(a), Some(b)) if a == b);
                unchanged.insert(key, holds);
                holds
            }
        };
        if !holds {
            call.dispatch = sem::Dispatch::Unknown;
        }
    }
    Ok(())
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
/// - only when an own symbol row declares in D (#863), whether D's content hash equals its hash
///   at the capture. An own symbol with no captured exact definition in its document takes the
///   destination row's declarations, which FULL discovers only over documents unchanged since
///   the capture (`definitions_sql`). Such a declaration needs D's own symbol information in
///   another artifact (#473; symbol keys are unique per artifact), and a multi-artifact batch
///   always settles through FULL. It compares content hashes, not versions: a config-only
///   change keeps the bytes and the declaration.
///
/// A call bound to a method of D also depends on D being unchanged since the capture (#557):
/// the destination row's declared target is cleared for a changed D (`staged/witness.rs`
/// `reused_source`, FULL's `StagedSide::unchanged`), and `rejoined_dispatch` then turns a
/// captured `Direct` call on a method into `Unknown`. That is not in the term (Decision 0021 D4
/// as amended 2026-11-21, #1279): the settlement keeps the `Direct`, the writer marks the row
/// capture-bound ([`capture_bound`]) and every read applies the demotion while D differs from
/// the capture ([`PayloadProjection::dispatch`]), so a first edit of D or its revert leaves the
/// dependent's payload in place.
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
        sensitive |= flags & TARGET_DECLARATION != 0;
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
    /// The reading revision's key, whose documents decide capture-bound dispatch.
    revision: String,
    native: String,
    pub(in crate::store) source_set: String,
    pub(crate) payload_id: Option<i64>,
    freshness: BTreeMap<i64, sem::Freshness>,
    ids: RefCell<BTreeMap<i64, (String, sem::ProofIdentity)>>,
    /// Whether each target document of a capture-bound call is unchanged since the capture at
    /// the reading revision: one lookup per document per read.
    unchanged: RefCell<BTreeMap<(String, String), bool>>,
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
        revision: revision_key.to_owned(),
        native,
        source_set,
        payload_id,
        freshness,
        ids: RefCell::new(BTreeMap::new()),
        unchanged: RefCell::new(BTreeMap::new()),
    })
}

impl PayloadProjection<'_> {
    /// Reads the payload as evidence of revision `revision`, whose native revision is `native`.
    pub(in crate::store) fn read_as(mut self, revision: &str, native: &str) -> Self {
        self.revision = revision.to_owned();
        self.native = native.to_owned();
        self
    }

    /// Whether `language`/`path`, the target document of a capture-bound call, has at the
    /// reading revision the content hash it had at the payload's capture.
    pub(in crate::store) fn unchanged_since_capture(
        &self,
        language: &str,
        path: &str,
    ) -> Result<bool> {
        let key = (language.to_owned(), path.to_owned());
        if let Some(unchanged) = self.unchanged.borrow().get(&key) {
            return Ok(*unchanged);
        }
        let payload = self.payload_id.context(SelectedIntegrity(
            "incompatible_index: payload-less tuple names a call".into(),
        ))?;
        let unchanged: Option<bool> = self
            .db
            .prepare_cached(
                "SELECT e.captured_hash IS NOT NULL AND e.captured_hash IS (SELECT v.content_hash FROM revision_documents d JOIN document_versions v ON v.vid=d.vid WHERE d.revision_id=?2 AND d.language=e.language AND d.path=e.path) FROM semantic_projection_dependencies e WHERE e.payload_id=?1 AND e.language=?3 AND e.path=?4",
            )?
            .query_row(params![payload, self.revision, language, path], |r| {
                r.get(0)
            })
            .optional()?;
        let unchanged = unchanged.context(SelectedIntegrity(
            "incompatible_index: capture-bound call without its dependency".into(),
        ))?;
        self.unchanged.borrow_mut().insert(key, unchanged);
        Ok(unchanged)
    }

    /// The reading revision's dispatch of a stored call binding (#1279, Decision 0021 D1 as
    /// amended 2026-11-21): a capture-bound `direct` ([`capture_bound`]) is `unknown` while its
    /// target document differs from the capture (#557), exactly as settling it at this revision
    /// would make it; every other dispatch is revision-neutral.
    pub(in crate::store) fn dispatch(
        &self,
        stored: sem::Dispatch,
        capture_bound: bool,
        target: Option<&sem::Target>,
    ) -> Result<sem::Dispatch> {
        if !capture_bound {
            return Ok(stored);
        }
        let (sem::Dispatch::Direct, Some(sem::Target::Internal { document, .. })) =
            (stored, target)
        else {
            return Err(SelectedIntegrity(
                "incompatible_index: corrupt capture-bound call dispatch".into(),
            )
            .into());
        };
        Ok(
            if self.unchanged_since_capture(&document.language, &document.path)? {
                stored
            } else {
                sem::Dispatch::Unknown
            },
        )
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
        Ok(self.proof(ordinal)?.0)
    }

    /// The reading revision's provenance ID for the row at `ordinal`, and the stable identity
    /// it is minted from (#1310).
    pub(in crate::store) fn proof(&self, ordinal: i64) -> Result<(String, sem::ProofIdentity)> {
        if let Some(proof) = self.ids.borrow().get(&ordinal) {
            return Ok(proof.clone());
        }
        let payload = self.payload_id.context(SelectedIntegrity(
            "incompatible_index: payload-less tuple names provenance".into(),
        ))?;
        let row: Option<(Option<String>, Option<String>, String)> = self
            .db
            .prepare_cached(
                "SELECT provenance_id,CASE WHEN derived_from_provenance_id IS NULL THEN NULL ELSE lower(hex(derived_from_provenance_id)) END,evidence_kind FROM semantic_provenance WHERE payload_id=?1 AND ordinal=?2",
            )?
            .query_row(params![payload, ordinal], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        let (literal, derived, kind) = row.context(SelectedIntegrity(
            "incompatible_index: semantic fact without provenance".into(),
        ))?;
        let identity = match (literal, derived) {
            (Some(id), None) => sem::ProofIdentity::Literal(id),
            (None, Some(raw)) => sem::ProofIdentity::Derived {
                raw,
                kind: semantic_enum(kind)?,
            },
            _ => {
                return Err(SelectedIntegrity(
                    "incompatible_index: corrupt semantic provenance ID".into(),
                )
                .into());
            }
        };
        let proof = (identity.provenance_id(&self.native), identity);
        self.ids.borrow_mut().insert(ordinal, proof.clone());
        Ok(proof)
    }

    /// The reading revision's freshness of every basis of the payload, as stored (#1310).
    pub(in crate::store) fn freshness_values(&self) -> Result<BTreeMap<i64, String>> {
        self.freshness
            .iter()
            .map(|(basis, freshness)| Ok((*basis, semantic_variant(freshness)?)))
            .collect()
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

/// The key one chunk of `table`'s rows is deleted by: the primary key of a payload child stored
/// `WITHOUT ROWID` (#1248 PR 4), else the rowid.
fn chunk_key(table: &str) -> &'static str {
    match table {
        "semantic_provenance" => "payload_id,ordinal",
        "semantic_reference_roles" => "payload_id,reference_id,ordinal",
        "semantic_symbol_targets" => "symbol_id,ordinal",
        "semantic_declaration_binding_symbols"
        | "semantic_declaration_join_candidates"
        | "semantic_call_join_candidates" => "binding_id,ordinal",
        "semantic_projection_dependency_targets" => "payload_id,language,path,syntax_id",
        _ => "rowid",
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
                let key = chunk_key(table);
                let deleted = tx.execute(
                    &format!("DELETE FROM {table} WHERE ({key}) IN (SELECT {key} FROM {table} WHERE {filter} LIMIT 256)"),
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
                content_hash: "c".repeat(64),
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
            content_hash: "c".repeat(64),
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
        db.execute_batch(CACHE_SCHEMA_V49).unwrap();
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
        db.execute_batch(CACHE_SCHEMA_V49).unwrap();
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
        db.execute_batch(CACHE_SCHEMA_V49).unwrap();
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

    /// Payload children stored `WITHOUT ROWID` (#1248 PR 4) have no rowid; collection deletes
    /// their 256-row chunks by primary key and still drains every one of them.
    #[test]
    fn release_collects_without_rowid_children_in_chunks() {
        let db = Connection::open_in_memory().unwrap();
        payload_with_children(&db, None, 1);
        db.execute("INSERT INTO semantic_declaration_bindings(binding_id,payload_id,ordinal,provenance_ordinal,join_document_language,join_document_path,join_content_hash,join_start,join_end,join_kind,join_status) VALUES(1,1,0,0,'javascript','a.js',zeroblob(32),0,1,'declarationName','exact')", []).unwrap();
        for i in 0..300_i64 {
            let id = i.to_be_bytes().repeat(2);
            for sql in [
                "INSERT INTO semantic_provenance(payload_id,ordinal,provenance_id,evidence_kind,basis_id) VALUES(1,?1,'p'||?1,'measuredSyntax',1)",
                "INSERT INTO semantic_reference_roles VALUES(1,?2,?1,'read')",
                "INSERT INTO semantic_symbol_targets(symbol_id,ordinal,kind,external_scheme,external_symbol,external_scope) VALUES(1,?1,'external','scip','s','global')",
                "INSERT INTO semantic_declaration_binding_symbols VALUES(1,?1,'scip','s','global',NULL,NULL)",
                "INSERT INTO semantic_declaration_join_candidates VALUES(1,?1,?2)",
                "INSERT INTO semantic_projection_dependency_targets VALUES(1,'javascript','a.js',?2,0)",
            ] {
                if sql.contains("?2") {
                    db.execute(sql, params![i, id]).unwrap();
                } else {
                    db.execute(sql, [i]).unwrap();
                }
            }
        }
        let mut calls = 0;
        while cleanup_payload_candidates(&db, "rev").unwrap() {
            calls += 1;
            assert!(calls < 40);
        }
        assert!(calls >= 7, "six 300-row tables take two chunks each");
        for table in [
            "semantic_provenance",
            "semantic_reference_roles",
            "semantic_symbol_targets",
            "semantic_declaration_binding_symbols",
            "semantic_declaration_join_candidates",
            "semantic_projection_dependency_targets",
            "semantic_payloads",
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

    /// A payload (tuple 1 at `same`, 2 at `edited`) whose one call binding is a `direct` call
    /// from `a.js` into `d.js`, marked `bound`, with `d.js`'s capture hash `captured`. `d.js` has
    /// hash `1…` at `same` and `2…` at `edited`.
    fn bound_call(bound: bool, captured: Option<&str>) -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(CACHE_SCHEMA_V49).unwrap();
        db.pragma_update(None, "foreign_keys", "OFF").unwrap();
        for (tuple, revision) in [(1, "same"), (2, "edited")] {
            db.execute("INSERT INTO native_revisions VALUES(?1,'set',?2,?2,?2,?1,'[]','[]','{}','[]','[]',NULL,'[]',0,'{}','[]',?3)", params![revision, "a".repeat(64), tuple]).unwrap();
            db.execute("INSERT INTO document_versions(vid,id,language,path,content_hash,extraction_context,producer_id,producer_version,byte_length,source_bytes,native_witness) VALUES(?1,?2,'javascript','d.js',?3,?3,'p','1',0,x'',?3)", params![tuple, format!("v{tuple}"), tuple.to_string().repeat(64)]).unwrap();
            db.execute("INSERT INTO revision_documents VALUES(?1,'javascript','d.js',?2,1,NULL,NULL,1,1,'complete',NULL,0,1)", params![revision, tuple]).unwrap();
            db.execute("INSERT INTO semantic_coverage VALUES(?1,?2,'p','1','javascript','a.js',1,1,'complete',NULL,1)", params![tuple, revision]).unwrap();
        }
        db.execute("INSERT INTO semantic_projection_dependencies(payload_id,language,path,term,captured_hash) VALUES(1,'javascript','d.js',NULL,?1)", [captured]).unwrap();
        db.execute("INSERT INTO semantic_call_bindings(payload_id,ordinal,join_document_language,join_document_path,join_content_hash,join_start,join_end,join_kind,join_status,resolution,declared_kind,declared_internal_syntax_id,declared_internal_language,declared_internal_path,dispatch,possible_dispatch_complete,stale_target,provenance_ordinal,capture_bound) VALUES(1,0,'javascript','a.js',zeroblob(32),0,1,'callee','exact','resolved','internal',?1,'javascript','d.js','direct',0,0,0,?2)", params![vec![7u8; 16], bound]).unwrap();
        db
    }

    /// The projected dispatch at `revision` (tuple `tuple`), through the shared projection and
    /// through its SQL form; the two must agree.
    fn read_dispatch(db: &Connection, tuple: i64, revision: &str) -> sem::Dispatch {
        let none = BTreeMap::new();
        let projection =
            project_payload(db, revision, tuple, FreshnessSource::Publication(&none)).unwrap();
        let target = sem::Target::Internal {
            syntax_id: format!("sid:v1:{}", "07".repeat(16)),
            document: crate::native_records::DocumentKey {
                source_set_id: "set".into(),
                language: "javascript".into(),
                path: "d.js".into(),
            },
            revision_id: revision.into(),
        };
        let bound: bool = db
            .query_row(
                "SELECT capture_bound FROM semantic_call_bindings",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let projected = projection
            .dispatch(sem::Dispatch::Direct, bound, Some(&target))
            .unwrap();
        let sql: String = db
            .query_row(
                &format!(
                    "SELECT {} FROM semantic_call_bindings b",
                    projected_dispatch_sql("b", "?1")
                ),
                [revision],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(semantic_enum::<sem::Dispatch>(sql).unwrap(), projected);
        projected
    }

    /// #1279: a capture-bound `direct` reads `direct` while its target document has its capture
    /// hash at the reading revision and `unknown` otherwise; an unbound one is revision-neutral;
    /// a document absent at the capture (no hash) is never unchanged.
    #[test]
    fn capture_bound_dispatch_follows_the_reading_revision() {
        let db = bound_call(true, Some(&"1".repeat(64)));
        assert_eq!(read_dispatch(&db, 1, "same"), sem::Dispatch::Direct);
        assert_eq!(read_dispatch(&db, 2, "edited"), sem::Dispatch::Unknown);
        let db = bound_call(false, Some(&"1".repeat(64)));
        assert_eq!(read_dispatch(&db, 2, "edited"), sem::Dispatch::Direct);
        let db = bound_call(true, None);
        assert_eq!(read_dispatch(&db, 1, "same"), sem::Dispatch::Unknown);
    }

    /// A capture-bound call whose dependency row is missing, or a bound row that is not a
    /// `direct` call to an internal target, is corruption, never an answer.
    #[test]
    fn corrupt_capture_bound_calls_fail_typed() {
        let db = bound_call(true, Some(&"1".repeat(64)));
        db.execute("DELETE FROM semantic_projection_dependencies", [])
            .unwrap();
        let projection = project_payload(&db, "same", 1, FreshnessSource::Stored).unwrap();
        let target = sem::Target::Internal {
            syntax_id: format!("sid:v1:{}", "07".repeat(16)),
            document: crate::native_records::DocumentKey {
                source_set_id: "set".into(),
                language: "javascript".into(),
                path: "d.js".into(),
            },
            revision_id: "same".into(),
        };
        let missing = projection
            .dispatch(sem::Dispatch::Direct, true, Some(&target))
            .unwrap_err();
        assert!(missing.is::<SelectedIntegrity>(), "{missing:#}");
        let not_direct = projection
            .dispatch(sem::Dispatch::Dynamic, true, Some(&target))
            .unwrap_err();
        assert!(not_direct.is::<SelectedIntegrity>(), "{not_direct:#}");
    }

    /// Only a `direct` call whose declared target is a `method` of another document is
    /// capture-bound.
    #[test]
    fn only_a_direct_method_call_into_another_document_is_capture_bound() {
        let db = bound_call(false, None);
        db.execute("INSERT INTO native_version_declarations(vid,syntax_id,kind,role,name,lookup_key,key_signature_present,key_ordinal,start_byte,end_byte,name_start,name_end) VALUES(1,?1,'method','production','m','m',0,0,0,10,1,2)", [vec![7u8; 16]]).unwrap();
        db.execute("INSERT INTO native_version_declarations(vid,syntax_id,kind,role,name,lookup_key,key_signature_present,key_ordinal,start_byte,end_byte,name_start,name_end) VALUES(1,?1,'function','production','f','f',0,0,0,10,1,2)", [vec![8u8; 16]]).unwrap();
        let call = |sid: u8, path: &str, dispatch: sem::Dispatch| sem::CallBinding {
            call_id: None,
            join: anchor("a.js", sem::JoinKind::Callee),
            resolution: sem::Resolution::Resolved,
            declared_target: Some(sem::Target::Internal {
                syntax_id: format!("sid:v1:{}", format!("{sid:02x}").repeat(16)),
                document: document(path),
                revision_id: "native".into(),
            }),
            candidates: vec![],
            dispatch,
            possible_dispatch: vec![],
            possible_dispatch_complete: false,
            stale_target: Some(false),
            provenance_id: "p".into(),
        };
        let bound = |binding: sem::CallBinding, own: &str| {
            capture_bound(&db, "same", "javascript", own, &binding).unwrap()
        };
        assert!(bound(call(7, "d.js", sem::Dispatch::Direct), "a.js"));
        assert!(
            !bound(call(7, "d.js", sem::Dispatch::Direct), "d.js"),
            "own document"
        );
        assert!(
            !bound(call(8, "d.js", sem::Dispatch::Direct), "a.js"),
            "free function"
        );
        assert!(
            !bound(call(7, "d.js", sem::Dispatch::Dynamic), "a.js"),
            "not direct"
        );
    }

    /// Guard (#1279): every source file whose SQL names the call binding table, with how many
    /// times. Their reads of `dispatch` go through the shared projection
    /// ([`PayloadProjection::dispatch`], or [`projected_dispatch_sql`] for a reader that sizes
    /// rows in SQL), or read a revision where the projection is the identity (the witness's
    /// original import). A new mention fails here until it is reviewed against the projection.
    #[test]
    fn every_reader_of_call_bindings_is_reviewed_against_the_projection() {
        let table = concat!("semantic_", "call_bindings");
        let expected: BTreeMap<&str, usize> = [
            // Schema, writer, tuple reader (projected), provenance selection, GC lists.
            ("src/store.rs", 11),
            // Navigation call budget (projected SQL) and its sizing arms.
            ("src/graph_read.rs", 9),
            // Architecture semantic inputs (projected), counts, and the dependency-only
            // alternatives of visited bindings (two, by binding ID); the latter do not consume
            // dispatch.
            ("src/store/semantic_display.rs", 6),
            // GC of unnamed payloads, and this module's projection tests.
            ("src/store/semantic_payload.rs", 7),
            // The original import's rows, read at the captured revision.
            ("src/store/native_first/witness.rs", 4),
            // A table list in a cancellation test.
            ("src/http/normal_post_capture_cancellation_tests.rs", 1),
            // #1248's digest storage tests: the stored join_content_hash only, no dispatch.
            ("src/store/digest_blob_tests.rs", 4),
        ]
        .into_iter()
        .collect();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut found = BTreeMap::new();
        let mut pending = vec![root.join("src")];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let count = std::fs::read_to_string(&path)
                        .unwrap()
                        .matches(table)
                        .count();
                    if count > 0 {
                        let relative = path.strip_prefix(root).unwrap().to_string_lossy();
                        found.insert(relative.replace('\\', "/"), count);
                    }
                }
            }
        }
        let expected: BTreeMap<String, usize> = expected
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        assert_eq!(found, expected);
    }
}
