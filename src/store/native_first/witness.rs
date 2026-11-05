//! Bounded original-admission authority for LOCAL source-association reuse.
//! Every call uses the owner's fresh exact-target snapshot; no SQLite handle survives it.
use super::*;
use crate::native_evidence::{DocumentKey, Range};
use crate::semantic_batch::{
    BatchLimits,
    descriptor::{Descriptor, SourceManifestHasher},
};
use crate::semantic_evidence as sem;

pub(crate) type Reserve<'a> = dyn FnMut(u64) -> Result<()> + 'a;

pub(crate) struct SourceAuthority {
    pub batch_id: i64,
    pub capture_id: i64,
    pub artifact_hash: String,
    pub captured_pin: IndexPin,
    pub original_pin: IndexPin,
    pub producer: sem::Producer,
    pub source_set_id: String,
    pub captured_revision_id: String,
    pub source_manifest_hash: String,
    pub toolchain_hash: String,
    pub config_hash: String,
    pub dependency_hash: String,
    pub descriptor: Descriptor,
    target: PendingSemanticTarget,
    /// The original associations are the batch's complete capture witness, not an import.
    pub cache: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SelectedVersion {
    pub vid: i64,
    pub version_id: String,
    pub content_hash: String,
    pub byte_length: u64,
    pub extraction_context: String,
    pub producer_id: String,
    pub producer_version: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalSourceWitness {
    pub tuple_id: i64,
    pub document: DocumentKey,
    pub captured_version: SelectedVersion,
    pub current_version: SelectedVersion,
    pub source_hash: String,
    pub wire_digest: Option<String>,
    pub capped: Option<i64>,
    pub requested: bool,
    pub selected: bool,
    pub state: sem::CoverageState,
    pub diagnostic: Option<String>,
    pub reasons: Option<sem::CoverageReasons>,
    /// The batch's raw exclusion of this document, which FULL's coverage also reads.
    pub exclusion: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CoverageInput {
    pub document: DocumentKey,
    pub current: SelectedVersion,
    pub requested: bool,
    pub selected: bool,
    pub exclusion: Option<String>,
}

pub(crate) enum WitnessEligibility {
    Witness(Box<OriginalSourceWitness>),
    ChangedBytes(CoverageInput),
    NewDocument,
    Uncovered(CoverageInput),
    Full(&'static str),
}

pub(crate) enum AuthorityStep {
    Progress,
    Eligible(Box<SourceAuthority>),
    /// A witness was attempted and declined: FULL, counted as a fallback with its reason.
    Full(&'static str),
    /// The witnessed route does not apply (context invalidation): FULL by rule.
    Ineligible,
}

#[derive(Default)]
pub(crate) struct OriginalAdmissionCursor {
    authority: Option<SourceAuthority>,
    after_selection: i64,
    original: Option<(u64, IndexPin)>,
    inventory: (usize, String),
    manifest: Option<SourceManifestHasher>,
    source_count: u64,
    phase: u8,
}

#[derive(Default)]
pub(crate) struct OriginalAssociationCursor {
    phase: u8,
    after_id: i64,
    after_text: String,
    after_blob: Vec<u8>,
    parent: i64,
    parent_blob: Vec<u8>,
    child_phase: u8,
    child_ordinal: i64,
    /// Rows of one capture-witness document, decoded once and handed out in pages.
    cached: Option<std::collections::VecDeque<OriginalRow>>,
}

#[derive(Default)]
pub(crate) struct InternalSymbolCursor {
    tuple: i64,
    binding_ordinal: i64,
    binding: Option<i64>,
    symbol_ordinal: i64,
    started: bool,
    pub done: bool,
}

pub(crate) struct OriginalPage {
    pub rows: Vec<OriginalRow>,
    pub done: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalBasis {
    pub basis_id: i64,
    pub producer_id: String,
    pub producer_version: String,
    pub producer_hash: String,
    pub artifact_hash: String,
    pub language: sem::Language,
    pub source_set_id: String,
    pub revision_id: String,
    pub source_manifest_hash: String,
    pub toolchain_hash: String,
    pub config_hash: String,
    pub dependency_hash: String,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalProvenance {
    pub id: String,
    pub content_hash: String,
    pub evidence_kind: sem::EvidenceKind,
    pub freshness: sem::Freshness,
    pub basis_id: i64,
    pub capture_id: i64,
    pub derived_from: sem::DerivedFrom,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalSymbol {
    pub row_id: i64,
    pub ordinal: i64,
    pub key: sem::SymbolKey,
    pub display_name: Option<String>,
    pub provenance_id: String,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalJoin {
    pub document: DocumentKey,
    pub revision_id: String,
    pub content_hash: String,
    pub range: Range,
    pub kind: sem::JoinKind,
    pub status: sem::JoinStatus,
    pub diagnostic: Option<String>,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalDeclaration {
    pub row_id: i64,
    pub ordinal: i64,
    pub syntax_id: Option<String>,
    pub provenance_id: String,
    pub join: OriginalJoin,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalReference {
    pub id: String,
    pub ordinal: i64,
    pub owner_syntax_id: String,
    pub range: Range,
    pub spelling: String,
    pub lookup_key: String,
    pub site: sem::ReferenceSite,
    pub resolution: sem::Resolution,
    pub declared_target: Option<sem::Target>,
    pub provenance_id: String,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OriginalCall {
    pub row_id: i64,
    pub ordinal: i64,
    pub call_id: Option<String>,
    pub join: OriginalJoin,
    pub resolution: sem::Resolution,
    pub declared_target: Option<sem::Target>,
    pub dispatch: sem::Dispatch,
    pub possible_dispatch_complete: bool,
    pub stale_target: Option<bool>,
    pub provenance_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum OriginalRow {
    Coverage(sem::Coverage),
    CoverageRole {
        observed: bool,
        ordinal: i64,
        role: sem::Role,
    },
    Basis(OriginalBasis),
    BasisLookupDependency {
        basis_id: i64,
        ordinal: i64,
        key: String,
    },
    Provenance(OriginalProvenance),
    Symbol(OriginalSymbol),
    SymbolTarget {
        row_id: i64,
        ordinal: i64,
        target: sem::Target,
    },
    DeclarationBinding(OriginalDeclaration),
    DeclarationSymbol {
        row_id: i64,
        ordinal: i64,
        key: sem::SymbolKey,
    },
    DeclarationCandidate {
        row_id: i64,
        ordinal: i64,
        id: String,
    },
    Reference(OriginalReference),
    ReferenceRole {
        reference_id: String,
        ordinal: i64,
        role: sem::Role,
    },
    ReferenceCandidate {
        reference_id: String,
        ordinal: i64,
        target: sem::Target,
    },
    CallBinding(OriginalCall),
    CallJoinCandidate {
        row_id: i64,
        ordinal: i64,
        id: String,
    },
    CallCandidate {
        row_id: i64,
        ordinal: i64,
        target: sem::Target,
    },
    CallDispatch {
        row_id: i64,
        ordinal: i64,
        target: sem::Target,
    },
    TypeRelationship {
        ordinal: i64,
        row: sem::TypeRelationship,
    },
}

#[derive(Debug)]
pub(crate) struct CurrentDeclaration {
    pub kind: String,
    pub name: Option<String>,
    pub name_range: Option<Range>,
    /// Coarse declaration family only; not the semantic dispatch/callability policy.
    pub callable: bool,
}

fn read_join(row: &rusqlite::Row<'_>, offset: usize, source_set: &str) -> Result<OriginalJoin> {
    let start: i64 = row.get(offset + 4)?;
    let end: i64 = row.get(offset + 5)?;
    selected_ensure!(start >= 0 && end >= start, "invalid original join span");
    Ok(OriginalJoin {
        document: DocumentKey {
            source_set_id: source_set.to_owned(),
            language: row.get(offset)?,
            path: row.get(offset + 1)?,
        },
        revision_id: row.get(offset + 2)?,
        content_hash: row.get(offset + 3)?,
        range: Range {
            start: start as usize,
            end: end as usize,
        },
        kind: semantic_enum(row.get(offset + 6)?)?,
        status: semantic_enum(row.get(offset + 7)?)?,
        diagnostic: row.get(offset + 8)?,
    })
}
fn decoded_id(row: &rusqlite::Row<'_>, column: usize, call: bool) -> Result<String> {
    let bytes = row.get_ref(column)?.as_blob()?;
    if call {
        id_codec::decode_occ(bytes)
    } else {
        id_codec::decode_sid(bytes)
    }
    .map_err(|_| corrupt("invalid original semantic ID"))
}
fn optional_id(row: &rusqlite::Row<'_>, column: usize, call: bool) -> Result<Option<String>> {
    if matches!(row.get_ref(column)?, rusqlite::types::ValueRef::Null) {
        Ok(None)
    } else {
        decoded_id(row, column, call).map(Some)
    }
}
fn next_parent(cursor: &mut OriginalAssociationCursor, phase: u8) {
    cursor.phase = phase;
    cursor.after_id = -1;
    cursor.after_text.clear();
    cursor.after_blob.clear();
    cursor.parent = 0;
    cursor.child_phase = 0;
    cursor.child_ordinal = -1;
}

pub(crate) fn original_document_page(
    db: &Connection,
    authority: &SourceAuthority,
    witness: &OriginalSourceWitness,
    cursor: &mut OriginalAssociationCursor,
    limit: usize,
    reserve: &mut Reserve<'_>,
) -> Result<OriginalPage> {
    original_guard(db, authority)?;
    ensure!(limit > 0, "witness page limit must be positive");
    if authority.cache {
        if cursor.cached.is_none() {
            cursor.cached = Some(cache_rows(db, authority, witness, reserve)?.into());
        }
        let rows = cursor.cached.as_mut().unwrap();
        let page: Vec<_> = (0..limit).map_while(|_| rows.pop_front()).collect();
        return Ok(OriginalPage {
            done: rows.is_empty(),
            rows: page,
        });
    }
    let mut result = Vec::new();
    // Transitions are units too: a document with empty families cannot hide an unbounded scan.
    // A unit reads up to the page's remaining rows of one family with one query per table.
    let mut units = 0;
    while result.len() < limit && units < limit && cursor.phase < 10 {
        units += 1;
        let room = limit - result.len();
        association_unit(db, authority, witness, cursor, room, &mut result, reserve)?;
    }
    Ok(OriginalPage {
        rows: result,
        done: cursor.phase >= 10,
    })
}

/// The same typed original rows as `association_unit`, from one capture-witness document.
fn cache_rows(
    db: &Connection,
    a: &SourceAuthority,
    w: &OriginalSourceWitness,
    reserve: &mut Reserve<'_>,
) -> Result<Vec<OriginalRow>> {
    let snapshot = crate::store::captured_witness::document(db, a.batch_id, &w.document)?
        .ok_or_else(|| corrupt("capture witness document missing"))?;
    reserve(
        (serde_json::to_vec(&snapshot)?.len() as u64)
            .checked_mul(2)
            .ok_or_else(|| corrupt("capture witness size"))?,
    )?;
    let [coverage] = snapshot.coverage.as_slice() else {
        return Err(corrupt("capture witness document coverage"));
    };
    let mut rows = vec![OriginalRow::Coverage(sem::Coverage {
        producer_id: a.producer.id.clone(),
        language: semantic_enum(w.document.language.clone())?,
        source_set_id: a.source_set_id.clone(),
        document_path: w.document.path.clone(),
        revision_id: a.captured_revision_id.clone(),
        requested: w.requested,
        selected: w.selected,
        state: w.state,
        supported_roles: Vec::new(),
        observed_roles: Vec::new(),
        diagnostic: w.diagnostic.clone(),
        reasons: w.reasons.clone(),
    })];
    for (observed, roles) in [
        (false, &coverage.supported_roles),
        (true, &coverage.observed_roles),
    ] {
        for (ordinal, role) in roles.iter().enumerate() {
            rows.push(OriginalRow::CoverageRole {
                observed,
                ordinal: ordinal as i64,
                role: *role,
            });
        }
    }
    let mut bases: Vec<sem::SemanticBasis> = Vec::new();
    let mut provenance = snapshot.provenance.clone();
    provenance.sort_by(|x, y| x.id.cmp(&y.id));
    let mut proofs = Vec::new();
    for p in &provenance {
        let (Some(basis), Some(derived)) = (&p.basis, &p.derived_from) else {
            return Err(corrupt("capture witness proof missing derivation"));
        };
        selected_ensure!(
            basis.producer_id == a.producer.id
                && basis.producer_version == a.producer.version
                && basis.producer_hash == a.producer.executable_hash
                && basis.artifact_hash == a.artifact_hash
                && basis.source_set_id == a.source_set_id
                && basis.revision_id == a.captured_revision_id
                && basis.source_manifest_hash == a.source_manifest_hash
                && basis.toolchain_hash == a.toolchain_hash
                && basis.config_hash == a.config_hash
                && basis.dependency_hash == a.dependency_hash
                && semantic_variant(&basis.language)? == w.document.language,
            "original basis mismatch"
        );
        selected_ensure!(
            p.content_hash == w.source_hash,
            "original provenance source mismatch"
        );
        let kind = semantic_variant(&derived.record_kind)?;
        let membership:bool=db.prepare_cached("SELECT EXISTS(SELECT 1 FROM semantic_raw_proofs WHERE capture_id=?1 AND proof_id=?2 AND record_kind=?3 AND fact_digest=?4 AND language=?5 AND path=?6)")?.query_row(params![a.capture_id,derived.provenance_id,kind,derived.fact_digest,w.document.language,w.document.path],|r|r.get(0))?;
        selected_ensure!(membership, "original derivation identity mismatch");
        let basis_id = match bases.iter().position(|b| b == basis) {
            Some(index) => index,
            None => {
                bases.push(basis.clone());
                bases.len() - 1
            }
        } as i64
            + 1;
        proofs.push(OriginalRow::Provenance(OriginalProvenance {
            id: p.id.clone(),
            content_hash: p.content_hash.clone(),
            evidence_kind: p.evidence_kind,
            freshness: p.freshness,
            basis_id,
            capture_id: a.capture_id,
            derived_from: derived.clone(),
        }));
    }
    for (index, basis) in bases.iter().enumerate() {
        let basis_id = index as i64 + 1;
        rows.push(OriginalRow::Basis(OriginalBasis {
            basis_id,
            producer_id: basis.producer_id.clone(),
            producer_version: basis.producer_version.clone(),
            producer_hash: basis.producer_hash.clone(),
            artifact_hash: basis.artifact_hash.clone(),
            language: basis.language,
            source_set_id: basis.source_set_id.clone(),
            revision_id: basis.revision_id.clone(),
            source_manifest_hash: basis.source_manifest_hash.clone(),
            toolchain_hash: basis.toolchain_hash.clone(),
            config_hash: basis.config_hash.clone(),
            dependency_hash: basis.dependency_hash.clone(),
        }));
        for (ordinal, key) in basis.lookup_dependencies.iter().enumerate() {
            rows.push(OriginalRow::BasisLookupDependency {
                basis_id,
                ordinal: ordinal as i64,
                key: key.clone(),
            });
        }
    }
    rows.extend(proofs);
    let join = |join: &sem::Join| OriginalJoin {
        document: join.anchor.document.clone(),
        revision_id: join.anchor.revision_id.clone(),
        content_hash: join.anchor.content_hash.clone(),
        range: join.anchor.range.clone(),
        kind: join.anchor.kind,
        status: join.status,
        diagnostic: join.diagnostic.clone(),
    };
    for (ordinal, symbol) in snapshot.symbols.iter().enumerate() {
        let row_id = ordinal as i64 + 1;
        rows.push(OriginalRow::Symbol(OriginalSymbol {
            row_id,
            ordinal: ordinal as i64,
            key: symbol.key.clone(),
            display_name: symbol.display_name.clone(),
            provenance_id: symbol.provenance_id.clone(),
        }));
        for (child, target) in symbol.declarations.iter().enumerate() {
            rows.push(OriginalRow::SymbolTarget {
                row_id,
                ordinal: child as i64,
                target: target.clone(),
            });
        }
    }
    for (ordinal, binding) in snapshot.declaration_bindings.iter().enumerate() {
        let row_id = ordinal as i64 + 1;
        rows.push(OriginalRow::DeclarationBinding(OriginalDeclaration {
            row_id,
            ordinal: ordinal as i64,
            syntax_id: binding.syntax_id.clone(),
            provenance_id: binding.provenance_id.clone(),
            join: join(&binding.join),
        }));
        for (child, key) in binding.symbols.iter().enumerate() {
            rows.push(OriginalRow::DeclarationSymbol {
                row_id,
                ordinal: child as i64,
                key: key.clone(),
            });
        }
        for (child, id) in binding.join.candidate_ids.iter().enumerate() {
            rows.push(OriginalRow::DeclarationCandidate {
                row_id,
                ordinal: child as i64,
                id: id.clone(),
            });
        }
    }
    let mut references = snapshot.references.clone();
    references.sort_by(|x, y| x.id.cmp(&y.id));
    for reference in references {
        rows.push(OriginalRow::Reference(OriginalReference {
            id: reference.id.clone(),
            ordinal: reference.ordinal as i64,
            owner_syntax_id: reference.owner_syntax_id.clone(),
            range: reference.range.clone(),
            spelling: reference.spelling.clone(),
            lookup_key: reference.lookup_key.clone(),
            site: reference.site,
            resolution: reference.resolution,
            declared_target: reference.declared_target.clone(),
            provenance_id: reference.provenance_id.clone(),
        }));
        for (child, role) in reference.roles.iter().enumerate() {
            rows.push(OriginalRow::ReferenceRole {
                reference_id: reference.id.clone(),
                ordinal: child as i64,
                role: *role,
            });
        }
        for (child, target) in reference.candidates.iter().enumerate() {
            rows.push(OriginalRow::ReferenceCandidate {
                reference_id: reference.id.clone(),
                ordinal: child as i64,
                target: target.clone(),
            });
        }
    }
    for (ordinal, call) in snapshot.call_bindings.iter().enumerate() {
        let row_id = ordinal as i64 + 1;
        rows.push(OriginalRow::CallBinding(OriginalCall {
            row_id,
            ordinal: ordinal as i64,
            call_id: call.call_id.clone(),
            join: join(&call.join),
            resolution: call.resolution,
            declared_target: call.declared_target.clone(),
            dispatch: call.dispatch,
            possible_dispatch_complete: call.possible_dispatch_complete,
            stale_target: call.stale_target,
            provenance_id: call.provenance_id.clone(),
        }));
        for (child, id) in call.join.candidate_ids.iter().enumerate() {
            rows.push(OriginalRow::CallJoinCandidate {
                row_id,
                ordinal: child as i64,
                id: id.clone(),
            });
        }
        for (child, target) in call.candidates.iter().enumerate() {
            rows.push(OriginalRow::CallCandidate {
                row_id,
                ordinal: child as i64,
                target: target.clone(),
            });
        }
        for (child, target) in call.possible_dispatch.iter().enumerate() {
            rows.push(OriginalRow::CallDispatch {
                row_id,
                ordinal: child as i64,
                target: target.clone(),
            });
        }
    }
    for (ordinal, relationship) in snapshot.type_relationships.iter().enumerate() {
        rows.push(OriginalRow::TypeRelationship {
            ordinal: ordinal as i64,
            row: relationship.clone(),
        });
    }
    Ok(rows)
}

/// One parent row of a family (symbols, declarations, references, calls, bases) in page order.
struct ParentRow {
    id: i64,
    blob: Vec<u8>,
    order: i64,
    bytes: u64,
    row: OriginalRow,
}
/// One child row, keyed by its parent's integer ID or reference ID.
struct ChildRow {
    id: i64,
    blob: Vec<u8>,
    ordinal: i64,
    bytes: u64,
    row: OriginalRow,
}
/// A child table of a family. Its query reads the children of a range of parents in
/// (parent order, child ordinal) order: `?1` the tuple, parents ordered in `[?2, ?3]`, the
/// parent at `?2` only for child ordinals above `?4`, at most `?5` rows. Column 0 is the
/// parent's key.
struct ChildTable {
    sql: &'static str,
    gap: &'static str,
    decode: fn(&rusqlite::Row<'_>, &str) -> Result<ChildRow>,
}

fn row_bytes(row: &rusqlite::Row<'_>, from: usize) -> Result<u64> {
    let mut bytes = (2 * std::mem::size_of::<OriginalRow>()) as u64;
    for index in from..row.as_ref().column_count() {
        let length = match row.get_ref(index)? {
            rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
                value.len() as u64
            }
            _ => 0,
        };
        bytes = bytes
            .checked_add(
                length
                    .checked_mul(2)
                    .ok_or_else(|| corrupt("witness row size"))?,
            )
            .ok_or_else(|| corrupt("witness row size"))?;
    }
    Ok(bytes)
}

/// Emits rows for one page; every emitted row after a unit's first also carries the
/// per-unit source-set charge, as when each row was its own unit.
struct Emit<'p, 'r> {
    out: &'p mut Vec<OriginalRow>,
    room: usize,
    emitted: usize,
    unit: u64,
    reserve: &'p mut Reserve<'r>,
}
impl Emit<'_, '_> {
    fn full(&self) -> bool {
        self.emitted >= self.room
    }
    fn push(&mut self, row: OriginalRow, bytes: u64) -> Result<()> {
        if self.emitted > 0 {
            (self.reserve)(self.unit)?;
        }
        (self.reserve)(bytes)?;
        self.out.push(row);
        self.emitted += 1;
        Ok(())
    }
}

fn query_rows<T>(
    db: &Connection,
    sql: &str,
    values: &[&dyn rusqlite::ToSql],
    mut decode: impl FnMut(&rusqlite::Row<'_>) -> Result<T>,
) -> Result<Vec<T>> {
    let mut statement = db.prepare_cached(sql)?;
    let mut rows = statement.query(values)?;
    let mut found = Vec::new();
    while let Some(row) = rows.next()? {
        found.push(decode(row)?);
    }
    Ok(found)
}

/// Up to `room` original rows of the cursor's current family, read with one query per
/// table rather than one `LIMIT 1` query per row (#670). Rows, order, ordinal checks and the
/// cursor positions are those of reading the rows one at a time.
fn association_unit(
    db: &Connection,
    a: &SourceAuthority,
    w: &OriginalSourceWitness,
    c: &mut OriginalAssociationCursor,
    room: usize,
    out: &mut Vec<OriginalRow>,
    reserve: &mut Reserve<'_>,
) -> Result<()> {
    let source_set = &a.source_set_id;
    let unit = (source_set.len() as u64)
        .checked_mul(2)
        .ok_or_else(|| corrupt("witness source-set bytes"))?;
    reserve(unit)?;
    let room = room.max(1);
    let limit = i64::try_from(room)?;
    let mut emit = Emit {
        out,
        room,
        emitted: 0,
        unit,
        reserve,
    };
    match c.phase {
        0 => {
            let bytes = std::mem::size_of::<OriginalRow>() as u64
                + w.document.path.len() as u64
                + w.source_hash.len() as u64
                + w.diagnostic.as_ref().map_or(0, |s| s.len() as u64)
                + a.producer.id.len() as u64
                + a.captured_revision_id.len() as u64
                + source_set.len() as u64;
            next_parent(c, 1);
            emit.push(
                OriginalRow::Coverage(sem::Coverage {
                    producer_id: a.producer.id.clone(),
                    language: semantic_enum(w.document.language.clone())?,
                    source_set_id: source_set.clone(),
                    document_path: w.document.path.clone(),
                    revision_id: a.captured_revision_id.clone(),
                    requested: w.requested,
                    selected: w.selected,
                    state: w.state,
                    supported_roles: Vec::new(),
                    observed_roles: Vec::new(),
                    diagnostic: w.diagnostic.clone(),
                    reasons: w.reasons.clone(),
                }),
                bytes,
            )
        }
        1 => {
            while !emit.full() && c.phase == 1 {
                let observed = c.child_phase == 1;
                let want = emit.room - emit.emitted;
                let rows = query_rows(
                    db,
                    "SELECT ordinal,role FROM semantic_coverage_roles WHERE tuple_id=?1 AND kind=?2 AND ordinal>?3 ORDER BY ordinal LIMIT ?4",
                    params![
                        w.tuple_id,
                        if observed { "observed" } else { "supported" },
                        c.child_ordinal,
                        i64::try_from(want)?
                    ],
                    |row| {
                        Ok((
                            row_bytes(row, 0)?,
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                        ))
                    },
                )?;
                let fetched = rows.len();
                for (bytes, ordinal, role) in rows {
                    selected_ensure!(
                        ordinal == c.child_ordinal + 1,
                        "original coverage role ordinal gap"
                    );
                    emit.push(
                        OriginalRow::CoverageRole {
                            observed,
                            ordinal,
                            role: semantic_enum(role)?,
                        },
                        bytes,
                    )?;
                    c.child_ordinal = ordinal;
                }
                if fetched == want {
                    break;
                }
                if observed {
                    next_parent(c, 2);
                } else {
                    c.child_phase = 1;
                    c.child_ordinal = -1;
                }
            }
            Ok(())
        }
        2 => {
            let parents = if c.parent != 0 {
                Vec::new()
            } else {
                query_rows(
                    db,
                    "SELECT basis_id,producer_id,producer_version,producer_hash,artifact_hash,language,source_set_id,captured_native_revision_id,source_manifest_hash,toolchain_hash,config_hash,dependency_hash FROM semantic_basis WHERE tuple_id=?1 AND basis_id>?2 ORDER BY basis_id LIMIT ?3",
                    params![w.tuple_id, c.after_id, limit],
                    |row| {
                        let b = OriginalBasis {
                            basis_id: row.get(0)?,
                            producer_id: row.get(1)?,
                            producer_version: row.get(2)?,
                            producer_hash: row.get(3)?,
                            artifact_hash: row.get(4)?,
                            language: semantic_enum(row.get(5)?)?,
                            source_set_id: row.get(6)?,
                            revision_id: row.get(7)?,
                            source_manifest_hash: row.get(8)?,
                            toolchain_hash: row.get(9)?,
                            config_hash: row.get(10)?,
                            dependency_hash: row.get(11)?,
                        };
                        selected_ensure!(
                            b.producer_id == a.producer.id
                                && b.producer_version == a.producer.version
                                && b.producer_hash == a.producer.executable_hash
                                && b.artifact_hash == a.artifact_hash
                                && b.source_set_id == a.source_set_id
                                && b.revision_id == a.captured_revision_id
                                && b.source_manifest_hash == a.source_manifest_hash
                                && b.toolchain_hash == a.toolchain_hash
                                && b.config_hash == a.config_hash
                                && b.dependency_hash == a.dependency_hash
                                && semantic_variant(&b.language)? == w.document.language,
                            "original basis mismatch"
                        );
                        Ok(ParentRow {
                            id: b.basis_id,
                            blob: Vec::new(),
                            order: b.basis_id,
                            bytes: row_bytes(row, 0)?,
                            row: OriginalRow::Basis(b),
                        })
                    },
                )?
            };
            family(
                db,
                w.tuple_id,
                source_set,
                c,
                &mut emit,
                parents,
                &[ChildTable {
                    sql: "SELECT d.basis_id,d.ordinal,d.lookup_key FROM semantic_basis b JOIN semantic_basis_lookup_dependencies d ON d.basis_id=b.basis_id WHERE b.tuple_id=?1 AND b.basis_id>=?2 AND b.basis_id<=?3 AND (b.basis_id>?2 OR d.ordinal>?4) ORDER BY b.basis_id,d.ordinal LIMIT ?5",
                    gap: "original basis dependency ordinal gap",
                    decode: |row, _| {
                        let id: i64 = row.get(0)?;
                        let ordinal: i64 = row.get(1)?;
                        Ok(ChildRow {
                            id,
                            blob: Vec::new(),
                            ordinal,
                            bytes: row_bytes(row, 1)?,
                            row: OriginalRow::BasisLookupDependency {
                                basis_id: id,
                                ordinal,
                                key: row.get(2)?,
                            },
                        })
                    },
                }],
                false,
                3,
            )
        }
        3 => {
            let want = limit;
            let rows = query_rows(
                db,
                "SELECT p.provenance_id,p.content_hash,p.evidence_kind,p.freshness,p.basis_id,p.capture_id,p.derived_from_provenance_id,p.derived_from_record_kind,p.derived_from_fact_digest,EXISTS(SELECT 1 FROM semantic_raw_proofs r WHERE r.capture_id=p.capture_id AND r.proof_id=p.derived_from_provenance_id AND r.record_kind=p.derived_from_record_kind AND r.fact_digest=p.derived_from_fact_digest AND r.language=?3 AND r.path=?4) FROM semantic_provenance p WHERE p.tuple_id=?1 AND p.provenance_id>?2 ORDER BY p.provenance_id LIMIT ?5",
                params![
                    w.tuple_id,
                    c.after_text,
                    w.document.language,
                    w.document.path,
                    want
                ],
                |row| {
                    // The membership column is an integer: it adds nothing to the size.
                    let bytes = row_bytes(row, 0)?;
                    let capture: Option<i64> = row.get(5)?;
                    let raw: Option<String> = row.get(6)?;
                    let kind: Option<String> = row.get(7)?;
                    let digest: Option<String> = row.get(8)?;
                    let (Some(capture_id), Some(raw), Some(kind), Some(digest)) =
                        (capture, raw, kind, digest)
                    else {
                        return Err(corrupt("original proof missing derivation"));
                    };
                    selected_ensure!(
                        capture_id == a.capture_id && row.get_ref(1)?.as_str()? == w.source_hash,
                        "original provenance source mismatch"
                    );
                    let membership: bool = row.get(9)?;
                    selected_ensure!(membership, "original derivation identity mismatch");
                    Ok((
                        bytes,
                        OriginalProvenance {
                            id: row.get(0)?,
                            content_hash: row.get(1)?,
                            evidence_kind: semantic_enum(row.get(2)?)?,
                            freshness: semantic_enum(row.get(3)?)?,
                            basis_id: row.get(4)?,
                            capture_id,
                            derived_from: sem::DerivedFrom {
                                provenance_id: raw,
                                record_kind: semantic_enum(kind)?,
                                fact_digest: digest,
                            },
                        },
                    ))
                },
            )?;
            let fetched = rows.len();
            for (bytes, proof) in rows {
                let id = proof.id.clone();
                emit.push(OriginalRow::Provenance(proof), bytes)?;
                c.after_text = id;
            }
            if fetched < room {
                next_parent(c, 4);
            }
            Ok(())
        }
        4 => {
            let parents = if c.parent != 0 {
                Vec::new()
            } else {
                let mut prior = c.after_id;
                query_rows(
                    db,
                    "SELECT symbol_id,ordinal,key_scheme,key_symbol,key_scope,key_document_language,key_document_path,display_name,provenance_id FROM semantic_symbols WHERE tuple_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT ?3",
                    params![w.tuple_id, c.after_id, limit],
                    |row| {
                        let ordinal: i64 = row.get(1)?;
                        selected_ensure!(ordinal == prior + 1, "original symbol ordinal gap");
                        prior = ordinal;
                        let symbol = OriginalSymbol {
                            row_id: row.get(0)?,
                            ordinal,
                            key: semantic_symbol_key_read(
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                                row.get(5)?,
                                row.get(6)?,
                                source_set,
                            )?,
                            display_name: row.get(7)?,
                            provenance_id: row.get(8)?,
                        };
                        Ok(ParentRow {
                            id: symbol.row_id,
                            blob: Vec::new(),
                            order: ordinal,
                            bytes: row_bytes(row, 0)?,
                            row: OriginalRow::Symbol(symbol),
                        })
                    },
                )?
            };
            family(
                db,
                w.tuple_id,
                source_set,
                c,
                &mut emit,
                parents,
                &[ChildTable {
                    sql: "SELECT t.* FROM semantic_symbols s JOIN semantic_symbol_targets t ON t.symbol_id=s.symbol_id WHERE s.tuple_id=?1 AND s.ordinal>=?2 AND s.ordinal<=?3 AND (s.ordinal>?2 OR t.ordinal>?4) ORDER BY s.ordinal,t.ordinal LIMIT ?5",
                    gap: "original symbol target ordinal gap",
                    decode: |row, source_set| {
                        let id: i64 = row.get(0)?;
                        let ordinal: i64 = row.get(1)?;
                        let target = semantic_target_read(row, 2, source_set)?
                            .ok_or_else(|| corrupt("missing symbol target"))?;
                        Ok(ChildRow {
                            id,
                            blob: Vec::new(),
                            ordinal,
                            bytes: row_bytes(row, 0)?,
                            row: OriginalRow::SymbolTarget {
                                row_id: id,
                                ordinal,
                                target,
                            },
                        })
                    },
                }],
                false,
                5,
            )
        }
        5 => {
            let parents = if c.parent != 0 {
                Vec::new()
            } else {
                let mut prior = c.after_id;
                query_rows(
                    db,
                    "SELECT binding_id,ordinal,syntax_id,provenance_id,join_document_language,join_document_path,join_native_revision_id,join_content_hash,join_start,join_end,join_kind,join_status,join_diagnostic FROM semantic_declaration_bindings WHERE tuple_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT ?3",
                    params![w.tuple_id, c.after_id, limit],
                    |row| {
                        let ordinal: i64 = row.get(1)?;
                        selected_ensure!(ordinal == prior + 1, "original declaration ordinal gap");
                        prior = ordinal;
                        let binding = OriginalDeclaration {
                            row_id: row.get(0)?,
                            ordinal,
                            syntax_id: optional_id(row, 2, false)?,
                            provenance_id: row.get(3)?,
                            join: read_join(row, 4, source_set)?,
                        };
                        Ok(ParentRow {
                            id: binding.row_id,
                            blob: Vec::new(),
                            order: ordinal,
                            bytes: row_bytes(row, 0)?,
                            row: OriginalRow::DeclarationBinding(binding),
                        })
                    },
                )?
            };
            family(
                db,
                w.tuple_id,
                source_set,
                c,
                &mut emit,
                parents,
                &declaration_children(),
                false,
                6,
            )
        }
        6 => {
            let parents = if !c.parent_blob.is_empty() {
                Vec::new()
            } else {
                query_rows(
                    db,
                    "SELECT * FROM semantic_references WHERE tuple_id=?1 AND reference_id>?2 ORDER BY reference_id LIMIT ?3",
                    params![w.tuple_id, c.after_blob, limit],
                    |row| {
                        let start: i64 = row.get(4)?;
                        let end: i64 = row.get(5)?;
                        selected_ensure!(
                            start >= 0 && end >= start,
                            "invalid original reference range"
                        );
                        let reference = OriginalReference {
                            id: decoded_id(row, 1, true)?,
                            ordinal: row.get(2)?,
                            owner_syntax_id: decoded_id(row, 3, false)?,
                            range: Range {
                                start: start as usize,
                                end: end as usize,
                            },
                            spelling: row.get(6)?,
                            lookup_key: row.get(7)?,
                            site: semantic_enum(row.get(8)?)?,
                            resolution: semantic_enum(row.get(9)?)?,
                            declared_target: semantic_target_read(row, 10, source_set)?,
                            provenance_id: row.get(20)?,
                        };
                        Ok(ParentRow {
                            id: 0,
                            blob: row.get(1)?,
                            order: 0,
                            bytes: row_bytes(row, 0)?,
                            row: OriginalRow::Reference(reference),
                        })
                    },
                )?
            };
            family(
                db,
                w.tuple_id,
                source_set,
                c,
                &mut emit,
                parents,
                &[
                    ChildTable {
                        sql: "SELECT reference_id,ordinal,role FROM semantic_reference_roles WHERE tuple_id=?1 AND reference_id>=?2 AND reference_id<=?3 AND (reference_id>?2 OR ordinal>?4) ORDER BY reference_id,ordinal LIMIT ?5",
                        gap: "original reference role ordinal gap",
                        decode: |row, _| {
                            let blob: Vec<u8> = row.get(0)?;
                            let ordinal: i64 = row.get(1)?;
                            Ok(ChildRow {
                                row: OriginalRow::ReferenceRole {
                                    reference_id: id_codec::decode_occ(&blob)
                                        .map_err(|_| corrupt("invalid reference child ID"))?,
                                    ordinal,
                                    role: semantic_enum(row.get(2)?)?,
                                },
                                id: 0,
                                blob,
                                ordinal,
                                bytes: row_bytes(row, 1)?,
                            })
                        },
                    },
                    ChildTable {
                        sql: "SELECT * FROM semantic_reference_candidates WHERE tuple_id=?1 AND reference_id>=?2 AND reference_id<=?3 AND (reference_id>?2 OR ordinal>?4) ORDER BY reference_id,ordinal LIMIT ?5",
                        gap: "original reference candidate ordinal gap",
                        decode: |row, source_set| {
                            let blob: Vec<u8> = row.get(1)?;
                            let ordinal: i64 = row.get(2)?;
                            Ok(ChildRow {
                                row: OriginalRow::ReferenceCandidate {
                                    reference_id: id_codec::decode_occ(&blob)
                                        .map_err(|_| corrupt("invalid reference child ID"))?,
                                    ordinal,
                                    target: semantic_target_read(row, 3, source_set)?
                                        .ok_or_else(|| corrupt("missing reference candidate"))?,
                                },
                                id: 0,
                                blob,
                                ordinal,
                                bytes: row_bytes(row, 0)?,
                            })
                        },
                    },
                ],
                true,
                7,
            )
        }
        7 => {
            let parents = if c.parent != 0 {
                Vec::new()
            } else {
                let mut prior = c.after_id;
                query_rows(
                    db,
                    "SELECT * FROM semantic_call_bindings WHERE tuple_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT ?3",
                    params![w.tuple_id, c.after_id, limit],
                    |row| {
                        let ordinal: i64 = row.get(2)?;
                        selected_ensure!(ordinal == prior + 1, "original call ordinal gap");
                        prior = ordinal;
                        let binding = OriginalCall {
                            row_id: row.get(0)?,
                            ordinal,
                            call_id: optional_id(row, 3, true)?,
                            join: read_join(row, 4, source_set)?,
                            resolution: semantic_enum(row.get(13)?)?,
                            declared_target: semantic_target_read(row, 14, source_set)?,
                            dispatch: semantic_enum(row.get(24)?)?,
                            possible_dispatch_complete: row.get(25)?,
                            stale_target: row.get(26)?,
                            provenance_id: row.get(27)?,
                        };
                        Ok(ParentRow {
                            id: binding.row_id,
                            blob: Vec::new(),
                            order: ordinal,
                            bytes: row_bytes(row, 0)?,
                            row: OriginalRow::CallBinding(binding),
                        })
                    },
                )?
            };
            fn call_target(
                row: &rusqlite::Row<'_>,
                source_set: &str,
                dispatch: bool,
            ) -> Result<ChildRow> {
                let id: i64 = row.get(0)?;
                let ordinal: i64 = row.get(1)?;
                let target = semantic_target_read(row, 2, source_set)?
                    .ok_or_else(|| corrupt("missing call target"))?;
                Ok(ChildRow {
                    id,
                    blob: Vec::new(),
                    ordinal,
                    bytes: row_bytes(row, 0)?,
                    row: if dispatch {
                        OriginalRow::CallDispatch {
                            row_id: id,
                            ordinal,
                            target,
                        }
                    } else {
                        OriginalRow::CallCandidate {
                            row_id: id,
                            ordinal,
                            target,
                        }
                    },
                })
            }
            family(
                db,
                w.tuple_id,
                source_set,
                c,
                &mut emit,
                parents,
                &[
                    ChildTable {
                        sql: "SELECT x.binding_id,x.ordinal,x.candidate_id FROM semantic_call_bindings b JOIN semantic_call_join_candidates x ON x.binding_id=b.binding_id WHERE b.tuple_id=?1 AND b.ordinal>=?2 AND b.ordinal<=?3 AND (b.ordinal>?2 OR x.ordinal>?4) ORDER BY b.ordinal,x.ordinal LIMIT ?5",
                        gap: "original call join candidate ordinal gap",
                        decode: |row, _| {
                            let id: i64 = row.get(0)?;
                            let ordinal: i64 = row.get(1)?;
                            Ok(ChildRow {
                                id,
                                blob: Vec::new(),
                                ordinal,
                                bytes: row_bytes(row, 1)?,
                                row: OriginalRow::CallJoinCandidate {
                                    row_id: id,
                                    ordinal,
                                    id: decoded_id(row, 2, true)?,
                                },
                            })
                        },
                    },
                    ChildTable {
                        sql: "SELECT x.* FROM semantic_call_bindings b JOIN semantic_call_candidates x ON x.binding_id=b.binding_id WHERE b.tuple_id=?1 AND b.ordinal>=?2 AND b.ordinal<=?3 AND (b.ordinal>?2 OR x.ordinal>?4) ORDER BY b.ordinal,x.ordinal LIMIT ?5",
                        gap: "original call target ordinal gap",
                        decode: |row, source_set| call_target(row, source_set, false),
                    },
                    ChildTable {
                        sql: "SELECT x.* FROM semantic_call_bindings b JOIN semantic_call_possible_dispatch x ON x.binding_id=b.binding_id WHERE b.tuple_id=?1 AND b.ordinal>=?2 AND b.ordinal<=?3 AND (b.ordinal>?2 OR x.ordinal>?4) ORDER BY b.ordinal,x.ordinal LIMIT ?5",
                        gap: "original call target ordinal gap",
                        decode: |row, source_set| call_target(row, source_set, true),
                    },
                ],
                false,
                8,
            )
        }
        8 => {
            let mut prior = c.after_id;
            let rows = query_rows(
                db,
                "SELECT * FROM semantic_type_relationships WHERE tuple_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT ?3",
                params![w.tuple_id, c.after_id, limit],
                |row| {
                    let ordinal: i64 = row.get(2)?;
                    selected_ensure!(ordinal == prior + 1, "original relationship ordinal gap");
                    prior = ordinal;
                    Ok((
                        row_bytes(row, 0)?,
                        ordinal,
                        sem::TypeRelationship {
                            kind: semantic_enum(row.get(3)?)?,
                            source: semantic_target_read(row, 4, source_set)?
                                .ok_or_else(|| corrupt("missing relationship source"))?,
                            target: semantic_target_read(row, 14, source_set)?
                                .ok_or_else(|| corrupt("missing relationship target"))?,
                            provenance_id: row.get(24)?,
                        },
                    ))
                },
            )?;
            let fetched = rows.len();
            for (bytes, ordinal, row) in rows {
                emit.push(OriginalRow::TypeRelationship { ordinal, row }, bytes)?;
                c.after_id = ordinal;
            }
            if fetched < room {
                next_parent(c, 10);
            }
            Ok(())
        }
        _ => Err(corrupt("invalid original association cursor")),
    }
}

/// The child tables of a declaration binding: its symbols, then its join candidates.
fn declaration_children() -> [ChildTable; 2] {
    [
        ChildTable {
            sql: "SELECT x.binding_id,x.ordinal,x.scheme,x.symbol,x.scope,x.document_language,x.document_path FROM semantic_declaration_bindings b JOIN semantic_declaration_binding_symbols x ON x.binding_id=b.binding_id WHERE b.tuple_id=?1 AND b.ordinal>=?2 AND b.ordinal<=?3 AND (b.ordinal>?2 OR x.ordinal>?4) ORDER BY b.ordinal,x.ordinal LIMIT ?5",
            gap: "original declaration symbol ordinal gap",
            decode: |row, source_set| {
                let id: i64 = row.get(0)?;
                let ordinal: i64 = row.get(1)?;
                Ok(ChildRow {
                    id,
                    blob: Vec::new(),
                    ordinal,
                    bytes: row_bytes(row, 1)?,
                    row: OriginalRow::DeclarationSymbol {
                        row_id: id,
                        ordinal,
                        key: semantic_symbol_key_read(
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            source_set,
                        )?,
                    },
                })
            },
        },
        ChildTable {
            sql: "SELECT x.binding_id,x.ordinal,x.candidate_id FROM semantic_declaration_bindings b JOIN semantic_declaration_join_candidates x ON x.binding_id=b.binding_id WHERE b.tuple_id=?1 AND b.ordinal>=?2 AND b.ordinal<=?3 AND (b.ordinal>?2 OR x.ordinal>?4) ORDER BY b.ordinal,x.ordinal LIMIT ?5",
            gap: "original declaration candidate ordinal gap",
            decode: |row, _| {
                let id: i64 = row.get(0)?;
                let ordinal: i64 = row.get(1)?;
                Ok(ChildRow {
                    id,
                    blob: Vec::new(),
                    ordinal,
                    bytes: row_bytes(row, 1)?,
                    row: OriginalRow::DeclarationCandidate {
                        row_id: id,
                        ordinal,
                        id: decoded_id(row, 2, false)?,
                    },
                })
            },
        },
    ]
}

/// Emit a page of one parent family: each parent, then each child table's rows for it. A
/// page may end inside a parent's children; the cursor then resumes there (`c.parent` or
/// `c.parent_blob` set, `c.child_phase` the table, `c.child_ordinal` its last ordinal).
#[allow(clippy::too_many_arguments)]
fn family(
    db: &Connection,
    tuple: i64,
    source_set: &str,
    c: &mut OriginalAssociationCursor,
    emit: &mut Emit<'_, '_>,
    parents: Vec<ParentRow>,
    tables: &[ChildTable],
    by_blob: bool,
    next_phase: u8,
) -> Result<()> {
    let resuming = if by_blob {
        !c.parent_blob.is_empty()
    } else {
        c.parent != 0
    };
    let fetched_parents = parents.len();
    // The parents' order range; when resuming, only the open parent's later children.
    let (low_int, high_int, low_blob, high_blob) = if resuming {
        (
            c.after_id,
            c.after_id,
            c.after_blob.clone(),
            c.after_blob.clone(),
        )
    } else {
        match parents.last() {
            None => {
                next_parent(c, next_phase);
                return Ok(());
            }
            Some(last) => (
                c.after_id,
                last.order,
                c.after_blob.clone(),
                last.blob.clone(),
            ),
        }
    };
    let want = i64::try_from(emit.room - emit.emitted)?;
    let mut children = Vec::with_capacity(tables.len());
    for (index, table) in tables.iter().enumerate() {
        let rows = if resuming && index < c.child_phase as usize {
            Vec::new()
        } else {
            let after_child = if resuming && index == c.child_phase as usize {
                c.child_ordinal
            } else if resuming {
                -1
            } else {
                i64::MAX
            };
            if by_blob {
                query_rows(
                    db,
                    table.sql,
                    params![tuple, low_blob, high_blob, after_child, want],
                    |row| (table.decode)(row, source_set),
                )?
            } else {
                query_rows(
                    db,
                    table.sql,
                    params![tuple, low_int, high_int, after_child, want],
                    |row| (table.decode)(row, source_set),
                )?
            }
        };
        children.push(rows.into_iter().peekable());
    }
    let same = |child: &ChildRow, id: i64, blob: &[u8]| {
        if by_blob {
            child.blob == blob
        } else {
            child.id == id
        }
    };
    // Children of the open parent, from its current table on.
    let mut drain = |c: &mut OriginalAssociationCursor,
                     emit: &mut Emit<'_, '_>,
                     id: i64,
                     blob: &[u8]|
     -> Result<bool> {
        for (index, rows) in children.iter_mut().enumerate() {
            if index < c.child_phase as usize {
                continue;
            }
            if index > c.child_phase as usize {
                c.child_phase = index as u8;
                c.child_ordinal = -1;
            }
            while let Some(child) = rows.next_if(|child| same(child, id, blob)) {
                if emit.full() {
                    return Ok(false);
                }
                selected_ensure!(
                    child.ordinal == c.child_ordinal + 1,
                    "{}",
                    tables[index].gap
                );
                emit.push(child.row, child.bytes)?;
                c.child_ordinal = child.ordinal;
            }
            // A full page may have cut this table's rows short: resume it, not the next.
            if emit.full() {
                return Ok(false);
            }
        }
        c.parent = 0;
        c.parent_blob.clear();
        c.child_phase = 0;
        c.child_ordinal = -1;
        Ok(true)
    };
    if resuming {
        let (id, blob) = (c.parent, c.parent_blob.clone());
        drain(c, emit, id, &blob)?;
        return Ok(());
    }
    for parent in parents {
        if emit.full() {
            return Ok(());
        }
        emit.push(parent.row, parent.bytes)?;
        if by_blob {
            c.after_blob = parent.blob.clone();
            c.parent_blob = parent.blob.clone();
        } else {
            c.after_id = parent.order;
            c.parent = parent.id;
        }
        c.child_phase = 0;
        c.child_ordinal = -1;
        if !drain(c, emit, parent.id, &parent.blob)? {
            return Ok(());
        }
    }
    if fetched_parents < emit.room {
        // Every parent fit with its children, so the family has no more parents.
        next_parent(c, next_phase);
    }
    Ok(())
}

fn corrupt(message: &str) -> anyhow::Error {
    SelectedIntegrity(format!("incompatible_index: {message}")).into()
}
fn row_reserve(row: &rusqlite::Row<'_>, reserve: &mut Reserve<'_>) -> Result<()> {
    reserve(row_bytes(row, 0)?)
}
fn guard(db: &Connection, authority: &SourceAuthority) -> Result<()> {
    selected_ensure!(
        pending_target(db)?.as_ref() == Some(&authority.target),
        "witness target changed"
    );
    let selected: bool = db.prepare_cached("SELECT EXISTS(SELECT 1 FROM semantic_revision_raw_selections WHERE revision_id=?1 AND producer_id=?2 AND batch_id=?3 AND captured_pin=?4)")?.query_row(
        params![semantic_pin_key(authority.target.pin), authority.producer.id, authority.batch_id, semantic_pin_key(authority.captured_pin)], |r|r.get(0))?;
    selected_ensure!(selected, "witness raw selection changed");
    Ok(())
}

fn original_guard(db: &Connection, authority: &SourceAuthority) -> Result<()> {
    guard(db, authority)?;
    if authority.cache {
        selected_ensure!(
            crate::store::captured_witness::exists(db, authority.batch_id)?,
            "original capture witness unavailable"
        );
        return Ok(());
    }
    let available:bool=db.prepare_cached("SELECT NOT EXISTS(SELECT 1 FROM native_revision_supersessions WHERE revision_id=?1 AND state<>'retained') AND EXISTS(SELECT 1 FROM semantic_revision_raw_selections WHERE revision_id=?1 AND producer_id=?2 AND batch_id=?3 AND captured_pin=?4)")?.query_row(params![semantic_pin_key(authority.original_pin),authority.producer.id,authority.batch_id,semantic_pin_key(authority.captured_pin)],|r|r.get(0))?;
    selected_ensure!(available, "original witness authority unavailable");
    Ok(())
}

pub(crate) fn selected_version(
    db: &Connection,
    pin: IndexPin,
    key: &DocumentKey,
) -> Result<Option<SelectedVersion>> {
    db.prepare_cached("SELECT m.vid,v.id,v.content_hash,v.byte_length,v.extraction_context,v.producer_id,v.producer_version
         FROM revision_documents m JOIN document_versions v ON v.vid=m.vid AND v.language=m.language AND v.path=m.path
         JOIN native_revisions r ON r.id=m.revision_id
         WHERE m.revision_id=?1 AND m.language=?2 AND m.path=?3 AND r.source_set_id=?4")?.query_row(
        params![semantic_pin_key(pin),key.language,key.path,key.source_set_id], |r| {
            Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?))
        }).optional()?.map(|(vid,version_id,content_hash,length,extraction_context,producer_id,producer_version)| {
            selected_ensure!(vid>0 && crate::scip_admission::hexhash(&content_hash) && crate::scip_admission::hexhash(&extraction_context), "invalid witness version metadata");
            Ok(SelectedVersion {vid,version_id,content_hash,byte_length:length.try_into().map_err(|_|corrupt("negative document length"))?,extraction_context,producer_id,producer_version})
        }).transpose()
}

pub(crate) fn source_authority(
    db: &Connection,
    target: &PendingSemanticTarget,
    selection: &PendingSemanticSelection,
    cursor: &mut OriginalAdmissionCursor,
    limits: &BatchLimits,
    cancel: &CancelFlag,
) -> Result<AuthorityStep> {
    check_cancel(cancel)?;
    selected_ensure!(
        pending_target(db)?.as_ref() == Some(target),
        "witness target changed"
    );
    if cursor.phase == 0 {
        let length: i64 = db
            .prepare_cached(
                "SELECT length(descriptor_bytes) FROM semantic_raw_batches WHERE batch_id=?1",
            )?
            .query_row([selection.batch_id], |r| r.get(0))?;
        if length < 0 || length as u64 > limits.descriptor_bytes {
            return Ok(AuthorityStep::Full("descriptor_bound"));
        }
        let (bytes, sha): (Vec<u8>, String) = db.prepare_cached("SELECT descriptor_bytes,descriptor_sha256 FROM semantic_raw_batches WHERE batch_id=?1")?.query_row(
            [selection.batch_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let descriptor = Descriptor::parse_admitted(&bytes, &sha, limits)?;
        let pinned = descriptor.check_producer()?;
        if descriptor.trust_origin == crate::scip_admission::SYNTHETIC_TRUST_ORIGIN {
            return Ok(AuthorityStep::Full("synthetic_producer"));
        }
        if descriptor.artifacts.len() != 1 {
            return Ok(AuthorityStep::Full("multi_artifact_batch"));
        }
        if descriptor.trust_origin != crate::scip_admission::REFERENCE_TRUST_ORIGIN {
            return Ok(AuthorityStep::Full("unsupported_witness_profile"));
        }
        descriptor.check_pages(limits)?;
        descriptor.check_artifacts(limits)?;
        let captured = Store::read_native_header(
            db,
            &ReadRevision::new(
                selection.captured_pin,
                semantic_pin_key(selection.captured_pin),
            ),
        )?;
        let current = Store::read_native_header(
            db,
            &ReadRevision::new(target.pin, semantic_pin_key(target.pin)),
        )?;
        // Project-root admission already ran; bind all immutable source/header components again.
        descriptor.bind_header(
            std::slice::from_ref(&descriptor.metadata.project_root),
            &captured.source_set,
            &captured.revision,
        )?;
        // As the pre-native-first witness did, config (ignore inventory, capture options) is
        // not a witness guard: unchanged documents keep their exact version identity, which
        // binds extraction context, and every target is verified selectively against N.
        if captured.producer != current.producer
            || captured.source_set != current.source_set
            || captured.revision.toolchain_hash != current.revision.toolchain_hash
            || captured.revision.dependency_hash != current.revision.dependency_hash
        {
            return Ok(AuthorityStep::Full("destination_witness_unavailable"));
        }
        // Changed capture options (file-size cap, SCIP or manifest path) are a context
        // invalidation: FULL by rule, as the pre-native-first route was, not a witness fallback.
        let same_options: bool = db.prepare_cached("SELECT c.reconcile_options IS n.reconcile_options FROM native_revisions c JOIN native_revisions n WHERE c.id=?1 AND n.id=?2")?.query_row(
            params![semantic_pin_key(selection.captured_pin), semantic_pin_key(target.pin)],
            |r| r.get(0),
        )?;
        if !same_options {
            return Ok(AuthorityStep::Ineligible);
        }
        let producer = pinned.producer();
        let producer_ok:bool=db.prepare_cached("SELECT EXISTS(SELECT 1 FROM semantic_revision_producers s JOIN semantic_producers p ON p.id=s.id AND p.version=s.version WHERE s.revision_id=?1 AND s.id=?2 AND p.version=?3 AND p.executable_hash=?4 AND p.kind='semantic' AND p.position_encoding=?5)")?.query_row(params![semantic_pin_key(target.pin),selection.producer_id,producer.version,producer.executable_hash,semantic_variant(&producer.position_encoding)?],|r|r.get(0))?;
        selected_ensure!(
            producer_ok && producer.id == selection.producer_id,
            "witness producer mismatch"
        );
        let metadata:bool=db.prepare_cached("SELECT producer_id=?2 AND producer_version=?3 AND profile_id=?4 AND source_set_id=?5 AND captured_revision=?6 AND source_manifest_hash=?7 AND decoder_version=?8 FROM semantic_raw_batches WHERE batch_id=?1")?.query_row(
            params![selection.batch_id,producer.id,producer.version,descriptor.profile_id,descriptor.source_set.id,descriptor.source_revision,descriptor.source_manifest_hash,descriptor.decoder_version],|r|r.get(0))?;
        selected_ensure!(metadata, "witness batch metadata mismatch");
        let entry = &descriptor.artifacts[0];
        let (capture_id,valid):(i64,bool)=db.prepare_cached("SELECT capture_id,ordinal=?2 AND language=?3 AND artifact_hash=?4 AND producer_id=?5 AND source_set_id=?6 AND captured_revision=?7 AND decoder_version=?8 AND length(binary_bytes)=?9 FROM semantic_raw_captures WHERE batch_id=?1 AND ordinal=?2")?.query_row(
            params![selection.batch_id,entry.ordinal,entry.language,entry.sha256,producer.id,descriptor.source_set.id,descriptor.source_revision,descriptor.decoder_version,i64::try_from(entry.byte_length)?],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if !valid {
            // A retained artifact that no longer matches its descriptor is the producer's
            // failure, as FULL authenticates and reports it, never index corruption.
            return Ok(AuthorityStep::Full("witness_capture_mismatch"));
        }
        let mut captures = db.prepare_cached(
            "SELECT capture_id FROM semantic_raw_captures WHERE batch_id=?1 LIMIT 2",
        )?;
        let mut captures = captures.query([selection.batch_id])?;
        selected_ensure!(
            captures.next()?.is_some() && captures.next()?.is_none(),
            "witness artifact count mismatch"
        );
        cursor.manifest = Some(SourceManifestHasher::new(&descriptor.source_set.id));
        cursor.authority = Some(SourceAuthority {
            batch_id: selection.batch_id,
            capture_id,
            artifact_hash: entry.sha256.clone(),
            captured_pin: selection.captured_pin,
            original_pin: selection.captured_pin,
            producer,
            source_set_id: descriptor.source_set.id.clone(),
            captured_revision_id: descriptor.source_revision.clone(),
            source_manifest_hash: descriptor.source_manifest_hash.clone(),
            toolchain_hash: captured.revision.toolchain_hash,
            config_hash: captured.revision.config_hash,
            dependency_hash: captured.revision.dependency_hash,
            descriptor,
            target: target.clone(),
            cache: false,
        });
        cursor.phase = 1;
        return Ok(AuthorityStep::Progress);
    }
    let authority = cursor
        .authority
        .as_ref()
        .ok_or_else(|| corrupt("completed witness cursor"))?;
    guard(db, authority)?;
    if cursor.phase == 1 {
        let row:Option<(i64,String,i64,String,String)>=db.prepare_cached("SELECT s.rowid,s.revision_id,r.published_index_revision,r.native_revision_id,r.source_set_id FROM semantic_revision_raw_selections s JOIN native_revisions r ON r.id=s.revision_id WHERE s.batch_id=?1 AND s.rowid>?2 AND s.producer_id=?3 AND s.captured_pin=?4 ORDER BY s.rowid LIMIT 1")?.query_row(
            params![authority.batch_id,cursor.after_selection,authority.producer.id,semantic_pin_key(authority.captured_pin)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        if let Some((rowid, key, revision, native, sources)) = row {
            cursor.after_selection = rowid;
            if native == authority.captured_revision_id && sources == authority.source_set_id {
                let pin = parse_pin(&key)?;
                let revision: u64 = revision
                    .try_into()
                    .map_err(|_| corrupt("negative original revision"))?;
                if cursor
                    .original
                    .as_ref()
                    .is_none_or(|(old, _)| revision < *old)
                {
                    cursor.original = Some((revision, pin));
                }
            }
            return Ok(AuthorityStep::Progress);
        }
        let Some((_, pin)) = cursor.original else {
            // An import that landed below its capture has no original import; a complete
            // capture witness, published by an earlier FULL catch-up, stands in for it.
            if !crate::store::captured_witness::exists(db, authority.batch_id)? {
                return Ok(AuthorityStep::Full("original_import_missing"));
            }
            if context::controlled_unchanged(
                db,
                &semantic_pin_key(target.pin),
                &semantic_pin_key(target.pin),
                &authority.producer.id,
                cancel,
            )? == Some(false)
            {
                return Ok(AuthorityStep::Full("controlled_profile_inputs_changed"));
            }
            let authority = cursor.authority.as_mut().unwrap();
            authority.cache = true;
            authority.original_pin = authority.captured_pin;
            cursor.phase = 2;
            return Ok(AuthorityStep::Progress);
        };
        let releasing:bool=db.prepare_cached("SELECT EXISTS(SELECT 1 FROM native_revision_supersessions WHERE revision_id=?1 AND state<>'retained')")?.query_row([semantic_pin_key(pin)],|r|r.get(0))?;
        if releasing {
            return Ok(AuthorityStep::Full("original_import_releasing"));
        }
        // Controlled profiles need the owner's profile-input comparison, not just equal VID:
        // the effective inputs recorded at the original import must still hold at N.
        if context::controlled_unchanged(
            db,
            &semantic_pin_key(pin),
            &semantic_pin_key(target.pin),
            &authority.producer.id,
            cancel,
        )? == Some(false)
        {
            return Ok(AuthorityStep::Full("controlled_profile_inputs_changed"));
        }
        cursor.authority.as_mut().unwrap().original_pin = pin;
        cursor.phase = 2;
        return Ok(AuthorityStep::Progress);
    }
    let authority = cursor.authority.as_ref().unwrap();
    if let Some(language) = authority
        .descriptor
        .source_set
        .languages
        .get(cursor.inventory.0)
    {
        let row:Option<(String,String)>=db.prepare_cached("SELECT m.path,v.content_hash FROM revision_documents m JOIN document_versions v ON v.vid=m.vid WHERE m.revision_id=?1 AND m.language=?2 AND m.path>?3 ORDER BY m.path LIMIT 1")?.query_row(
            params![semantic_pin_key(authority.captured_pin),language,cursor.inventory.1],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((path, hash)) = row {
            cursor
                .manifest
                .as_mut()
                .unwrap()
                .push(language, &path, &hash);
            cursor.inventory.1 = path;
            cursor.source_count += 1;
            selected_ensure!(
                cursor.source_count <= authority.descriptor.totals.source_documents
                    && cursor.source_count <= limits.source_documents,
                "witness source inventory exceeds descriptor"
            );
        } else {
            cursor.inventory.0 += 1;
            cursor.inventory.1.clear();
        }
        return Ok(AuthorityStep::Progress);
    }
    selected_ensure!(
        cursor.source_count == authority.descriptor.totals.source_documents
            && cursor.manifest.take().unwrap().finish() == authority.source_manifest_hash,
        "witness source manifest mismatch"
    );
    cursor.phase = 3;
    Ok(AuthorityStep::Eligible(Box::new(
        cursor.authority.take().unwrap(),
    )))
}

fn has_nonderived_original_provenance(db: &Connection, tuple: i64) -> Result<bool> {
    db.prepare_cached("SELECT EXISTS(SELECT 1 FROM semantic_provenance WHERE tuple_id=?1 AND capture_id IS NULL AND derived_from_provenance_id IS NULL AND derived_from_record_kind IS NULL AND derived_from_fact_digest IS NULL)")?.query_row([tuple],|row|row.get(0)).map_err(Into::into)
}

pub(crate) fn original_document_begin(
    db: &Connection,
    authority: &SourceAuthority,
    target: &PendingSemanticTarget,
    key: &DocumentKey,
) -> Result<WitnessEligibility> {
    original_guard(db, authority)?;
    selected_ensure!(
        target == &authority.target && key.source_set_id == authority.source_set_id,
        "witness source identity changed"
    );
    let language: sem::Language = semantic_enum(key.language.clone())?;
    let Some(current) = selected_version(db, target.pin, key)? else {
        return Ok(WitnessEligibility::Full("destination_source_missing"));
    };
    if !authority.producer.languages.contains(&language) {
        // Another producer's language: FULL emits no coverage row or fact for it either.
        return Ok(WitnessEligibility::Uncovered(CoverageInput {
            document: key.clone(),
            current,
            requested: false,
            selected: false,
            exclusion: None,
        }));
    }
    let captured = selected_version(db, authority.captured_pin, key)?;
    let raw:Option<(String,String,i64)>=db.prepare_cached("SELECT source_hash,wire_digest,capped FROM semantic_raw_documents WHERE capture_id=?1 AND language=?2 AND path=?3")?.query_row(params![authority.capture_id,key.language,key.path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let exclusion:Option<String>=db.prepare_cached("SELECT reason FROM semantic_raw_exclusions WHERE batch_id=?1 AND language=?2 AND path=?3")?.query_row(params![authority.batch_id,key.language,key.path],|r|r.get(0)).optional()?;
    let claims:bool=db.prepare_cached("SELECT EXISTS(SELECT 1 FROM semantic_raw_proofs WHERE capture_id=?1 AND language=?2 AND path=?3)")?.query_row(params![authority.capture_id,key.language,key.path],|r|r.get(0))?;
    let Some(captured) = captured else {
        selected_ensure!(
            raw.is_none() && exclusion.is_none() && !claims,
            "raw association outside captured inventory"
        );
        return Ok(WitnessEligibility::NewDocument);
    };
    if raw.is_none() && exclusion.is_none() {
        return Ok(WitnessEligibility::Full("raw_document_missing"));
    }
    selected_ensure!(
        raw.is_none() || exclusion.is_none(),
        "raw document both admitted and excluded"
    );
    let source_hash = raw
        .as_ref()
        .map_or_else(|| captured.content_hash.clone(), |row| row.0.clone());
    selected_ensure!(
        source_hash == captured.content_hash,
        "raw document source mismatch"
    );
    if let Some((_, digest, capped)) = &raw {
        selected_ensure!(
            crate::scip_admission::hexhash(digest) && (0..=3).contains(capped),
            "invalid raw document metadata"
        );
    }
    let requested = exclusion.as_deref() != Some("notRequested");
    if source_hash != current.content_hash {
        return Ok(WitnessEligibility::ChangedBytes(CoverageInput {
            document: key.clone(),
            current,
            requested,
            selected: requested,
            exclusion,
        }));
    }
    if captured != current {
        return Ok(WitnessEligibility::Full("selected_version_changed"));
    }
    let (tuple_id, requested, selected, state, diagnostic, reasons, cached_authored) = if authority
        .cache
    {
        let Some(snapshot) = crate::store::captured_witness::document(db, authority.batch_id, key)?
        else {
            return Ok(WitnessEligibility::Full("original_tuple_missing"));
        };
        let [coverage] = snapshot.coverage.as_slice() else {
            return Err(corrupt("capture witness document coverage"));
        };
        if snapshot
            .provenance
            .iter()
            .any(|p| p.derived_from.is_none() || p.basis.is_none())
        {
            return Ok(WitnessEligibility::Full("original_provenance_not_derived"));
        }
        (
            0,
            coverage.requested,
            coverage.selected,
            coverage.state,
            coverage.diagnostic.clone(),
            coverage.reasons.clone(),
            Some(!snapshot.provenance.is_empty()),
        )
    } else {
        let tuple:Option<(i64,bool,bool,String,Option<String>)>=db.prepare_cached("SELECT tuple_id,requested,selected,state,diagnostic FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND producer_version=?3 AND language=?4 AND path=?5")?.query_row(params![semantic_pin_key(authority.original_pin),authority.producer.id,authority.producer.version,key.language,key.path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let Some((tuple_id, requested, selected, state, diagnostic)) = tuple else {
            return Ok(WitnessEligibility::Full("original_tuple_missing"));
        };
        context::validate_tuple(
            db,
            &semantic_pin_key(authority.original_pin),
            &authority.producer.id,
            tuple_id,
        )?;
        let state: sem::CoverageState = semantic_enum(state)?;
        let reasons = read_coverage_reasons(db, tuple_id)?;
        if has_nonderived_original_provenance(db, tuple_id)? {
            return Ok(WitnessEligibility::Full("original_provenance_not_derived"));
        }
        (
            tuple_id, requested, selected, state, diagnostic, reasons, None,
        )
    };
    if reasons.as_ref().is_some_and(|r| r.omitted) {
        return Ok(WitnessEligibility::Full("original_associations_omitted"));
    }
    let bearing = matches!(
        state,
        sem::CoverageState::Complete | sem::CoverageState::Partial
    );
    if raw.is_some() && (!requested || !selected || !bearing) {
        return Ok(WitnessEligibility::Full("original_coverage_differs"));
    }
    if exclusion.is_some() {
        let authored: bool = match cached_authored {
            Some(authored) => authored,
            None => db
                .prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM semantic_provenance WHERE tuple_id=?1)",
                )?
                .query_row([tuple_id], |r| r.get(0))?,
        };
        if bearing || claims || authored {
            return Ok(WitnessEligibility::Full("excluded_original_has_facts"));
        }
    }
    Ok(WitnessEligibility::Witness(Box::new(
        OriginalSourceWitness {
            tuple_id,
            document: key.clone(),
            captured_version: captured,
            current_version: current,
            source_hash: source_hash.clone(),
            wire_digest: raw.as_ref().map(|r| r.1.clone()),
            capped: raw.map(|r| r.2),
            requested,
            selected,
            state,
            diagnostic,
            reasons,
            exclusion,
        },
    )))
}

pub(crate) fn current_declaration(
    db: &Connection,
    pin: IndexPin,
    key: &DocumentKey,
    syntax_id: &str,
    reserve: &mut Reserve<'_>,
) -> Result<Option<CurrentDeclaration>> {
    let Some(version) = selected_version(db, pin, key)? else {
        return Ok(None);
    };
    let encoded =
        id_codec::encode_sid(syntax_id).ok_or_else(|| corrupt("invalid targeted syntax ID"))?;
    let mut stmt=db.prepare_cached("SELECT kind,name,name_start,name_end,start_byte,end_byte FROM native_version_declarations WHERE vid=?1 AND syntax_id=?2 LIMIT 2")?;
    let mut rows = stmt.query(params![version.vid, encoded])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    row_reserve(row, reserve)?;
    let (kind, name, start, end, outer_start, outer_end): (
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        i64,
        i64,
    ) = (
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    );
    selected_ensure!(rows.next()?.is_none(), "duplicate targeted declaration");
    match (name, start, end) {
        (None, None, None) => Ok(Some(CurrentDeclaration {
            kind,
            name: None,
            name_range: None,
            callable: false,
        })),
        (Some(name), Some(start), Some(end)) => {
            selected_ensure!(
                outer_start >= 0
                    && start >= outer_start
                    && end > start
                    && outer_end >= end
                    && outer_end as u64 <= version.byte_length,
                "invalid targeted declaration span"
            );
            let callable = matches!(kind.as_str(), "function" | "method" | "constructor");
            Ok(Some(CurrentDeclaration {
                kind,
                name: Some(name),
                name_range: Some(Range {
                    start: start as usize,
                    end: end as usize,
                }),
                callable,
            }))
        }
        _ => Err(corrupt("malformed targeted declaration")),
    }
}

pub(crate) fn internal_symbols_page(
    db: &Connection,
    authority: &SourceAuthority,
    cursor: &mut InternalSymbolCursor,
    limit: usize,
    reserve: &mut Reserve<'_>,
) -> Result<Vec<sem::SymbolKey>> {
    original_guard(db, authority)?;
    ensure!(limit > 0, "witness page limit must be positive");
    let mut result = Vec::new();
    if cursor.done {
        return Ok(result);
    }
    if !cursor.started {
        cursor.binding_ordinal = -1;
        cursor.symbol_ordinal = -1;
        cursor.started = true;
    }
    if authority.cache {
        // One capture-witness document per call, keyed by its rowid.
        let next: Option<(i64, String)> = db
            .prepare_cached("SELECT rowid,snapshot_json FROM semantic_batch_witness_documents WHERE batch_id=?1 AND rowid>?2 ORDER BY rowid LIMIT 1")?.query_row(
                params![authority.batch_id, cursor.tuple],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((rowid, json)) = next else {
            cursor.done = true;
            return Ok(result);
        };
        reserve(json.len() as u64)?;
        cursor.tuple = rowid;
        let snapshot: sem::SemanticSnapshot = serde_json::from_str(&json).context(
            SelectedIntegrity("incompatible_index: corrupt captured witness snapshot".into()),
        )?;
        for binding in &snapshot.declaration_bindings {
            if binding.syntax_id.is_some() && binding.join.status == sem::JoinStatus::Exact {
                result.extend(binding.symbols.iter().cloned());
            }
        }
        return Ok(result);
    }
    // Each call advances at most `limit` rows/transitions, including empty parents.
    for _ in 0..limit {
        if let Some(binding) = cursor.binding {
            let mut stmt=db.prepare_cached("SELECT ordinal,scheme,symbol,scope,document_language,document_path FROM semantic_declaration_binding_symbols WHERE binding_id=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1")?;
            let mut rows = stmt.query(params![binding, cursor.symbol_ordinal])?;
            if let Some(row) = rows.next()? {
                row_reserve(row, reserve)?;
                let ordinal: i64 = row.get(0)?;
                selected_ensure!(
                    ordinal == cursor.symbol_ordinal + 1,
                    "original scope symbol ordinal gap"
                );
                cursor.symbol_ordinal = ordinal;
                reserve(authority.source_set_id.len() as u64)?;
                result.push(semantic_symbol_key_read(
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    &authority.source_set_id,
                )?);
            } else {
                cursor.binding = None;
                cursor.symbol_ordinal = -1;
            }
        } else {
            let row:Option<(i64,i64)>=db.prepare_cached("SELECT binding_id,ordinal FROM semantic_declaration_bindings WHERE tuple_id=?1 AND ordinal>?2 AND syntax_id IS NOT NULL AND join_status='exact' ORDER BY ordinal LIMIT 1")?.query_row(params![cursor.tuple,cursor.binding_ordinal],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((id, ordinal)) = row {
                cursor.binding = Some(id);
                cursor.binding_ordinal = ordinal;
            } else {
                let next:Option<i64>=db.prepare_cached("SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND tuple_id>?3 ORDER BY tuple_id LIMIT 1")?.query_row(params![semantic_pin_key(authority.original_pin),authority.producer.id,cursor.tuple],|r|r.get(0)).optional()?;
                if let Some(tuple) = next {
                    cursor.tuple = tuple;
                    cursor.binding_ordinal = -1;
                } else {
                    cursor.done = true;
                    break;
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version_fixture() -> (Connection, IndexPin, DocumentKey) {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE native_revisions(id TEXT PRIMARY KEY,source_set_id TEXT);
            CREATE TABLE revision_documents(revision_id TEXT,language TEXT,path TEXT,vid INTEGER,PRIMARY KEY(revision_id,language,path));
            CREATE TABLE document_versions(vid INTEGER PRIMARY KEY,id TEXT,language TEXT,path TEXT,content_hash TEXT,byte_length INTEGER,extraction_context TEXT,producer_id TEXT,producer_version TEXT);
            CREATE TABLE native_version_declarations(vid INTEGER,syntax_id BLOB,kind TEXT,name TEXT,name_start INTEGER,name_end INTEGER,start_byte INTEGER,end_byte INTEGER,PRIMARY KEY(vid,syntax_id));").unwrap();
        let pin = IndexPin {
            index_generation: uuid::Uuid::new_v4(),
            index_revision: 1,
        };
        let key = DocumentKey {
            source_set_id: "sources".into(),
            language: "javascript".into(),
            path: "a.js".into(),
        };
        db.execute(
            "INSERT INTO native_revisions VALUES(?1,'sources')",
            [semantic_pin_key(pin)],
        )
        .unwrap();
        db.execute("INSERT INTO document_versions VALUES(7,'version','javascript','a.js',?1,40,?2,'native','1')",params!["a".repeat(64),"b".repeat(64)]).unwrap();
        db.execute(
            "INSERT INTO revision_documents VALUES(?1,'javascript','a.js',7)",
            [semantic_pin_key(pin)],
        )
        .unwrap();
        (db, pin, key)
    }

    #[test]
    fn nonderived_original_provenance_requires_full_without_hiding_mixed_links() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE semantic_provenance(tuple_id INTEGER,capture_id INTEGER,derived_from_provenance_id TEXT,derived_from_record_kind TEXT,derived_from_fact_digest TEXT);
            INSERT INTO semantic_provenance VALUES(1,NULL,NULL,NULL,NULL),(2,9,'proof','occurrence','digest'),(3,9,NULL,NULL,NULL),(4,NULL,'proof',NULL,NULL);").unwrap();
        assert!(has_nonderived_original_provenance(&db, 1).unwrap());
        for tuple in [2, 3, 4, 5] {
            assert!(!has_nonderived_original_provenance(&db, tuple).unwrap());
        }
    }

    #[test]
    fn exact_version_link_and_target_absence_are_metadata_only() {
        let (db, pin, key) = version_fixture();
        let selected = selected_version(&db, pin, &key).unwrap().unwrap();
        assert_eq!(selected.vid, 7);
        assert_eq!(selected.extraction_context, "b".repeat(64));
        let sid = "sid:v1:11111111111111111111111111111111";
        let encoded = id_codec::encode_sid(sid).unwrap();
        assert!(
            current_declaration(&db, pin, &key, sid, &mut |_| Ok(()))
                .unwrap()
                .is_none()
        );
        db.execute(
            "INSERT INTO native_version_declarations VALUES(7,?1,'function',NULL,NULL,NULL,0,30)",
            [&encoded],
        )
        .unwrap();
        let mut refused =
            |_: u64| -> Result<()> { anyhow::bail!("target row reservation refused") };
        assert!(current_declaration(&db, pin, &key, sid, &mut refused).is_err());
        let unnamed = current_declaration(&db, pin, &key, sid, &mut |_| Ok(()))
            .unwrap()
            .unwrap();
        assert!(unnamed.name.is_none());
        db.execute("UPDATE native_version_declarations SET name='entry',name_start=9,name_end=14 WHERE vid=7",[]).unwrap();
        let named = current_declaration(&db, pin, &key, sid, &mut |_| Ok(()))
            .unwrap()
            .unwrap();
        assert_eq!(named.name.as_deref(), Some("entry"));
        assert_eq!(named.name_range, Some(Range { start: 9, end: 14 }));
        db.execute("UPDATE native_version_declarations SET name_end=41", [])
            .unwrap();
        assert!(
            current_declaration(&db, pin, &key, sid, &mut |_| Ok(()))
                .unwrap_err()
                .is::<SelectedIntegrity>()
        );
        let mut other = key;
        other.source_set_id = "other".into();
        assert!(selected_version(&db, pin, &other).unwrap().is_none());
    }

    #[test]
    fn declaration_families_page_by_set_and_resume_inside_children() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE semantic_declaration_bindings(binding_id INTEGER PRIMARY KEY,tuple_id INTEGER,ordinal INTEGER,UNIQUE(tuple_id,ordinal));
            CREATE TABLE semantic_declaration_binding_symbols(binding_id INTEGER,ordinal INTEGER,scheme TEXT,symbol TEXT,scope TEXT,document_language TEXT,document_path TEXT,PRIMARY KEY(binding_id,ordinal));
            CREATE TABLE semantic_declaration_join_candidates(binding_id INTEGER,ordinal INTEGER,candidate_id BLOB,PRIMARY KEY(binding_id,ordinal));
            INSERT INTO semantic_declaration_bindings VALUES(1,7,0),(2,7,1),(3,8,0);
            INSERT INTO semantic_declaration_binding_symbols VALUES(1,0,'scip','first','global',NULL,NULL),(1,1,'scip','second','document','javascript','a.js'),(2,0,'scip','third','global',NULL,NULL),(3,0,'scip','other tuple','global',NULL,NULL);").unwrap();
        let candidate = id_codec::encode_sid("sid:v1:11111111111111111111111111111111").unwrap();
        db.execute(
            "INSERT INTO semantic_declaration_join_candidates VALUES(1,0,?1)",
            [&candidate],
        )
        .unwrap();
        // The family code never inspects a parent's row; a marker row names each parent.
        let parent = |id: i64, order: i64| ParentRow {
            id,
            blob: Vec::new(),
            order,
            bytes: 1,
            row: OriginalRow::BasisLookupDependency {
                basis_id: id,
                ordinal: -1,
                key: "parent".into(),
            },
        };
        let label = |row: &OriginalRow| match row {
            OriginalRow::BasisLookupDependency { basis_id, .. } => format!("P{basis_id}"),
            OriginalRow::DeclarationSymbol {
                row_id, ordinal, ..
            } => format!("s{row_id}.{ordinal}"),
            OriginalRow::DeclarationCandidate {
                row_id, ordinal, ..
            } => format!("c{row_id}.{ordinal}"),
            _ => panic!("unexpected row"),
        };
        let page = |cursor: &mut OriginalAssociationCursor,
                    parents: Vec<ParentRow>,
                    room: usize,
                    reserve: &mut Reserve<'_>|
         -> Result<Vec<String>> {
            let mut out = Vec::new();
            let mut emit = Emit {
                out: &mut out,
                room,
                emitted: 0,
                unit: 0,
                reserve,
            };
            let tables = declaration_children();
            family(
                &db, 7, "sources", cursor, &mut emit, parents, &tables, false, 6,
            )?;
            Ok(out.iter().map(label).collect())
        };
        let fresh = || OriginalAssociationCursor {
            phase: 5,
            after_id: -1,
            child_ordinal: -1,
            ..Default::default()
        };
        let owned = std::cell::Cell::new(0u64);
        let mut reserve = |bytes: u64| -> Result<()> {
            owned.set(owned.get() + bytes);
            Ok(())
        };

        // A whole family in one page: each parent, then its symbols, then its candidates.
        let mut cursor = fresh();
        let rows = page(
            &mut cursor,
            vec![parent(1, 0), parent(2, 1)],
            16,
            &mut reserve,
        );
        assert_eq!(rows.unwrap(), ["P1", "s1.0", "s1.1", "c1.0", "P2", "s2.0"]);
        assert_eq!((cursor.phase, cursor.after_id, cursor.parent), (6, -1, 0));

        // A short page ends inside the first parent's symbols and resumes there.
        let mut cursor = fresh();
        let rows = page(
            &mut cursor,
            vec![parent(1, 0), parent(2, 1)],
            2,
            &mut reserve,
        );
        assert_eq!(rows.unwrap(), ["P1", "s1.0"]);
        let position = |c: &OriginalAssociationCursor| {
            (
                c.phase,
                c.after_id,
                c.parent,
                c.child_phase,
                c.child_ordinal,
            )
        };
        assert_eq!(position(&cursor), (5, 0, 1, 0, 0));
        // A refused reservation leaves the cursor where it was.
        let mut refused = |_: u64| -> Result<()> { anyhow::bail!("reservation refused") };
        assert!(page(&mut cursor, Vec::new(), 2, &mut refused).is_err());
        assert_eq!(position(&cursor), (5, 0, 1, 0, 0));
        let rows = page(&mut cursor, Vec::new(), 2, &mut reserve);
        assert_eq!(rows.unwrap(), ["s1.1", "c1.0"]);
        // The page filled at the last candidate: that table resumes, finds none, and the
        // parent closes; the next parent page starts after it.
        assert_eq!(position(&cursor), (5, 0, 1, 1, 0));
        assert!(
            page(&mut cursor, Vec::new(), 2, &mut reserve)
                .unwrap()
                .is_empty()
        );
        assert_eq!(position(&cursor), (5, 0, 0, 0, -1));
        let rows = page(&mut cursor, vec![parent(2, 1)], 2, &mut reserve);
        assert_eq!(rows.unwrap(), ["P2", "s2.0"]);
        assert!(owned.get() > 0);

        // A missing child ordinal is corruption, not a shorter family.
        db.execute(
            "DELETE FROM semantic_declaration_binding_symbols WHERE binding_id=1 AND ordinal=0",
            [],
        )
        .unwrap();
        let mut cursor = fresh();
        let error = page(&mut cursor, vec![parent(1, 0)], 16, &mut reserve).unwrap_err();
        assert!(error.is::<SelectedIntegrity>(), "{error:#}");
    }
}
